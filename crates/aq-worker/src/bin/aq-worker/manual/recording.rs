//! Private, restartable terminal transcript spool. The sandbox pipe is independent
//! of terminal attachments; only normalized text leaves the worker.
use super::{cli, tmux};
use anyhow::{Result, ensure};
use aq_node_service::{
    enrollment_file::write_private,
    link::{LinkError, ServerLink},
};
use aq_worker::backend::docker_local::DockerLocalConfig;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::{Path, PathBuf};
use tokio::sync::watch;
use uuid::Uuid;

const RAW_LIMIT: u64 = 128 * 1024 * 1024;
const TEXT_LIMIT: u64 = 64 * 1024 * 1024;
const READ: u64 = 8192;

#[derive(Default, Serialize, Deserialize)]
struct Normalizer {
    escape: u8,
    utf8: Vec<u8>,
    prefix: Vec<u8>,
    masked: bool,
}
impl Normalizer {
    fn feed(&mut self, input: &[u8], final_input: bool) -> String {
        let mut clean = Vec::new();
        for &b in input {
            if self.escape != 0 {
                self.escape = match (self.escape, b) {
                    (1, b'[') => 2,
                    (1, b']' | b'P' | b'^' | b'_') => 3,
                    (1, _) => 0,
                    (2, 0x40..=0x7e) => 0,
                    (2, _) => 2,
                    (3, 7) => 0,
                    (3, 27) => 4,
                    (3, _) => 3,
                    (4, b'\\') => 0,
                    (4, _) => 3,
                    _ => 0,
                };
                continue;
            }
            if b == 27 {
                self.escape = 1;
                continue;
            }
            // Emit carriage returns as line boundaries to preserve repainted text.
            let b = if b == b'\r' { b'\n' } else { b };
            if b < 32 && b != b'\n' && b != b'\t' || b == 127 {
                continue;
            }
            if self.masked {
                if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' {
                    clean.push(b'*');
                    continue;
                }
                if b == b'\n' {
                    clean.push(b);
                    continue;
                }
                self.masked = false;
            }
            clean.push(b);
            self.prefix.push(b);
            if self.prefix.len() > 7 {
                self.prefix.remove(0);
            }
            if self.prefix.ends_with(b"aqa_") || self.prefix.ends_with(b"sk-ant-") {
                self.masked = true;
                self.prefix.clear();
            }
        }
        self.utf8.extend(clean);
        let mut out = String::new();
        loop {
            match std::str::from_utf8(&self.utf8) {
                Ok(s) => {
                    out.push_str(s);
                    self.utf8.clear();
                    break;
                }
                Err(e) => {
                    let valid = e.valid_up_to();
                    out.push_str(std::str::from_utf8(&self.utf8[..valid]).unwrap());
                    if let Some(n) = e.error_len() {
                        out.push('\u{fffd}');
                        self.utf8.drain(..valid + n);
                    } else {
                        self.utf8.drain(..valid);
                        if final_input {
                            out.push('\u{fffd}');
                            self.utf8.clear();
                        }
                        break;
                    }
                }
            }
        }
        out
    }
}
#[derive(Serialize, Deserialize)]
struct State {
    id: Uuid,
    node: Uuid,
    container: String,
    offset: u64,
    bytes: u64,
    next: u64,
    normalizer: Normalizer,
    ended: bool,
    reason: Option<String>,
}
fn dir(root: &Path, id: Uuid) -> PathBuf {
    root.join(id.to_string()).join("recording")
}
fn save(path: &Path, state: &State) -> Result<()> {
    write_private(&path.join("state.json"), &serde_json::to_vec(state)?)
}
fn chunk(path: &Path, n: u64) -> PathBuf {
    path.join(format!("chunk-{n:020}"))
}
pub(super) fn initialize(root: &Path, id: Uuid, node: Uuid, container: &str) -> Result<()> {
    let path = dir(root, id);
    if path.join("state.json").exists() {
        return Ok(());
    }
    save(
        &path,
        &State {
            id,
            node,
            container: container.into(),
            offset: 0,
            bytes: 0,
            next: 1,
            normalizer: Normalizer::default(),
            ended: false,
            reason: None,
        },
    )
}
fn add(path: &Path, state: &mut State, mut text: String) -> Result<()> {
    let left = TEXT_LIMIT.saturating_sub(state.bytes) as usize;
    if text.len() > left {
        let mut end = left.min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        state
            .reason
            .get_or_insert_with(|| "Normalized output limit reached".into());
    }
    if !text.is_empty() {
        ensure!(text.len() <= 32768, "normalized chunk too large");
        write_private(&chunk(path, state.next), text.as_bytes())?;
        state.bytes += text.len() as u64;
        state.next += 1;
    }
    Ok(())
}
async fn tick(path: &Path, state: &mut State, exe: &Path, cfg: &DockerLocalConfig) -> Result<()> {
    // A forced tmux close cannot prove that the PTY kernel buffer was empty.
    if path.join("forced-stop").exists() {
        state.reason.get_or_insert_with(|| {
            "Session was stopped before terminal output could be fully verified".into()
        });
    }
    // Read bounded blocks into durable immutable chunks before attempting HTTP.
    if !state.ended && path.join("capture-lost").exists() {
        state.reason = Some("Capture ended before the final output could be drained".into());
        state.ended = true;
        save(path, state)?;
    }
    if !state.ended {
        let status = cli(exe, &cfg.socket, &["exec", &state.container, "/bin/sh", "-c", "if [ -e /aq/out/transcript.failed ]; then echo failed; elif [ -f /aq/out/transcript.raw ]; then if [ -e /aq/out/transcript.done ]; then echo done; else echo open; fi; wc -c < /aq/out/transcript.raw; else echo waiting; fi"], None).await;
        if let Ok(status) = status {
            let status = String::from_utf8_lossy(&status);
            if status.starts_with("failed") {
                state.reason = Some("Terminal capture could not start".into());
                state.ended = true;
                let _ = cli(
                    exe,
                    &cfg.socket,
                    &[
                        "exec",
                        &state.container,
                        "/bin/sh",
                        "-c",
                        "rm -f /aq/out/transcript.raw; : > /aq/out/transcript.drained",
                    ],
                    None,
                )
                .await;
            } else if let Some(size) = status
                .lines()
                .nth(1)
                .and_then(|s| s.trim().parse::<u64>().ok())
            {
                for _ in 0..16 {
                    if state.offset >= size.min(RAW_LIMIT) {
                        break;
                    }
                    let count = READ.min(size.min(RAW_LIMIT) - state.offset);
                    let data = cli(
                        exe,
                        &cfg.socket,
                        &[
                            "exec",
                            &state.container,
                            "/bin/sh",
                            "-c",
                            &format!(
                                "tail -c +{} /aq/out/transcript.raw | head -c {count}",
                                state.offset + 1
                            ),
                        ],
                        None,
                    )
                    .await?;
                    if data.is_empty() {
                        break;
                    }
                    state.offset += data.len() as u64;
                    let text = state.normalizer.feed(&data, false);
                    add(path, state, text)?;
                    save(path, state)?;
                }
                if size >= RAW_LIMIT {
                    state.reason = Some("Raw output limit reached".into());
                }
                if status.lines().any(|line| line == "done") && state.offset >= size.min(RAW_LIMIT)
                {
                    let text = state.normalizer.feed(&[], true);
                    add(path, state, text)?;
                    if path.join("forced-stop").exists() {
                        state.reason.get_or_insert_with(|| {
                            "Session was stopped before terminal output could be fully verified"
                                .into()
                        });
                    }
                    state.ended = true;
                    save(path, state)?;
                    let _ = cli(
                        exe,
                        &cfg.socket,
                        &[
                            "exec",
                            &state.container,
                            "/bin/sh",
                            "-c",
                            "rm -f /aq/out/transcript.raw; : > /aq/out/transcript.drained",
                        ],
                        None,
                    )
                    .await;
                }
            }
        } else {
            let stopped = cli(
                exe,
                &cfg.socket,
                &[
                    "inspect",
                    "--format",
                    "{{.State.Running}}",
                    &state.container,
                ],
                None,
            )
            .await
            .is_ok_and(|v| String::from_utf8_lossy(&v).trim() == "false");
            if stopped || path.join("capture-lost").exists() {
                state.reason =
                    Some("Capture ended before the final output could be drained".into());
                state.ended = true;
            }
        }
        save(path, state)?;
    }
    if state.ended {
        // The runner must be released even if the worker crashed after committing
        // the final offset but before sending the marker.
        let notified = cli(
            exe,
            &cfg.socket,
            &[
                "exec",
                &state.container,
                "/bin/sh",
                "-c",
                "rm -f /aq/out/transcript.raw; : > /aq/out/transcript.drained",
            ],
            None,
        )
        .await
        .is_ok();
        if !notified {
            let stopped = cli(
                exe,
                &cfg.socket,
                &[
                    "inspect",
                    "--format",
                    "{{.State.Running}}",
                    &state.container,
                ],
                None,
            )
            .await
            .is_ok_and(|v| String::from_utf8_lossy(&v).trim() == "false");
            if !stopped {
                return Ok(());
            }
        }
    }
    Ok(())
}
#[derive(Default, Serialize, Deserialize)]
struct Progress {
    uploaded: u64,
    finalized: bool,
    unsupported: bool,
}
fn save_progress(path: &Path, progress: &Progress) -> Result<()> {
    write_private(&path.join("upload.json"), &serde_json::to_vec(progress)?)
}
async fn upload(path: &Path, state: &State, link: &ServerLink) -> Result<()> {
    let progress_file = path.join("upload.json");
    let mut progress: Progress = if progress_file.exists() {
        serde_json::from_slice(&std::fs::read(progress_file)?)?
    } else {
        Progress::default()
    };
    if progress.finalized {
        return Ok(());
    }
    if progress.unsupported {
        return Ok(());
    }
    if progress.uploaded + 1 < state.next {
        let n = progress.uploaded + 1;
        let text = std::fs::read_to_string(chunk(path, n))?;
        match link
            .manual_transcript(state.id, &json!({"sequence":n,"text":text}))
            .await
        {
            Ok(ack) => {
                ensure!(
                    ack["accepted_through"].as_u64() == Some(n),
                    "unexpected archive acknowledgement"
                );
                progress.uploaded = n;
                save_progress(path, &progress)?;
                let _ = std::fs::remove_file(chunk(path, n));
            }
            Err(e)
                if e.downcast_ref::<LinkError>()
                    .is_some_and(|e| e.status == 404) =>
            {
                progress.unsupported = true;
                save_progress(path, &progress)?;
            }
            Err(_) => {}
        }
    } else if state.ended {
        let coverage = if state.reason.is_some() {
            "partial"
        } else {
            "complete"
        };
        match link
            .manual_transcript(
                state.id,
                &json!({"final_sequence":progress.uploaded,"coverage":coverage,"reason":state.reason}),
            )
            .await
        {
            Ok(ack) => {
                ensure!(
                    ack["accepted_through"].as_u64() == Some(progress.uploaded),
                    "unexpected archive acknowledgement"
                );
                progress.finalized = true;
                save_progress(path, &progress)?;
            }
            Err(e)
                if e.downcast_ref::<LinkError>()
                    .is_some_and(|e| e.status == 404) =>
            {
                progress.unsupported = true;
                save_progress(path, &progress)?;
            }
            Err(_) => {}
        }
    }
    Ok(())
}
pub(super) async fn run(
    root: PathBuf,
    node: Uuid,
    link: ServerLink,
    exe: PathBuf,
    cfg: DockerLocalConfig,
    _shutdown: watch::Receiver<bool>,
) {
    let capture = async {
        let mut errors = std::collections::BTreeMap::<PathBuf, u8>::new();
        loop {
            if let Ok(entries) = std::fs::read_dir(&root) {
                for entry in entries.flatten() {
                    let path = entry.path().join("recording");
                    if let Ok(bytes) = std::fs::read(path.join("state.json")) {
                        match serde_json::from_slice::<State>(&bytes) {
                            Ok(mut state) if state.node == node => {
                                if tick(&path, &mut state, &exe, &cfg).await.is_err() {
                                    let count = errors.entry(path.clone()).or_default();
                                    *count = count.saturating_add(1);
                                    let _ = write_private(
                                        &path.join("recording-error"),
                                        b"Recording could not read or persist output",
                                    );
                                    if *count == 1 {
                                        tracing::warn!(session_id=%state.id, "manual recording encountered a local error");
                                    }
                                    if *count >= 3 {
                                        let _ = write_private(
                                            &path.join("capture-lost"),
                                            b"capture lost",
                                        );
                                    }
                                } else {
                                    errors.remove(&path);
                                }
                            }
                            Err(_) => {
                                if !errors.contains_key(&path) {
                                    tracing::warn!("manual recording state is unreadable");
                                }
                                errors.insert(path.clone(), 1);
                                let _ = write_private(
                                    &path.join("recording-error"),
                                    b"Local recording state is unreadable",
                                );
                            }
                            _ => {}
                        }
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    };
    let uploads = async {
        loop {
            if let Ok(entries) = std::fs::read_dir(&root) {
                for entry in entries.flatten() {
                    let path = entry.path().join("recording");
                    if let Ok(bytes) = std::fs::read(path.join("state.json")) {
                        if let Ok(state) = serde_json::from_slice::<State>(&bytes) {
                            if state.node == node {
                                if upload(&path, &state, &link).await.is_err() {
                                    let _ = write_private(
                                        &path.join("upload-error"),
                                        b"Recording upload state or pending chunk is unreadable",
                                    );
                                }
                            }
                        }
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    };
    tokio::join!(capture, uploads);
}

pub(super) async fn request_stop(
    root: &Path,
    id: Uuid,
    exe: &Path,
    cfg: &DockerLocalConfig,
    container: &str,
) -> bool {
    let path = dir(root, id);
    let Ok(bytes) = std::fs::read(path.join("state.json")) else {
        return true;
    };
    if serde_json::from_slice::<State>(&bytes).is_ok_and(|s| s.ended) {
        return true;
    }
    if !path.join("stop-requested").exists() {
        let _ = write_private(&path.join("forced-stop"), b"forced stop");
        if tmux(exe, cfg, container, &["kill-session", "-t", "aq"], None)
            .await
            .is_err()
        {
            // Before the provider starts there is no pipe to drain. If a pipe
            // already exists, leave the recorder to consume its remaining bytes.
            let raw_exists = cli(
                exe,
                &cfg.socket,
                &[
                    "exec",
                    container,
                    "/bin/sh",
                    "-c",
                    "test -f /aq/out/transcript.raw",
                ],
                None,
            )
            .await
            .is_ok();
            if !raw_exists {
                let _ = write_private(&path.join("capture-lost"), b"capture lost");
                let _ = write_private(&path.join("stop-requested"), b"stop requested");
            }
        } else {
            let _ = write_private(&path.join("stop-requested"), b"stop requested");
        }
    }
    false
}
pub(super) async fn drain_before_stop(
    root: &Path,
    id: Uuid,
    exe: &Path,
    cfg: &DockerLocalConfig,
    container: &str,
) {
    for _ in 0..40 {
        if request_stop(root, id, exe, cfg, container).await {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    let _ = write_private(&dir(root, id).join("capture-lost"), b"capture lost");
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn split_sequences_and_credentials_are_safe() {
        let bytes = "α\x1b]52;c;secret\x07 sk-ant-abcdefghijklmnopqrstuv\rnext".as_bytes();
        for i in 0..=bytes.len() {
            let mut n = Normalizer::default();
            let mut out = n.feed(&bytes[..i], false);
            out.push_str(&n.feed(&bytes[i..], true));
            assert_eq!(out, "α sk-ant-**********************\n****");
        }
    }
    #[test]
    fn restart_keeps_pending_chunk_and_normalizer_state() {
        let root = tempfile::tempdir().unwrap();
        let id = Uuid::new_v4();
        initialize(root.path(), id, Uuid::new_v4(), "fixture").unwrap();
        let path = dir(root.path(), id);
        let mut state: State =
            serde_json::from_slice(&std::fs::read(path.join("state.json")).unwrap()).unwrap();
        let text = state.normalizer.feed(b"prefix sk-an", false);
        add(&path, &mut state, text).unwrap();
        state.offset = 12;
        save(&path, &state).unwrap();
        let mut restored: State =
            serde_json::from_slice(&std::fs::read(path.join("state.json")).unwrap()).unwrap();
        assert_eq!(
            std::fs::read_to_string(chunk(&path, 1)).unwrap(),
            "prefix sk-an"
        );
        assert_eq!(restored.offset, 12);
        assert_eq!(
            restored.normalizer.feed(b"t-abcdefghijklmnopqrst", true),
            "t-********************"
        );
    }
    #[test]
    fn normalization_expansion_and_size_limit_are_bounded() {
        let mut n = Normalizer::default();
        let text = n.feed(&vec![0xff; READ as usize], false);
        assert!(text.len() <= 32768);
        let root = tempfile::tempdir().unwrap();
        let id = Uuid::new_v4();
        initialize(root.path(), id, Uuid::new_v4(), "fixture").unwrap();
        let path = dir(root.path(), id);
        let mut state: State =
            serde_json::from_slice(&std::fs::read(path.join("state.json")).unwrap()).unwrap();
        state.bytes = TEXT_LIMIT - 2;
        add(&path, &mut state, "abc".into()).unwrap();
        assert_eq!(std::fs::read_to_string(chunk(&path, 1)).unwrap(), "ab");
        assert!(state.reason.is_some());
    }
    #[tokio::test]
    async fn lost_ack_retries_exact_chunk_after_restart_and_finalizes() {
        use axum::{Json, Router, http::StatusCode, routing::post};
        use std::sync::{Arc, Mutex};
        let received = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
        let seen = received.clone();
        let app = Router::new().route("/v1/worker/manual-agents/{id}/transcript", post(move |Json(body): Json<serde_json::Value>| {
            let seen = seen.clone();
            async move {
                let mut seen = seen.lock().unwrap();
                seen.push(body.clone());
                if seen.len() == 1 { return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({}))); }
                (StatusCode::OK, Json(json!({"accepted_through":1,"next_sequence":2,"archive_state":"complete"})))
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let root = tempfile::tempdir().unwrap();
        let id = Uuid::new_v4();
        initialize(root.path(), id, Uuid::new_v4(), "fixture").unwrap();
        let path = dir(root.path(), id);
        let mut state: State =
            serde_json::from_slice(&std::fs::read(path.join("state.json")).unwrap()).unwrap();
        add(&path, &mut state, "fixture text\n".into()).unwrap();
        state.ended = true;
        save(&path, &state).unwrap();
        let link = ServerLink::new(&origin, "fixture-credential", None).unwrap();
        upload(&path, &state, &link).await.unwrap();
        assert!(!path.join("upload.json").exists());
        let state: State =
            serde_json::from_slice(&std::fs::read(path.join("state.json")).unwrap()).unwrap();
        upload(&path, &state, &link).await.unwrap();
        upload(&path, &state, &link).await.unwrap();
        let bodies = received.lock().unwrap();
        assert_eq!(bodies.len(), 3);
        assert_eq!(bodies[0], bodies[1]);
        assert_eq!(bodies[2]["final_sequence"], 1);
        assert_eq!(bodies[2]["coverage"], "complete");
        drop(bodies);
        server.abort();
    }
    #[tokio::test]
    async fn fast_exit_drains_every_byte_through_real_tmux_pipe() {
        use std::os::unix::fs::PermissionsExt;
        if std::process::Command::new("tmux")
            .arg("-V")
            .output()
            .is_err()
        {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let out = temp.path().join("out");
        std::fs::create_dir(&out).unwrap();
        let exe = temp.path().join("remote-fixture");
        let script = format!(
            r##"#!/usr/bin/env python3
import os,sys
args=sys.argv[4:]
if args[0] == 'exec':
    args=[x.replace('/aq/out', {out:?}) for x in args[2:]]
    os.execvp(args[0],args)
sys.exit(1)
"##,
            out = out.to_str().unwrap()
        );
        std::fs::write(&exe, script).unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket = out.join("manual.sock");
        let tmux = |args: &[&str]| {
            std::process::Command::new("tmux")
                .arg("-S")
                .arg(&socket)
                .args(args)
                .output()
                .unwrap()
        };
        assert!(tmux(&["new-session", "-d", "-s", "aq", "while [ ! -e /aq/out/ready ]; do sleep 0.05; done; head -c 90000 /dev/zero | tr '\\0' x; printf LAST".replace("/aq/out", out.to_str().unwrap()).as_str()]).status.success());
        let pipe = format!(
            "umask 077; head -c 134217728 > {}/transcript.raw; : > {}/transcript.done",
            out.display(),
            out.display()
        );
        assert!(
            tmux(&["pipe-pane", "-O", "-t", "aq", &pipe])
                .status
                .success()
        );
        std::fs::write(out.join("ready"), b"").unwrap();
        for _ in 0..200 {
            if out.join("transcript.done").exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(out.join("transcript.done").exists());
        let id = Uuid::new_v4();
        initialize(temp.path(), id, Uuid::new_v4(), "fixture").unwrap();
        let path = dir(temp.path(), id);
        let cfg = DockerLocalConfig::new(temp.path().join("fixture.sock"));
        let mut state: State =
            serde_json::from_slice(&std::fs::read(path.join("state.json")).unwrap()).unwrap();
        while !state.ended {
            tick(&path, &mut state, &exe, &cfg).await.unwrap();
        }
        assert!(state.reason.is_none());
        let mut text = String::new();
        for n in 1..state.next {
            text.push_str(&std::fs::read_to_string(chunk(&path, n)).unwrap());
        }
        assert_eq!(text.len(), 90004);
        assert!(text.starts_with(&"x".repeat(90000)));
        assert!(text.ends_with("LAST"));
        assert!(out.join("transcript.drained").exists());
    }
    #[tokio::test]
    async fn stop_drains_backlog_before_returning_ready() {
        use std::os::unix::fs::PermissionsExt;
        if std::process::Command::new("tmux")
            .arg("-V")
            .output()
            .is_err()
        {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let out = temp.path().join("out");
        std::fs::create_dir(&out).unwrap();
        let exe = temp.path().join("remote-fixture");
        let script = format!(
            "#!/usr/bin/env python3\nimport os,sys\na=sys.argv[4:]\nif a[0]=='exec':\n a=[x.replace('/aq/out',{:?}) for x in a[2:]]\n os.execvp(a[0],a)\nsys.exit(1)\n",
            out.to_str().unwrap()
        );
        std::fs::write(&exe, script).unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket = out.join("manual.sock");
        let tmux = |args: &[&str]| {
            std::process::Command::new("tmux")
                .arg("-S")
                .arg(&socket)
                .args(args)
                .output()
                .unwrap()
        };
        let command = format!(
            "while [ ! -e {0}/ready ]; do sleep 0.05; done; head -c 150000 /dev/zero | tr '\\0' x; printf LAST; : > {0}/produced; sleep 30",
            out.display()
        );
        assert!(
            tmux(&["new-session", "-d", "-s", "aq", &command])
                .status
                .success()
        );
        let pipe = format!(
            "umask 077; head -c 134217728 > {0}/transcript.raw; : > {0}/transcript.done",
            out.display()
        );
        assert!(
            tmux(&["pipe-pane", "-O", "-t", "aq", &pipe])
                .status
                .success()
        );
        std::fs::write(out.join("ready"), b"").unwrap();
        for _ in 0..400 {
            if out.join("produced").exists()
                && std::fs::metadata(out.join("transcript.raw")).is_ok_and(|m| m.len() >= 140000)
            {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(out.join("produced").exists());
        let id = Uuid::new_v4();
        initialize(temp.path(), id, Uuid::new_v4(), "fixture").unwrap();
        let path = dir(temp.path(), id);
        let cfg = DockerLocalConfig::new(temp.path().join("fixture.sock"));
        assert!(!request_stop(temp.path(), id, &exe, &cfg, "fixture").await);
        let mut state: State =
            serde_json::from_slice(&std::fs::read(path.join("state.json")).unwrap()).unwrap();
        for _ in 0..100 {
            tick(&path, &mut state, &exe, &cfg).await.unwrap();
            if state.ended {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(state.ended);
        assert!(state.reason.is_some());
        assert!(request_stop(temp.path(), id, &exe, &cfg, "fixture").await);
        let mut text = String::new();
        for n in 1..state.next {
            text.push_str(&std::fs::read_to_string(chunk(&path, n)).unwrap());
        }
        assert_eq!(text.len(), 150004);
        assert!(text.ends_with("LAST"));
    }
}
