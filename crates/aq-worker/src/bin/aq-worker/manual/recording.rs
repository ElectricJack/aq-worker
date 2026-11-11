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
struct Identity {
    id: Uuid,
    node: Uuid,
    container: String,
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
    // State first prevents a concurrent scanner from mistaking initialization
    // for a lost state; readable existing states are never overwritten.
    if !path.join("state.json").exists() {
        ensure!(
            !path.join("identity.json").exists(),
            "recording state is missing"
        );
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
        )?;
    }
    if !path.join("identity.json").exists() {
        write_private(
            &path.join("identity.json"),
            &serde_json::to_vec(&Identity {
                id,
                node,
                container: container.into(),
            })?,
        )?;
    }
    Ok(())
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
                    "{{.State.Running}} {{.State.StartedAt}}",
                    &state.container,
                ],
                None,
            )
            .await
            .is_ok_and(|v| {
                let output = String::from_utf8_lossy(&v);
                let mut fields = output.split_whitespace();
                fields.next() == Some("false")
                    && fields
                        .next()
                        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                        .is_some_and(|started| started.timestamp() > 0)
            });
            // A created sandbox is not running yet, but that is not capture
            // loss. A nonzero start time proves the runner was launched.
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
// Release the runner without deleting the raw spool. Archive recovery uses the
// independently persisted identity and server's acknowledged high-water mark.
async fn release_corrupt(
    path: &Path,
    node: Uuid,
    exe: &Path,
    cfg: &DockerLocalConfig,
) -> Result<()> {
    if path.join("corrupt-released").exists() {
        return Ok(());
    }
    let identity: Identity = serde_json::from_slice(&std::fs::read(path.join("identity.json"))?)?;
    ensure!(
        identity.node == node,
        "recording identity belongs to another node"
    );
    ensure!(
        path.parent()
            .and_then(Path::file_name)
            .and_then(|s| s.to_str())
            == Some(identity.id.to_string().as_str()),
        "recording identity does not match directory"
    );
    cli(
        exe,
        &cfg.socket,
        &[
            "exec",
            &identity.container,
            "/bin/sh",
            "-c",
            ": > /aq/out/transcript.drained",
        ],
        None,
    )
    .await?;
    write_private(
        &path.join("corrupt-released"),
        b"Recording state unreadable; archive requires recovery",
    )?;
    Ok(())
}

const CORRUPT_REASON: &str = "Recording state was lost; retained output may be incomplete";
#[derive(Serialize, Deserialize)]
struct CorruptRecovery {
    sequence: u64,
    #[serde(default)]
    conflicts: u8,
}

