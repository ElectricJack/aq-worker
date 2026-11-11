//! Separate service-user shells; no sandbox or agent lifecycle calls.
use crate::manual_stream::pty::Pty;
use anyhow::{Context, Result, ensure};
use aq_node_service::{
    config::ServiceConfig, enrollment_file::EnrollmentFile, layout::Layout, link::ServerLink,
};
use futures_util::{SinkExt, StreamExt};
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
pub async fn run(path: &Path) -> Result<()> {
    let config = ServiceConfig::load(path)?;
    let layout = Layout::for_service(path.parent().context("configuration directory")?, &config);
    std::fs::create_dir_all(&layout.runtime_root)?;
    let root = config.state_root.join("host-shells");
    std::fs::create_dir_all(&root)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(root.join("companion.lock"))?;
    fs2::FileExt::try_lock_exclusive(&lock)?;
    let exe = std::env::current_exe()?
        .parent()
        .context("companion directory")?
        .join("tmux");
    ensure!(exe.is_file(), "bundled tmux absent");
    let socket = layout.runtime_root.join("host.sock");
    let mut tasks = BTreeMap::<Uuid, JoinHandle<()>>::new();
    let mut last = BTreeMap::<Uuid, Instant>::new();
    let mut tick = tokio::time::interval(Duration::from_secs(2));
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    loop {
        tokio::select! {_=terminate.recv()=>break,_=tokio::signal::ctrl_c()=>break,_=tick.tick()=>{}}
        for slot in 1..=config.slots {
            let Ok(file) = EnrollmentFile::load(&layout.slot_enrollment(slot)) else {
                continue;
            };
            let Ok(link) = ServerLink::from_enrollment(&file) else {
                continue;
            };
            let Ok(response) = link.host_terminal_poll(&json!({})).await else {
                continue;
            };
            last.insert(file.node_id, Instant::now());
            let Some(shells) = response["shells"].as_array().filter(|s| s.len() <= 8) else {
                continue;
            };
            for item in shells {
                let Ok(id) = serde_json::from_value::<Uuid>(item["id"].clone()) else {
                    continue;
                };
                if id.is_nil() {
                    continue;
                }
                let marker = root.join(id.to_string());
                let exists = tmux(
                    &exe,
                    &socket,
                    &["has-session", "-t", &format!("={}", name(id))],
                )
                .await
                .unwrap_or(false);
                if item["desired_running"] != true {
                    if let Some(task) = tasks.remove(&id) {
                        task.abort();
                    }
                    let _ = tmux(
                        &exe,
                        &socket,
                        &["kill-session", "-t", &format!("={}", name(id))],
                    )
                    .await;
                    let _ = link
                        .host_terminal_poll(&json!({"id":id,"status":"ended"}))
                        .await;
                    continue;
                }
                let running = if exists {
                    true
                } else if marker.exists() {
                    false
                } else {
                    // Mark before spawn: a crash can fail a shell but cannot silently respawn it.
                    std::fs::write(&marker, file.node_id.to_string())?;
                    start(&exe, &socket, id).await.is_ok()
                };
                if link
                    .host_terminal_poll(
                        &json!({"id":id,"status":if running{"running"}else{"ended"}}),
                    )
                    .await
                    .is_err()
                {
                    continue;
                }
                if running && tasks.get(&id).is_none_or(|t| t.is_finished()) {
                    let (exe, socket, enrollment) =
                        (exe.clone(), socket.clone(), layout.slot_enrollment(slot));
                    tasks.insert(
                        id,
                        tokio::spawn(async move {
                            if let Ok(file) = EnrollmentFile::load(&enrollment)
                                && let Ok(link) = ServerLink::from_enrollment(&file)
                            {
                                let _ = relay(&link, id, &exe, &socket).await;
                            }
                        }),
                    );
                }
            }
        }
        // Only private host-shell sessions are considered, never an agent socket.
        for entry in std::fs::read_dir(&root)? {
            let entry = entry?;
            let Ok(id) = entry.file_name().to_string_lossy().parse::<Uuid>() else {
                continue;
            };
            let Ok(node) = std::fs::read_to_string(entry.path())
                .unwrap_or_default()
                .parse::<Uuid>()
            else {
                continue;
            };
            if last
                .get(&node)
                .is_none_or(|seen| seen.elapsed() > Duration::from_secs(60))
            {
                if let Some(task) = tasks.remove(&id) {
                    task.abort();
                }
                let _ = tmux(
                    &exe,
                    &socket,
                    &["kill-session", "-t", &format!("={}", name(id))],
                )
                .await;
            }
        }
    }
    for (_, task) in tasks {
        task.abort();
    }
    let _ = tmux(&exe, &socket, &["kill-server"]).await;
    drop(lock);
    Ok(())
}
async fn relay(link: &ServerLink, id: Uuid, exe: &Path, socket: &Path) -> Result<()> {
    let socket_ws = link.host_terminal_stream(id).await?;
    let (mut sink, mut source) = socket_ws.split();
    let mut attach: Option<(Uuid, u64, Pty)> = None;
    loop {
        tokio::select! {
            frame=source.next()=>{
                let text=match frame{Some(Ok(Message::Text(text)))=>text,Some(Ok(Message::Ping(_)))=>{sink.flush().await?;continue},Some(Ok(Message::Pong(_)))=>continue,_=>break};
                let v:Value=serde_json::from_str(&text)?;
                let connection:Uuid=serde_json::from_value(v["connection_id"].clone())?;
                let generation=v["generation"].as_u64().context("generation")?;
                if v["kind"]=="attach" {
                    let(c,r)=size(&v)?;drop(attach.take());
                    let mut command=clean_command(exe);
                    command.arg("-u").arg("-S").arg(socket).args(["set-window-option","-t",&format!("={}",name(id)),"window-size","latest",";","attach-session","-E","-t",&format!("={}",name(id))]);
                    attach=Some((connection,generation,Pty::command(command,c,r)?));
                    sink.send(Message::Text(json!({"kind":"ready","connection_id":connection}).to_string().into())).await?;
                } else if let Some((current,g,p))=&attach&&*current==connection&&*g==generation {
                    if v["kind"]=="input" {
                        let bytes:Vec<u8>=serde_json::from_value(v["bytes"].clone())?;
                        ensure!(bytes.len()<=4096,"input bound");p.write(&bytes).await?;
                    }else if v["kind"]=="resize"{let(c,r)=size(&v)?;p.resize(c,r)?;}
                    else if v["kind"]=="detach"{attach=None;}
                }
            }
            output=async{match &attach{Some((c,_,p))=>p.read().await.map(|b|(*c,b)),None=>std::future::pending().await}}=>{
                let(c,bytes)=output?;let mut data=c.as_bytes().to_vec();data.extend(bytes);
                tokio::time::timeout(Duration::from_secs(2),sink.send(Message::Binary(data.into()))).await??;
            }
        }
    }
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
    #[tokio::test]
    async fn real_host_shell_survives_detach_resizes_and_ends_only_its_own_socket() {
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("h.sock");
        let exe = Path::new("/usr/bin/tmux");
        let id = Uuid::new_v4();
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
        let mut bytes = Vec::new();
        tokio::time::timeout(Duration::from_secs(8), async {
            while !bytes.windows(14).any(|v| v == b"HOST_PTY_READY") {
                bytes.extend(p.read().await.unwrap());
            }
        })
        .await
        .unwrap();
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
        let mut bytes = Vec::new();
        tokio::time::timeout(Duration::from_secs(8), async {
            while !bytes.windows(13).any(|v| v == b"RESUMED_SHELL") {
                bytes.extend(p.read().await.unwrap());
            }
        })
        .await
        .unwrap();
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
