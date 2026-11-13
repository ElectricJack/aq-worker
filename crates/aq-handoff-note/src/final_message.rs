//! Owner ruling (2026-11-13, result size): when an attempt has an agent channel,
//! the driver posts the agent's whole final message (at most 16 KiB, clipped with
//! an explicit marker) as an agent comment on the task, once per attempt. The
//! result report's conclusion stays a bounded summary. Without a channel only the
//! summary exists.
use anyhow::{Context, Result, ensure};
use std::io::{Read, Write};
use std::path::Path;
use uuid::Uuid;

/// aq-server's task comment body bound (`aq_task_comments::MAX_BODY_BYTES`).
pub const MAX_COMMENT_BYTES: usize = 16 * 1024;
const CLIPPED: &str = "[clipped: the final message exceeded 16 KiB]";

/// The comment's operation id: one per attempt, so a retried post replays.
pub fn comment_operation(attempt: Uuid) -> Uuid {
    Uuid::new_v5(&attempt, b"aq.agent-final-message")
}

/// Plain text: the message sits in a fenced block, so no markup in it renders.
/// A line that would close the fence gets a zero-width space first. Control
/// characters other than tab and line feed are dropped.
pub fn comment_body(attempt: Uuid, message: &str) -> String {
    let head = format!("Final message of attempt {attempt}:\n\n```text\n");
    let tail = "\n```\n";
    let clean: String = message
        .lines()
        .map(|l| {
            let l: String = l
                .chars()
                .filter(|c| !c.is_control() || *c == '\t')
                .collect();
            if l.trim_start().starts_with("```") {
                format!("\u{200B}{l}")
            } else {
                l
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let room = MAX_COMMENT_BYTES - head.len() - tail.len();
    let text = if clean.len() <= room {
        clean
    } else {
        let mut cut = room - CLIPPED.len() - 1;
        while !clean.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}\n{CLIPPED}", &clean[..cut])
    };
    format!("{head}{text}{tail}")
}

/// Posts the comment through the attempt's channel socket (`<root>/<attempt>/aq.sock`,
/// spec 0006 §5), which adds the attempt's credential. A 200 reply, including a
/// replay of the same operation, means the comment exists.
pub fn post(root: &Path, attempt: Uuid, message: &str) -> Result<()> {
    let socket = root
        .join(attempt.to_string())
        .join(aq_agent_channel::SOCKET_NAME);
    let body = serde_json::to_vec(&serde_json::json!({
        "api_version": 1,
        "operation_id": comment_operation(attempt),
        "body": comment_body(attempt, message),
    }))?;
    let mut stream = std::os::unix::net::UnixStream::connect(&socket).context("agent channel")?;
    let limit = Some(std::time::Duration::from_secs(5));
    stream.set_read_timeout(limit)?;
    stream.set_write_timeout(limit)?;
    let head = format!(
        "POST /v1/agent/comments HTTP/1.1\r\nHost: aq\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(&body)?;
    let mut reply = Vec::new();
    stream.take(64 * 1024).read_to_end(&mut reply)?;
    let status = reply.split(|b| *b == b'\n').next().unwrap_or_default();
    ensure!(
        status.starts_with(b"HTTP/1.1 200") || status.starts_with(b"HTTP/1.0 200"),
        "comment refused: {}",
        String::from_utf8_lossy(status).trim()
    );
    Ok(())
}

/// The driver's step: with a channel and a final message, post it once and say so;
/// never fails the attempt (the summary still carries the result).
pub fn post_from_stream(
    on: bool,
    channel: &Option<std::path::PathBuf>,
    attempt: Uuid,
    stream: &[u8],
) -> bool {
    let (true, Some(root), Some(message)) = (on, channel, crate::final_text(stream)) else {
        return false;
    };
    match post(root, attempt, &message) {
        Ok(()) => {
            println!("aq-sandbox-harness: final message posted as a task comment");
            true
        }
        Err(e) => {
            println!("aq-sandbox-harness: final message not posted ({e:#}); summary only");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn the_comment_is_plain_bounded_text() {
        let attempt = Uuid::from_u128(7);
        let body = comment_body(attempt, "# Title\n```rust\nfn x() {}\n```\n**bold**\u{7}");
        assert!(body.starts_with(&format!("Final message of attempt {attempt}:\n\n```text\n")));
        assert!(body.contains("\n\u{200B}```rust\n") && body.contains("\n\u{200B}```\n"));
        assert!(body.contains("**bold**\n```\n") && !body.contains('\u{7}'));
        assert_eq!(
            body.matches("\n```").count(),
            2,
            "only the wrapper fences: {body}"
        );
        let long = comment_body(attempt, &"é".repeat(MAX_COMMENT_BYTES));
        assert!(
            long.len() <= MAX_COMMENT_BYTES && long.contains(CLIPPED),
            "{}",
            long.len()
        );
        assert_eq!(comment_operation(attempt), comment_operation(attempt));
        assert_ne!(
            comment_operation(attempt),
            comment_operation(Uuid::from_u128(8))
        );
    }
    #[test]
    fn post_sends_one_idempotent_comment_through_the_channel() {
        let dir = tempfile::tempdir().unwrap();
        let attempt = Uuid::from_u128(9);
        std::fs::create_dir(dir.path().join(attempt.to_string())).unwrap();
        let socket = dir
            .path()
            .join(attempt.to_string())
            .join(aq_agent_channel::SOCKET_NAME);
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for status in ["200 OK", "200 OK", "403 Forbidden"] {
                let (mut s, _) = listener.accept().unwrap();
                let mut buf = vec![0u8; 64 * 1024];
                let mut got = Vec::new();
                loop {
                    let n = s.read(&mut buf).unwrap();
                    got.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&got).to_string();
                    if let Some((head, body)) = text.split_once("\r\n\r\n") {
                        let len: usize = head
                            .lines()
                            .find_map(|l| l.strip_prefix("Content-Length: "))
                            .unwrap()
                            .parse()
                            .unwrap();
                        if body.len() >= len {
                            seen.push(text);
                            break;
                        }
                    }
                }
                write!(s, "HTTP/1.1 {status}\r\nContent-Length: 0\r\n\r\n").unwrap();
            }
            seen
        });
        post(dir.path(), attempt, "The answer.").unwrap();
        post(dir.path(), attempt, "The answer.").unwrap();
        assert!(post(dir.path(), attempt, "The answer.").is_err());
        let seen = server.join().unwrap();
        assert!(seen[0].starts_with("POST /v1/agent/comments HTTP/1.1\r\n"));
        let body = |s: &str| -> serde_json::Value {
            serde_json::from_str(s.split_once("\r\n\r\n").unwrap().1).unwrap()
        };
        let (a, b) = (body(&seen[0]), body(&seen[1]));
        assert_eq!(a, b, "a retry replays the same operation and body");
        assert_eq!(
            a["operation_id"],
            serde_json::json!(comment_operation(attempt))
        );
        assert_eq!(a["api_version"], 1);
        assert!(a["body"].as_str().unwrap().contains("The answer."));
    }
}
