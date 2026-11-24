//! Separate service-user shells; no sandbox or agent lifecycle calls.
mod companion;
mod relay;
use crate::manual_stream::pty::Pty;
use anyhow::{Context, Result, ensure};
use aq_node_service::{
    config::ServiceConfig, enrollment_file::EnrollmentFile, layout::Layout, link::ServerLink,
};
pub(crate) use companion::run;
use futures_util::{SinkExt, StreamExt};
use relay::relay;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::Path,
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::{process::Command, task::JoinHandle};
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

fn clean_command(exe: &Path) -> Command {
    let mut command = Command::new(exe);
    command.env_clear();
    for key in [
        "HOME", "USER", "LOGNAME", "PATH", "LANG", "LC_ALL", "LC_CTYPE", "TZ",
    ] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    command
        .env("TERM", "xterm-256color")
        .env("COLORTERM", "truecolor");
    command
}
async fn tmux(exe: &Path, socket: &Path, args: &[&str]) -> Result<bool> {
    let status = tokio::time::timeout(
        Duration::from_secs(5),
        clean_command(exe)
            .arg("-u")
            .arg("-S")
            .arg(socket)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status(),
    )
    .await??;
    Ok(status.success())
}
fn name(id: Uuid) -> String {
    format!("host-{id}")
}
async fn start(exe: &Path, socket: &Path, id: Uuid) -> Result<()> {
    let home = std::env::var_os("HOME").context("host home absent")?;
    // Use the account's configured shell, never a command supplied by the server.
    let shell = unsafe { libc::getpwuid(libc::getuid()) };
    let shell = if shell.is_null() {
        "/bin/bash".to_owned()
    } else {
        unsafe { std::ffi::CStr::from_ptr((*shell).pw_shell) }
            .to_string_lossy()
            .into_owned()
    };
    ensure!(
        shell.starts_with('/') && !shell.contains('\''),
        "invalid login shell"
    );
    let command = format!("exec '{shell}' -l");
    let status = clean_command(exe)
        .arg("-u")
        .arg("-S")
        .arg(socket)
        .args([
            "new-session",
            "-d",
            "-s",
            &name(id),
            "-x",
            "120",
            "-y",
            "32",
            "-c",
        ])
        .arg(home)
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await?;
    ensure!(status.success(), "host shell could not start");
    Ok(())
}
fn size(v: &Value) -> Result<(u16, u16)> {
    let c = v["columns"].as_u64().context("columns")?;
    let r = v["rows"].as_u64().context("rows")?;
    ensure!(
        (2..=500).contains(&c) && (1..=300).contains(&r),
        "terminal dimensions"
    );
    Ok((c as u16, r as u16))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::process::Command::new("/usr/bin/tmux")
                .arg("-S")
                .arg(&self.0)
                .arg("kill-server")
                .output();
        }
    }
    async fn expect_output(p: &Pty, wanted: &[u8]) {
        let mut bytes = Vec::new();
        tokio::time::timeout(Duration::from_secs(8), async {
            while !bytes.windows(wanted.len()).any(|v| v == wanted) {
                bytes.extend(p.read().await.unwrap());
            }
        })
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn real_host_shell_survives_detach_resizes_and_ends_only_its_own_socket() {
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("h.sock");
        let exe = Path::new("/usr/bin/tmux");
        let id = Uuid::new_v4();
        let _cleanup = Cleanup(socket.clone());
        start(exe, &socket, id).await.unwrap();
        let attach = || {
            let mut c = clean_command(exe);
            c.arg("-u").arg("-S").arg(&socket).args([
                "attach-session",
                "-E",
                "-t",
                &format!("={}", name(id)),
            ]);
            Pty::command(c, 93, 33).unwrap()
        };
        let p = attach();
        p.write(b"printf '\\nHOST_PTY_%s\\n' READY\r")
            .await
            .unwrap();
        expect_output(&p, b"HOST_PTY_READY").await;
        p.resize(102, 38).unwrap();
        drop(p);
        assert!(
            tmux(
                exe,
                &socket,
                &["has-session", "-t", &format!("={}", name(id))]
            )
            .await
            .unwrap()
        );
        let p = attach();
        p.write(b"printf '\\nRESUMED_%s\\n' SHELL\r").await.unwrap();
        expect_output(&p, b"RESUMED_SHELL").await;
        drop(p);
        assert!(
            tmux(
                exe,
                &socket,
                &["kill-session", "-t", &format!("={}", name(id))]
            )
            .await
            .unwrap()
        );
        assert!(
            !tmux(
                exe,
                &socket,
                &["has-session", "-t", &format!("={}", name(id))]
            )
            .await
            .unwrap()
        );
    }
    #[test]
    fn host_commands_inherit_only_the_explicit_environment_allowlist() {
        let c = clean_command(Path::new("/bin/true"));
        let keys = c
            .as_std()
            .get_envs()
            .map(|(k, _)| k.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(keys.iter().all(|k| {
            [
                "HOME",
                "USER",
                "LOGNAME",
                "PATH",
                "LANG",
                "LC_ALL",
                "LC_CTYPE",
                "TZ",
                "TERM",
                "COLORTERM",
            ]
            .contains(&k.as_str())
        }));
        assert!(size(&json!({"columns":501,"rows":24})).is_err());
    }
}
