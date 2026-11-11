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
    uploaded: u64,
    normalizer: Normalizer,
    ended: bool,
    finalized: bool,
    unsupported: bool,
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
            uploaded: 0,
            normalizer: Normalizer::default(),
            ended: false,
            finalized: false,
            unsupported: false,
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
async fn tick(
    path: &Path,
    state: &mut State,
    link: &ServerLink,
    exe: &Path,
    cfg: &DockerLocalConfig,
) -> Result<()> {
    if state.finalized {
        return Ok(());
    }
    // Read bounded blocks into durable immutable chunks before attempting HTTP.
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
    if state.unsupported {
        return Ok(());
    }
    if state.uploaded + 1 < state.next {
        let n = state.uploaded + 1;
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
                state.uploaded = n;
                save(path, state)?;
                let _ = std::fs::remove_file(chunk(path, n));
            }
            Err(e)
                if e.downcast_ref::<LinkError>()
                    .is_some_and(|e| e.status == 404) =>
            {
                state.unsupported = true;
                save(path, state)?;
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
                &json!({"final_sequence":state.uploaded,"coverage":coverage,"reason":state.reason}),
            )
            .await
        {
            Ok(ack) => {
                ensure!(
                    ack["accepted_through"].as_u64() == Some(state.uploaded),
                    "unexpected archive acknowledgement"
                );
                state.finalized = true;
                save(path, state)?;
            }
            Err(e)
                if e.downcast_ref::<LinkError>()
                    .is_some_and(|e| e.status == 404) =>
            {
                state.unsupported = true;
                save(path, state)?;
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
    loop {
        if let Ok(entries) = std::fs::read_dir(&root) {
            for entry in entries.flatten() {
                let path = entry.path().join("recording");
                if let Ok(bytes) = std::fs::read(path.join("state.json")) {
                    if let Ok(mut state) = serde_json::from_slice::<State>(&bytes) {
                        if state.node == node {
                            let _ = tick(&path, &mut state, &link, &exe, &cfg).await;
                        }
                    }
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}
pub(super) async fn drain_before_stop(
    root: &Path,
    id: Uuid,
    exe: &Path,
    cfg: &DockerLocalConfig,
    container: &str,
) {
    let path = dir(root, id);
    if !path.join("state.json").exists() {
        return;
    }
    let _ = tmux(exe, cfg, container, &["kill-session", "-t", "aq"], None).await;
    for _ in 0..40 {
        if let Ok(bytes) = std::fs::read(path.join("state.json")) {
            if serde_json::from_slice::<State>(&bytes).is_ok_and(|s| s.ended) {
                return;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    let _ = write_private(&path.join("capture-lost"), b"capture lost");
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
            assert_eq!(out, "α sk-ant-**********************\nnext");
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
}
