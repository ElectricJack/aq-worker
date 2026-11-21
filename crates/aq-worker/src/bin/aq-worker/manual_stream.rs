//! Streaming tmux attach companion: owns disposable PTYs, never agent lifetime.
//! Hand-started agents are attached through their sandbox's own tmux; queued
//! (sandboxed) attempts through the attempt's session-host tmux, whose pane shows
//! the harness TUI that the node driver relays from the sandbox.
pub(crate) mod pty;
use anyhow::{Context, Result, ensure};
use aq_node_service::{
    config::ServiceConfig, enrollment_file::EnrollmentFile, layout::Layout, link::ServerLink,
};
use aq_sandbox_archive::redact;
use aq_session_protocol::complete_terminal_input;
use aq_worker::{backend::SandboxIdentity, native::runner::ExecutorConfig};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;
#[derive(Deserialize)]
struct Local {
    id: Uuid,
    node_id: Uuid,
    status: String,
    desired_running: bool,
    sandbox: Option<SandboxIdentity>,
}
pub async fn run(path: &Path) -> Result<()> {
    let config = ServiceConfig::load(path)?;
    let manual = config.manual.as_ref();
    let layout = Layout::for_service(
        path.parent().context("service configuration directory")?,
        &config,
    );
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(config.state_root.join("manual-terminal-relay.lock"))?;
    fs2::FileExt::try_lock_exclusive(&lock).context("terminal relay already running")?;
    let native: aq_worker::native::runner::NativeConfig =
        serde_json::from_slice(&std::fs::read(&config.native_template)?)?;
    let Some(ExecutorConfig::ClaudeDockerLocalResultLocal { settings, .. }) = native.executor
    else {
        anyhow::bail!("installed local sandbox absent")
    };
    let mut enrollments = BTreeMap::new();
    for slot in 1..=config.nodes {
        let p = layout.slot_enrollment(slot);
        let e = EnrollmentFile::load(&p)?;
        enrollments.insert(e.node_id, p);
    }
    let root = config.state_root.join("manual-agents");
    let mut tasks = BTreeMap::<Uuid, JoinHandle<()>>::new();
    let tmux = settings.host.tmux.path.clone();
    let runtime = config.runtime_root();
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    loop {
        tokio::select! {_=terminate.recv()=>break,_=tokio::signal::ctrl_c()=>break,_=interval.tick()=>{
                tasks.retain(|_,task|!task.is_finished());
                for attempt in attempts(&runtime) {
                    let Some(enrollment)=enrollments.get(&attempt.node) else{continue};
                    if tasks.contains_key(&attempt.id)||tasks.len()>=32{continue;}
                    let (tmux,enrollment)=(tmux.clone(),enrollment.clone());
                    tasks.insert(attempt.id,tokio::spawn(async move{while attempt.live(){
                        if let Ok(file)=EnrollmentFile::load(&enrollment)&&let Ok(link)=ServerLink::from_enrollment(&file)&&let Ok(socket)=link.attempt_terminal_stream(attempt.id).await{
                            let _=relay(socket,attempt.id,Attachment::Attempt { attempt: &attempt, tmux: &tmux }).await;}
                        tokio::time::sleep(Duration::from_secs(1)).await;}}));
                }
                let Some(manual)=manual else{continue};
                for entry in std::fs::read_dir(&root).into_iter().flatten() {let state=entry?.path().join("state.json");let Ok(text)=aq_handoff_note::read_private(&state, 65536) else{continue};let Ok(local)=serde_json::from_slice::<Local>(&text) else{continue};if tasks.contains_key(&local.id)||local.status!="running"||!local.desired_running||local.sandbox.is_none(){continue;}
                 let Some(enrollment)=enrollments.get(&local.node_id) else{continue};if tasks.len()>=32{break;}
                 let (id,exe,docker,enrollment)=(local.id,manual.podman_executable.clone(),settings.host.docker_local.clone(),enrollment.clone());
                 tasks.insert(id,tokio::spawn(async move{loop{let Ok(text)=aq_handoff_note::read_private(&state, 65536) else{break};let Ok(local)=serde_json::from_slice::<Local>(&text) else{break};if local.status!="running"||!local.desired_running{break;}let Some(sandbox)=local.sandbox else{break};if sandbox.owner.node_id!=local.node_id||sandbox.owner.attempt_id!=local.id||sandbox.owner.boot_id!=local.id||sandbox.backend_id!=sandbox.owner.deterministic_name(){break;}
        if let Ok(file)=EnrollmentFile::load(&enrollment)&&let Ok(link)=ServerLink::from_enrollment(&file)&&let Ok(socket)=link.terminal_stream(id).await{let _=relay(socket,id,Attachment::Manual {
                executable: &exe, socket: &docker.socket, container: &sandbox.backend_id,
            }).await;}tokio::time::sleep(Duration::from_secs(1)).await;}}));
                }
               }}
    }
    for (_, task) in tasks {
        task.abort();
    }
    drop(lock);
    Ok(())
}
/// A running queued attempt on this machine: its session-host directory holds the
/// tmux socket, and the driver directory beside it names the attempt.
struct Attempt {
    id: Uuid,
    node: Uuid,
    host: PathBuf,
    driver: PathBuf,
}
impl Attempt {
    /// The session host is up and the driver has not finished.
    fn live(&self) -> bool {
        self.host.join("tmux.sock").exists() && !self.driver.join("exit.json").exists()
    }
    fn tmux(&self, tmux: &Path) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(tmux);
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .args(["-N", "-u", "-S"])
            .arg(self.host.join("tmux.sock"))
            .kill_on_drop(true);
        command
    }
    /// A fresh attach has tmux draw the current pane on a disposable, fixed-size PTY.
    fn viewer(&self, tmux: &Path, read_only: bool) -> tokio::process::Command {
        let mut command = self.tmux(tmux);
        command.args([
            "-T",
            "RGB",
            "attach-session",
            "-E",
            "-f",
            if read_only {
                "ignore-size,read-only"
            } else {
                "ignore-size"
            },
            "-t",
            "aq",
        ]);
        command
    }
    /// The real pane's current dimensions, as tmux reports them.
    async fn pane_size(&self, tmux: &Path) -> Result<(u16, u16)> {
        let output = tokio::time::timeout(
            Duration::from_secs(2),
            self.tmux(tmux)
                .args([
                    "display-message",
                    "-p",
                    "-t",
                    "aq",
                    "#{pane_width} #{pane_height}",
                ])
                .output(),
        )
        .await??;
        ensure!(
            output.status.success(),
            "attempt pane dimensions unavailable"
        );
        let (columns, rows) = std::str::from_utf8(&output.stdout)?
            .trim()
            .split_once(' ')
            .context("attempt pane dimensions missing")?;
        Ok((columns.parse()?, rows.parse()?))
    }
    async fn attach(&self, tmux: &Path, read_only: bool) -> Result<(pty::Pty, (u16, u16))> {
        let (columns, rows) = self.pane_size(tmux).await?;
        Ok((
            pty::Pty::command(self.viewer(tmux, read_only), columns, rows)?,
            (columns, rows),
        ))
    }
    /// Resize the shared pane itself (viewers attach with `ignore-size`, so only this
    /// call changes it), then confirm what tmux reports.
    async fn resize_pane(&self, tmux: &Path, columns: u16, rows: u16) -> Result<(u16, u16)> {
        let output = tokio::time::timeout(
            Duration::from_secs(2),
            self.tmux(tmux)
                .args(["resize-window", "-t", "aq", "-x"])
                .arg(columns.to_string())
                .arg("-y")
                .arg(rows.to_string())
                .output(),
        )
        .await??;
        ensure!(output.status.success(), "attempt pane resize failed");
        let confirmed = self.pane_size(tmux).await?;
        ensure!(
            confirmed == (columns, rows),
            "attempt pane dimensions not applied"
        );
        Ok(confirmed)
    }
}
/// Every live attempt under the service's runtime root (`<slot>/host/driver-<launch>`).
fn attempts(runtime: &Path) -> Vec<Attempt> {
    let dirs = |path: &Path| {
        std::fs::read_dir(path)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
    };
    let mut found = Vec::new();
    for host in dirs(runtime).map(|slot| slot.join("host")) {
        for driver in dirs(&host) {
            let launch = driver.file_name().and_then(|name| name.to_str());
            let Some(launch) = launch.and_then(|name| name.strip_prefix("driver-")) else {
                continue;
            };
            let Ok(text) = aq_handoff_note::read_private(&driver.join("driver.json"), 1 << 20)
            else {
                continue;
            };
            let Ok(config) = serde_json::from_slice::<Value>(&text) else {
                continue;
            };
            let id = |name: &str| serde_json::from_value::<Uuid>(config["owner"][name].clone());
            if let (Ok(id), Ok(node)) = (id("attempt_id"), id("node_id")) {
                let attempt = Attempt {
                    id,
                    node,
                    host: host.join(launch),
                    driver,
                };
                if attempt.live() {
                    found.push(attempt);
                }
            }
        }
    }
    found
}
enum Attachment<'a> {
    Attempt {
        attempt: &'a Attempt,
        tmux: &'a Path,
    },
    Manual {
        executable: &'a Path,
        socket: &'a Path,
        container: &'a str,
    },
}
impl Attachment<'_> {
    async fn open(
        &self,
        size: Option<(u16, u16)>,
        read_only: bool,
    ) -> Result<(pty::Pty, (u16, u16))> {
        match self {
            Self::Attempt { attempt, tmux } => attempt.attach(tmux, read_only).await,
            Self::Manual {
                executable,
                socket,
                container,
            } => {
                let (columns, rows) = size.context("manual terminal size")?;
                let pty = pty::Pty::attach(executable, socket, container, columns, rows)?;
                Ok((pty, (columns, rows)))
            }
        }
    }
}
async fn relay(
    socket: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    id: Uuid,
    target: Attachment<'_>,
) -> Result<()> {
    tracing::info!(session_id=%id,"manual terminal relay connected");
    let fixed = matches!(target, Attachment::Attempt { .. });
    let (mut sink, mut source) = socket.split();
    let mut attach: Option<(Uuid, u64, pty::Pty)> = None;
    let mut redactor = redact::Redactor::default();
    let mut read_only = false;
    loop {
        tokio::select! {
         frame=source.next()=>{let text=match frame{Some(Ok(Message::Text(text)))=>text,Some(Ok(Message::Ping(_)))=>{sink.flush().await?;continue},Some(Ok(Message::Pong(_)))=>continue,_=>break};let v:Value=serde_json::from_str(&text)?;let connection:Uuid=serde_json::from_value(v["connection_id"].clone())?;let generation=v["generation"].as_u64().context("terminal generation")?;
          let current = attach.as_ref().is_some_and(|(id, g, _)| *id == connection && *g == generation);
          match v["kind"].as_str() {
            Some("attach") | Some("repaint") if v["kind"] == "attach" || (fixed && current) => {
                let size = if fixed { None } else { Some(size(&v)?) };
                if fixed && v["kind"] == "attach" {
                    read_only = v["read_only"].as_bool().context("attempt viewer mode")?;
                }
                drop(attach.take());
                redactor = redact::Redactor::default();
                let (p, (columns, rows)) = target.open(size, read_only).await?;
                attach = Some((connection, generation, p));
                sink.send(Message::Text(json!({"kind":"ready","connection_id":connection,"columns":columns,"rows":rows}).to_string().into())).await?;
            }
            Some(kind) if current => {
                let p = &attach.as_ref().unwrap().2;
                match kind {
                    "input" if !read_only => {
                        let bytes = v["bytes"].as_array().context("terminal bytes")?;
                        ensure!(bytes.len() <= aq_session_protocol::MAX_INPUT_BYTES, "terminal input bound");
                        let bytes = bytes.iter().map(|b| b.as_u64().filter(|n| *n <= 255).map(|n| n as u8).context("terminal byte")).collect::<Result<Vec<_>>>()?;
                        ensure!(!fixed || complete_terminal_input(&bytes, false), "incomplete terminal input");
                        p.write(&bytes).await?;
                    }
                    "escape" if fixed && !read_only => p.write(b"\x1b").await?,
                    "resize" if !fixed => { let (c,r) = size(&v)?; p.resize(c,r)?; }
                    // Only the current writable controller may change the shared pane.
                    "resize" if !read_only => {
                        let (c, r) = size(&v)?;
                        let Attachment::Attempt { attempt, tmux } = &target else { continue };
                        let (c, r) = attempt.resize_pane(tmux, c, r).await?;
                        p.resize(c, r)?;
                    }
                    "detach" => attach = None,
                    _ => {}
                }
            }
            _ => {}
          }
         }
         output=async{match &attach{Some((connection,_,p))=>p.read().await.map(|bytes|(*connection,bytes)),None=>std::future::pending().await}}=>{let(connection,bytes)=output?;let mut data=connection.as_bytes().to_vec();data.extend_from_slice(&redactor.frame(&bytes));tokio::time::timeout(Duration::from_secs(2),sink.send(Message::Binary(data.into()))).await??;}
        }
    }
    Ok(())
}
fn size(v: &Value) -> Result<(u16, u16)> {
    let (c, r) = (
        v["columns"].as_u64().context("terminal columns")?,
        v["rows"].as_u64().context("terminal rows")?,
    );
    ensure!(
        (2..=500).contains(&c) && (1..=300).contains(&r),
        "terminal dimensions"
    );
    Ok((c as u16, r as u16))
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    /// A queued attempt is served while its session host's tmux socket exists and its
    /// driver has not finished; the viewer is a plain client of that one session.
    #[test]
    fn live_attempts_are_found_by_their_driver_and_session_host_directories() {
        let root = tempfile::tempdir().unwrap();
        let host = root.path().join("1-2/host");
        let (attempt, node) = (Uuid::from_u128(3), Uuid::from_u128(1));
        for dir in ["driver-launch", "launch", "driver-other"] {
            std::fs::create_dir_all(host.join(dir)).unwrap();
            let private = std::fs::Permissions::from_mode(0o700);
            std::fs::set_permissions(host.join(dir), private).unwrap();
        }
        let config = json!({"owner": {"node_id": node, "boot_id": Uuid::from_u128(2), "attempt_id": attempt}});
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(host.join("driver-launch/driver.json"))
            .and_then(|mut f| std::io::Write::write_all(&mut f, config.to_string().as_bytes()))
            .unwrap();
        assert!(attempts(root.path()).is_empty(), "no session host yet");
        std::fs::write(host.join("launch/tmux.sock"), b"").unwrap();
        let found = attempts(root.path());
        assert_eq!(found.len(), 1);
        assert_eq!((found[0].id, found[0].node), (attempt, node));
        let viewer = found[0].viewer(Path::new("/usr/bin/tmux"), false);
        let args: Vec<_> = viewer.as_std().get_args().map(|a| a.to_owned()).collect();
        assert_eq!(args[3], host.join("launch/tmux.sock"));
        assert_eq!(
            &args[args.len() - 6..],
            ["attach-session", "-E", "-f", "ignore-size", "-t", "aq"]
        );
        std::fs::write(host.join("driver-launch/exit.json"), b"{}").unwrap();
        assert!(
            attempts(root.path()).is_empty(),
            "a finished driver is not served"
        );
    }

    fn frame(kind: &str, connection: Uuid, generation: u64, extra: Value) -> Message {
        let mut v = json!({"kind": kind, "connection_id": connection, "generation": generation});
        v.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        Message::Text(v.to_string().into())
    }
    async fn pane(attempt: &Attempt) -> (u16, u16) {
        attempt.pane_size(Path::new("/usr/bin/tmux")).await.unwrap()
    }
    /// A real tmux pane: only the current writable controller changes its dimensions;
    /// stale connections or generations and watchers (attach, repaint, reconnect) never do.
    #[tokio::test]
    async fn managed_pane_resizes_only_for_its_current_writable_controller() {
        let tmux = Path::new("/usr/bin/tmux");
        let root = tempfile::tempdir().unwrap();
        let attempt = Attempt {
            id: Uuid::from_u128(3),
            node: Uuid::from_u128(1),
            host: root.path().to_path_buf(),
            driver: root.path().join("driver"),
        };
        // `Attempt::tmux` passes -N (never start a server), so start it directly.
        let started = tokio::process::Command::new(tmux)
            .arg("-S")
            .arg(root.path().join("tmux.sock"))
            .args([
                "new-session",
                "-d",
                "-s",
                "aq",
                "-x",
                "80",
                "-y",
                "24",
                "cat",
            ])
            .output()
            .await
            .unwrap();
        assert!(started.status.success());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let client = tokio::spawn(async move {
            let (socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();
            socket
        });
        let (stream, _) = listener.accept().await.unwrap();
        let mut server = tokio_tungstenite::accept_async(stream).await.unwrap();
        let socket = client.await.unwrap();
        let relay_task = {
            let (host, driver) = (attempt.host.clone(), attempt.driver.clone());
            tokio::spawn(async move {
                let attempt = Attempt {
                    id: Uuid::from_u128(3),
                    node: Uuid::from_u128(1),
                    host,
                    driver,
                };
                relay(
                    socket,
                    attempt.id,
                    Attachment::Attempt {
                        attempt: &attempt,
                        tmux: Path::new("/usr/bin/tmux"),
                    },
                )
                .await
            })
        };
        async fn ready(
            server: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
        ) -> Value {
            loop {
                let next = tokio::time::timeout(Duration::from_secs(5), server.next())
                    .await
                    .unwrap();
                if let Some(Ok(Message::Text(text))) = next {
                    return serde_json::from_str(&text).unwrap();
                }
            }
        }
        let (controller, watcher) = (Uuid::from_u128(10), Uuid::from_u128(11));
        let resize = |c, g, columns: u64, rows: u64| {
            frame("resize", c, g, json!({"columns": columns, "rows": rows}))
        };
        server
            .send(frame("attach", controller, 1, json!({"read_only": false})))
            .await
            .unwrap();
        let r = ready(&mut server).await;
        assert_eq!(
            (r["columns"].as_u64(), r["rows"].as_u64()),
            (Some(80), Some(24))
        );
        // Controller resize reaches the real pane.
        server.send(resize(controller, 1, 100, 30)).await.unwrap();
        // Stale generation and an unattached connection change nothing.
        server.send(resize(controller, 0, 120, 40)).await.unwrap();
        server.send(resize(watcher, 1, 130, 50)).await.unwrap();
        // A repaint is a no-op for the size; the ready frame follows the earlier resize.
        server
            .send(frame("repaint", controller, 1, json!({})))
            .await
            .unwrap();
        let r = ready(&mut server).await;
        assert_eq!(
            (r["columns"].as_u64(), r["rows"].as_u64()),
            (Some(100), Some(30))
        );
        assert_eq!(pane(&attempt).await, (100, 30));
        // A view-only watcher observes the pane unchanged and cannot resize it.
        server
            .send(frame("attach", watcher, 2, json!({"read_only": true})))
            .await
            .unwrap();
        let r = ready(&mut server).await;
        assert_eq!(
            (r["columns"].as_u64(), r["rows"].as_u64()),
            (Some(100), Some(30))
        );
        server.send(resize(watcher, 2, 60, 20)).await.unwrap();
        server
            .send(frame("repaint", watcher, 2, json!({})))
            .await
            .unwrap();
        let r = ready(&mut server).await;
        assert_eq!(
            (r["columns"].as_u64(), r["rows"].as_u64()),
            (Some(100), Some(30))
        );
        assert_eq!(pane(&attempt).await, (100, 30));
        // The old controller connection was replaced by the watcher: no resize either.
        server.send(resize(controller, 1, 70, 25)).await.unwrap();
        // Reconnect as controller: dimensions are confirmed from the real pane.
        server
            .send(frame("attach", controller, 3, json!({"read_only": false})))
            .await
            .unwrap();
        let r = ready(&mut server).await;
        assert_eq!(
            (r["columns"].as_u64(), r["rows"].as_u64()),
            (Some(100), Some(30))
        );
        server.send(resize(controller, 3, 90, 33)).await.unwrap();
        server
            .send(frame("repaint", controller, 3, json!({})))
            .await
            .unwrap();
        let r = ready(&mut server).await;
        assert_eq!(
            (r["columns"].as_u64(), r["rows"].as_u64()),
            (Some(90), Some(33))
        );
        assert_eq!(pane(&attempt).await, (90, 33));
        drop(server);
        let _ = relay_task.await;
        let _ = attempt.tmux(tmux).arg("kill-server").output().await;
    }
    #[test]
    fn terminal_dimension_bounds_are_two_to_five_hundred_by_one_to_three_hundred() {
        let ok = |c: u64, r: u64| size(&json!({"columns": c, "rows": r})).is_ok();
        assert!(ok(2, 1) && ok(500, 300));
        assert!(!ok(1, 1) && !ok(501, 300) && !ok(2, 0) && !ok(80, 301));
    }
}