async fn recover_corrupt_archive(path: &Path, node: Uuid, link: &ServerLink) -> Result<()> {
    if path.join("corrupt-finalized").exists() || path.join("corrupt-recovery-needed").exists() {
        return Ok(());
    }
    let identity: Identity = serde_json::from_slice(&std::fs::read(path.join("identity.json"))?)?;
    ensure!(
        identity.node == node,
        "recording identity belongs to another node"
    );
    ensure!(
        path.parent()
            .and_then(Path::file_name)
            .and_then(|s| s.to_str())
            == Some(identity.id.to_string().as_str()),
        "recording identity does not match directory"
    );
    let progress: Option<Progress> = std::fs::read(path.join("upload.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok());
    if progress
        .as_ref()
        .is_some_and(|p| p.finalized || p.unsupported)
    {
        return Ok(());
    }
    let recovery_path = path.join("corrupt-recovery.json");
    let mut recovery = match std::fs::read(&recovery_path) {
        Ok(bytes) => serde_json::from_slice::<CorruptRecovery>(&bytes)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => CorruptRecovery {
            sequence: progress
                .map(|p| p.uploaded)
                .filter(|n| *n <= TEXT_LIMIT)
                .unwrap_or(0),
            conflicts: 0,
        },
        Err(e) => return Err(e.into()),
    };
    ensure!(
        recovery.sequence <= TEXT_LIMIT,
        "invalid recording recovery sequence"
    );
    // Persist the proposed cursor so repeated conflicts cannot spin after restart.
    write_private(&recovery_path, &serde_json::to_vec(&recovery)?)?;
    match link.manual_transcript(identity.id, &json!({"final_sequence":recovery.sequence,"coverage":"partial","reason":CORRUPT_REASON})).await {
        Ok(ack) => {
            ensure!(ack["accepted_through"].as_u64() == Some(recovery.sequence), "unexpected archive acknowledgement");
            write_private(&path.join("corrupt-finalized"), b"Partial archive finalized after recording state loss")?;
        }
        Err(e) => {
            if let Some(conflict) = e.downcast_ref::<LinkError>().filter(|e| e.status == 409 && e.code.as_deref() == Some("manual_transcript_conflict")) {
                match conflict.next_sequence.filter(|n| (1..=TEXT_LIMIT + 1).contains(n)).map(|n| n - 1) {
                    Some(sequence) if sequence != recovery.sequence && recovery.conflicts == 0 => {
                        recovery.sequence = sequence;
                        recovery.conflicts = 1;
                        write_private(&recovery_path, &serde_json::to_vec(&recovery)?)?;
                    }
                    _ => { write_private(&path.join("corrupt-recovery-needed"), b"Archive finalization needs manual recovery")?; }
                }
            } else if e.downcast_ref::<LinkError>().is_some_and(|e| e.status == 404) {
                write_private(&path.join("corrupt-recovery-needed"), b"Archive finalization needs manual recovery")?;
            }
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
                                // Recordings created before identity sidecars were introduced
                                // get one while their mutable state is still readable.
                                if !path.join("identity.json").exists()
                                    && path
                                        .parent()
                                        .and_then(Path::file_name)
                                        .and_then(|s| s.to_str())
                                        == Some(state.id.to_string().as_str())
                                {
                                    let identity = Identity {
                                        id: state.id,
                                        node: state.node,
                                        container: state.container.clone(),
                                    };
                                    if let Ok(bytes) = serde_json::to_vec(&identity) {
                                        let _ = write_private(&path.join("identity.json"), &bytes);
                                    }
                                }
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
                                let _ = release_corrupt(&path, node, &exe, &cfg).await;
                            }
                            _ => {}
                        }
                    } else if path.join("identity.json").exists() {
                        let _ = write_private(
                            &path.join("recording-error"),
                            b"Local recording state is unreadable",
                        );
                        let _ = release_corrupt(&path, node, &exe, &cfg).await;
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
                        match serde_json::from_slice::<State>(&bytes) {
                            Ok(state) if state.node == node => {
                                if upload(&path, &state, &link).await.is_err() {
                                    let _ = write_private(
                                        &path.join("upload-error"),
                                        b"Recording upload state or pending chunk is unreadable",
                                    );
                                }
                            }
                            Err(_) => {
                                let _ = recover_corrupt_archive(&path, node, &link).await;
                            }
                            _ => {}
                        }
                    } else if path.join("identity.json").exists() {
                        let _ = recover_corrupt_archive(&path, node, &link).await;
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    };
    tokio::join!(capture, uploads);
}

// The creator can fail before a sandbox is started. Do not infer this from
// Running=false: created containers are expected during normal preparation.
pub(super) fn startup_failed(root: &Path, id: Uuid) {
    let path = dir(root, id);
    if path.join("state.json").exists() {
        let _ = write_private(&path.join("capture-lost"), b"startup failed");
    }
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
    let state = match serde_json::from_slice::<State>(&bytes) {
        Ok(state) => state,
        Err(_) => {
            let _ = write_private(
                &path.join("recording-error"),
                b"Local recording state is unreadable",
            );
            let _ = tmux(exe, cfg, container, &["kill-session", "-t", "aq"], None).await;
            let _ = cli(
                exe,
                &cfg.socket,
                &[
                    "exec",
                    container,
                    "/bin/sh",
                    "-c",
                    ": > /aq/out/transcript.drained",
                ],
                None,
            )
            .await;
            return true;
        }
    };
    if state.ended {
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
    #[tokio::test]
    async fn raw_limit_is_bounded_and_finalizes_partial() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let out = temp.path().join("out");
        std::fs::create_dir(&out).unwrap();
        let raw = std::fs::File::create(out.join("transcript.raw")).unwrap();
        raw.set_len(RAW_LIMIT).unwrap();
        std::fs::write(out.join("transcript.done"), b"").unwrap();
        let exe = temp.path().join("remote-fixture");
        let script = format!(
            "#!/usr/bin/env python3\nimport os,sys\na=sys.argv[4:]\nif a[0]=='exec':\n a=[x.replace('/aq/out',{:?}) for x in a[2:]]\n os.execvp(a[0],a)\nsys.exit(1)\n",
            out.to_str().unwrap()
        );
        std::fs::write(&exe, script).unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o700)).unwrap();
        let id = Uuid::new_v4();
        initialize(temp.path(), id, Uuid::new_v4(), "fixture").unwrap();
        let path = dir(temp.path(), id);
        let cfg = DockerLocalConfig::new(temp.path().join("fixture.sock"));
        let mut state: State =
            serde_json::from_slice(&std::fs::read(path.join("state.json")).unwrap()).unwrap();
        state.offset = RAW_LIMIT - 4;
        tick(&path, &mut state, &exe, &cfg).await.unwrap();
        assert_eq!(state.offset, RAW_LIMIT);
        assert!(state.ended);
        assert_eq!(state.reason.as_deref(), Some("Raw output limit reached"));
        assert!(!out.join("transcript.raw").exists());
        assert!(out.join("transcript.drained").exists());
    }

    #[tokio::test]
    async fn upload_resumes_after_server_restart() {
        use axum::{Json, Router, routing::post};
        use std::sync::{Arc, Mutex};
        let received = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let id = Uuid::new_v4();
        initialize(temp.path(), id, Uuid::new_v4(), "fixture").unwrap();
        let path = dir(temp.path(), id);
        let mut state: State =
            serde_json::from_slice(&std::fs::read(path.join("state.json")).unwrap()).unwrap();
        add(&path, &mut state, "before restart".into()).unwrap();
        save(&path, &state).unwrap();
        let link = ServerLink::new(&format!("http://{addr}"), "fixture-credential", None).unwrap();
        let seen = received.clone();
        let app = Router::new().route("/v1/worker/manual-agents/{id}/transcript", post(move |Json(body): Json<serde_json::Value>| {
            let seen = seen.clone();
            async move {
                let sequence = body["sequence"].as_u64().or_else(|| body["final_sequence"].as_u64()).unwrap();
                seen.lock().unwrap().push(body);
                Json(json!({"accepted_through":sequence,"next_sequence":sequence+1,"archive_state":"recording"}))
            }
        }));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        upload(&path, &state, &link).await.unwrap();
        server.abort();
        let _ = server.await;
        add(&path, &mut state, "after restart".into()).unwrap();
        state.ended = true;
        save(&path, &state).unwrap();
        upload(&path, &state, &link).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<Progress>(&std::fs::read(path.join("upload.json")).unwrap())
                .unwrap()
                .uploaded,
            1
        );
        let seen = received.clone();
        let app = Router::new().route(
            "/v1/worker/manual-agents/{id}/transcript",
            post(move |Json(body): Json<serde_json::Value>| {
                let seen = seen.clone();
                async move {
                    seen.lock().unwrap().push(body);
                    Json(json!({"accepted_through":2,"next_sequence":3,"archive_state":"complete"}))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let state: State =
            serde_json::from_slice(&std::fs::read(path.join("state.json")).unwrap()).unwrap();
        upload(&path, &state, &link).await.unwrap();
        upload(&path, &state, &link).await.unwrap();
        let bodies = received.lock().unwrap();
        assert_eq!(bodies.len(), 3);
        assert_eq!(bodies[0]["text"], "before restart");
        assert_eq!(bodies[1]["text"], "after restart");
        assert_eq!(bodies[2]["final_sequence"], 2);
        assert!(
            serde_json::from_slice::<Progress>(&std::fs::read(path.join("upload.json")).unwrap())
                .unwrap()
                .finalized
        );
        drop(bodies);
        server.abort();
    }
    #[tokio::test]
    async fn corrupt_state_releases_runner_and_stop_without_erasing_chunks() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let out = temp.path().join("out");
        std::fs::create_dir(&out).unwrap();
        let exe = temp.path().join("remote-fixture");
        let script = format!(
            "#!/usr/bin/env python3\nimport os,sys\na=sys.argv[4:]\nif a[0]=='exec':\n a=[x.replace('/aq/out',{:?}) for x in a[2:]]\n if a[0]=='tmux':\n  open({:?},'w').close(); sys.exit(0)\n os.execvp(a[0],a)\nsys.exit(1)\n",
            out.to_str().unwrap(),
            temp.path().join("stopped").to_str().unwrap()
        );
        std::fs::write(&exe, script).unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o700)).unwrap();
        let id = Uuid::new_v4();
        let node = Uuid::new_v4();
        initialize(temp.path(), id, node, "fixture").unwrap();
        let path = dir(temp.path(), id);
        std::fs::write(chunk(&path, 1), b"retained fixture").unwrap();
        std::fs::write(path.join("state.json"), b"corrupt").unwrap();
        let cfg = DockerLocalConfig::new(temp.path().join("fixture.sock"));
        assert!(
            release_corrupt(&path, Uuid::new_v4(), &exe, &cfg)
                .await
                .is_err()
        );
        assert!(!out.join("transcript.drained").exists());
        release_corrupt(&path, node, &exe, &cfg).await.unwrap();
        assert!(out.join("transcript.drained").exists());
        assert!(path.join("corrupt-released").exists());
        assert!(!temp.path().join("stopped").exists());
        assert!(request_stop(temp.path(), id, &exe, &cfg, "fixture").await);
        assert!(temp.path().join("stopped").exists());
        assert_eq!(std::fs::read(chunk(&path, 1)).unwrap(), b"retained fixture");
        assert!(path.join("recording-error").exists());
        assert!(!path.join("upload.json").exists());
        std::fs::remove_file(path.join("state.json")).unwrap();
        assert!(initialize(temp.path(), id, node, "fixture").is_err());
        assert!(!path.join("state.json").exists());
    }
    /// Opt-in integration with a real archive server supplied by its test harness.
    #[tokio::test]
    #[ignore = "requires disposable Docker image and private server fixture"]
    async fn real_archive_docker_fixture() {
        use std::os::unix::fs::PermissionsExt;
        #[derive(Deserialize)]
        struct Fixture {
            origin: String,
            credential: String,
            session_id: Uuid,
            node_id: Uuid,
        }
        let fixture_path =
            std::env::var_os("AQ_RECORDING_FIXTURE").expect("private fixture path required");
        let fixture: Fixture =
            serde_json::from_slice(&std::fs::read(fixture_path).unwrap()).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let container_file = temp.path().join("container");
        let helper =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/qualify-manual-recording.py");
        let mut child = tokio::process::Command::new("python3")
            .arg(helper)
            .arg("--container-file")
            .arg(&container_file)
            .spawn()
            .unwrap();
        let mut container = String::new();
        for _ in 0..300 {
            if let Ok(name) = std::fs::read_to_string(&container_file) {
                if !name.is_empty() {
                    container = name;
                    break;
                }
            }
            assert!(
                child.try_wait().unwrap().is_none(),
                "Docker fixture exited early"
            );
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(!container.is_empty(), "Docker fixture timed out");
        let exe = temp.path().join("docker-adapter");
        std::fs::write(&exe, "#!/bin/sh\nshift 3\nexec docker \"$@\"\n").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o700)).unwrap();
        initialize(temp.path(), fixture.session_id, fixture.node_id, &container).unwrap();
        let path = dir(temp.path(), fixture.session_id);
        let cfg = DockerLocalConfig::new(temp.path().join("unused.sock"));
        let mut state: State =
            serde_json::from_slice(&std::fs::read(path.join("state.json")).unwrap()).unwrap();
        for _ in 0..30 {
            tick(&path, &mut state, &exe, &cfg).await.unwrap();
            if state.ended {
                break;
            }
        }
        assert!(state.ended && state.reason.is_none());
        let link = ServerLink::new(&fixture.origin, &fixture.credential, None).unwrap();
        for _ in 0..120 {
            upload(&path, &state, &link).await.unwrap();
            if path.join("upload.json").exists() {
                let progress: Progress =
                    serde_json::from_slice(&std::fs::read(path.join("upload.json")).unwrap())
                        .unwrap();
                assert!(
                    !progress.unsupported,
                    "server does not support archive uploads"
                );
                if progress.finalized {
                    break;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
        let progress: Progress =
            serde_json::from_slice(&std::fs::read(path.join("upload.json")).unwrap()).unwrap();
        assert!(progress.finalized, "archive upload did not finalize");
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(15), child.wait())
                .await
                .unwrap()
                .unwrap()
                .success()
        );
    }
    #[tokio::test]
    async fn corrupt_recovery_uses_server_high_water_and_bounds_conflicts() {
        use axum::{Json, Router, http::StatusCode, routing::post};
        use std::sync::{Arc, Mutex};
        let bodies = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
        let seen = bodies.clone();
        let app = Router::new().route("/v1/worker/manual-agents/{id}/transcript", post(move |Json(body): Json<serde_json::Value>| {
            let seen = seen.clone();
            async move {
                seen.lock().unwrap().push(body.clone());
                if body["final_sequence"] == 2 {
                    (StatusCode::OK, Json(json!({"accepted_through":2,"next_sequence":3,"archive_state":"complete"})))
                } else {
                    (StatusCode::CONFLICT, Json(json!({"error":"manual_transcript_conflict","message":"conflict","next_sequence":3})))
                }
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let root = tempfile::tempdir().unwrap();
        let id = Uuid::new_v4();
        let node = Uuid::new_v4();
        initialize(root.path(), id, node, "fixture").unwrap();
        let path = dir(root.path(), id);
        write_private(
            &path.join("upload.json"),
            &serde_json::to_vec(&Progress {
                uploaded: 1,
                ..Progress::default()
            })
            .unwrap(),
        )
        .unwrap();
        std::fs::write(path.join("state.json"), b"corrupt").unwrap();
        std::fs::write(chunk(&path, 2), b"pending fixture").unwrap();
        let link = ServerLink::new(&origin, "fixture-credential", None).unwrap();
        recover_corrupt_archive(&path, node, &link).await.unwrap();
        assert!(!path.join("corrupt-finalized").exists());
        recover_corrupt_archive(&path, node, &link).await.unwrap();
        assert!(path.join("corrupt-finalized").exists());
        assert_eq!(std::fs::read(chunk(&path, 2)).unwrap(), b"pending fixture");
        let seen = bodies.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0]["final_sequence"], 1);
        assert_eq!(seen[1]["final_sequence"], 2);
        assert_eq!(seen[1]["coverage"], "partial");
        drop(seen);
        // A conflicting finalization at the same server high-water mark is terminal.
        let id2 = Uuid::new_v4();
        initialize(root.path(), id2, node, "fixture").unwrap();
        let path2 = dir(root.path(), id2);
        write_private(
            &path2.join("upload.json"),
            &serde_json::to_vec(&Progress {
                uploaded: 2,
                ..Progress::default()
            })
            .unwrap(),
        )
        .unwrap();
        // The mock accepts sequence 2, so emulate a previous divergent response by
        // persisting the one allowed cursor adjustment and asking at sequence 1.
        write_private(
            &path2.join("corrupt-recovery.json"),
            &serde_json::to_vec(&CorruptRecovery {
                sequence: 1,
                conflicts: 1,
            })
            .unwrap(),
        )
        .unwrap();
        recover_corrupt_archive(&path2, node, &link).await.unwrap();
        assert!(path2.join("corrupt-recovery-needed").exists());
        recover_corrupt_archive(&path2, node, &link).await.unwrap();
        assert_eq!(bodies.lock().unwrap().len(), 3);
        server.abort();
    }

    #[tokio::test]
    #[ignore = "requires a private real-server fixture"]
    async fn real_archive_corrupt_recovery_fixture() {
        #[derive(Deserialize)]
        struct Fixture {
            origin: String,
            credential: String,
            session_id: Uuid,
            node_id: Uuid,
        }
        let fixture: Fixture = serde_json::from_slice(
            &std::fs::read(std::env::var_os("AQ_RECORDING_FIXTURE").unwrap()).unwrap(),
        )
        .unwrap();
        let link = ServerLink::new(&fixture.origin, &fixture.credential, None).unwrap();
        for sequence in 1..=2 {
            let ack = link
                .manual_transcript(
                    fixture.session_id,
                    &json!({"sequence":sequence,"text":format!("fixture retained {sequence}\n")}),
                )
                .await
                .unwrap();
            assert_eq!(ack["accepted_through"], sequence);
        }
        let root = tempfile::tempdir().unwrap();
        initialize(root.path(), fixture.session_id, fixture.node_id, "fixture").unwrap();
        let path = dir(root.path(), fixture.session_id);
        write_private(
            &path.join("upload.json"),
            &serde_json::to_vec(&Progress {
                uploaded: 1,
                ..Progress::default()
            })
            .unwrap(),
        )
        .unwrap();
        std::fs::write(path.join("state.json"), b"corrupt").unwrap();
        std::fs::write(chunk(&path, 2), b"fixture pending").unwrap();
        recover_corrupt_archive(&path, fixture.node_id, &link)
            .await
            .unwrap();
        recover_corrupt_archive(&path, fixture.node_id, &link)
            .await
            .unwrap();
        assert!(path.join("corrupt-finalized").exists());
        assert!(chunk(&path, 2).exists());
    }
    #[tokio::test]
    async fn created_container_waits_and_fast_exit_drains_before_finalizing() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let out = temp.path().join("out");
        std::fs::create_dir(&out).unwrap();
        let exe = temp.path().join("remote-fixture");
        let script = format!(
            "#!/usr/bin/env python3\nimport os,sys\na=sys.argv[4:]\nif a[0]=='inspect':\n print('false 0001-01-01T00:00:00Z'); sys.exit(0)\nif a[0]=='exec' and os.path.exists({ready:?}):\n a=[x.replace('/aq/out',{out:?}) for x in a[2:]]\n os.execvp(a[0],a)\nsys.exit(1)\n",
            ready = temp.path().join("started").to_str().unwrap(),
            out = out.to_str().unwrap()
        );
        std::fs::write(&exe, script).unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o700)).unwrap();
        let id = Uuid::new_v4();
        initialize(temp.path(), id, Uuid::new_v4(), "fixture").unwrap();
        let path = dir(temp.path(), id);
        let cfg = DockerLocalConfig::new(temp.path().join("fixture.sock"));
        let mut state: State =
            serde_json::from_slice(&std::fs::read(path.join("state.json")).unwrap()).unwrap();
        for _ in 0..3 {
            tick(&path, &mut state, &exe, &cfg).await.unwrap();
            assert!(!state.ended);
            assert!(state.reason.is_none());
        }
        let restored: State =
            serde_json::from_slice(&std::fs::read(path.join("state.json")).unwrap()).unwrap();
        assert!(!restored.ended);
        // The provider has already exited by the next tick, leaving a fully
        // drained pipe in the still-running sandbox runner.
        std::fs::write(
            out.join("transcript.raw"),
            b"initial fixture\nfinal fixture\n",
        )
        .unwrap();
        std::fs::write(out.join("transcript.done"), b"").unwrap();
        std::fs::write(temp.path().join("started"), b"").unwrap();
        tick(&path, &mut state, &exe, &cfg).await.unwrap();
        assert!(state.ended && state.reason.is_none());
        assert_eq!(
            std::fs::read_to_string(chunk(&path, 1)).unwrap(),
            "initial fixture\nfinal fixture\n"
        );
        assert!(out.join("transcript.drained").exists());
    }

    #[tokio::test]
    async fn previously_started_stopped_container_reports_capture_loss() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let exe = temp.path().join("remote-fixture");
        std::fs::write(&exe, "#!/bin/sh\nshift 3\nif [ \"$1\" = inspect ]; then echo 'false 2026-11-11T12:00:00Z'; exit 0; fi\nexit 1\n").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o700)).unwrap();
        let id = Uuid::new_v4();
        initialize(temp.path(), id, Uuid::new_v4(), "fixture").unwrap();
        let path = dir(temp.path(), id);
        let cfg = DockerLocalConfig::new(temp.path().join("fixture.sock"));
        let mut state: State =
            serde_json::from_slice(&std::fs::read(path.join("state.json")).unwrap()).unwrap();
        tick(&path, &mut state, &exe, &cfg).await.unwrap();
        assert!(state.ended);
        assert_eq!(
            state.reason.as_deref(),
            Some("Capture ended before the final output could be drained")
        );
    }

    #[test]
    fn startup_failure_marks_existing_recording_as_partial() {
        let temp = tempfile::tempdir().unwrap();
        let id = Uuid::new_v4();
        startup_failed(temp.path(), id);
        assert!(!dir(temp.path(), id).exists());
        initialize(temp.path(), id, Uuid::new_v4(), "fixture").unwrap();
        startup_failed(temp.path(), id);
        assert!(dir(temp.path(), id).join("capture-lost").exists());
    }
    #[tokio::test]
    #[ignore = "requires Docker and the local aq-bootstrap-tmux-proof:26.04 image"]
    async fn real_created_container_waits_before_first_start() {
        use std::os::unix::fs::PermissionsExt;
        struct Container(String);
        impl Drop for Container {
            fn drop(&mut self) {
                let _ = std::process::Command::new("docker")
                    .args(["rm", "-f", &self.0])
                    .output();
            }
        }
        let temp = tempfile::tempdir().unwrap();
        let container = Container(format!(
            "aq-recorder-created-fixture-{}",
            Uuid::new_v4().simple()
        ));
        let command = "mkdir -p /aq/out; printf 'fixture begin\\nfixture end\\n' > /aq/out/transcript.raw; : > /aq/out/transcript.done; while [ ! -e /aq/out/transcript.drained ]; do sleep 0.1; done";
        assert!(
            std::process::Command::new("docker")
                .args([
                    "create",
                    "--network",
                    "none",
                    "--name",
                    &container.0,
                    "aq-bootstrap-tmux-proof:26.04",
                    "/bin/sh",
                    "-c",
                    command
                ])
                .output()
                .unwrap()
                .status
                .success()
        );
        let exe = temp.path().join("docker-adapter");
        std::fs::write(&exe, "#!/bin/sh\nshift 3\nexec docker \"$@\"\n").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o700)).unwrap();
        let id = Uuid::new_v4();
        initialize(temp.path(), id, Uuid::new_v4(), &container.0).unwrap();
        let path = dir(temp.path(), id);
        let cfg = DockerLocalConfig::new(temp.path().join("unused.sock"));
        let mut state: State =
            serde_json::from_slice(&std::fs::read(path.join("state.json")).unwrap()).unwrap();
        tick(&path, &mut state, &exe, &cfg).await.unwrap();
        assert!(!state.ended && state.reason.is_none());
        assert!(
            std::process::Command::new("docker")
                .args(["start", &container.0])
                .output()
                .unwrap()
                .status
                .success()
        );
        for _ in 0..100 {
            let done = std::process::Command::new("docker")
                .args([
                    "exec",
                    &container.0,
                    "test",
                    "-e",
                    "/aq/out/transcript.done",
                ])
                .output()
                .unwrap()
                .status
                .success();
            if done {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        tick(&path, &mut state, &exe, &cfg).await.unwrap();
        assert!(state.ended && state.reason.is_none());
        assert_eq!(
            std::fs::read_to_string(chunk(&path, 1)).unwrap(),
            "fixture begin\nfixture end\n"
        );
    }
}
