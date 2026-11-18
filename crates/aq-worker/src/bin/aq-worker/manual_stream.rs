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
                            let _=relay(socket,attempt.id,|c,r|pty::Pty::command(attempt.viewer(&tmux),c,r)).await;}
                        tokio::time::sleep(Duration::from_secs(1)).await;}}));
                }
                let Some(manual)=manual else{continue};
                for entry in std::fs::read_dir(&root).into_iter().flatten() {let state=entry?.path().join("state.json");let Ok(text)=aq_handoff_note::read_private(&state, 65536) else{continue};let Ok(local)=serde_json::from_slice::<Local>(&text) else{continue};if tasks.contains_key(&local.id)||local.status!="running"||!local.desired_running||local.sandbox.is_none(){continue;}
                 let Some(enrollment)=enrollments.get(&local.node_id) else{continue};if tasks.len()>=32{break;}
                 let (id,exe,docker,enrollment)=(local.id,manual.podman_executable.clone(),settings.host.docker_local.clone(),enrollment.clone());
                 tasks.insert(id,tokio::spawn(async move{loop{let Ok(text)=aq_handoff_note::read_private(&state, 65536) else{break};let Ok(local)=serde_json::from_slice::<Local>(&text) else{break};if local.status!="running"||!local.desired_running{break;}let Some(sandbox)=local.sandbox else{break};if sandbox.owner.node_id!=local.node_id||sandbox.owner.attempt_id!=local.id||sandbox.owner.boot_id!=local.id||sandbox.backend_id!=sandbox.owner.deterministic_name(){break;}
        if let Ok(file)=EnrollmentFile::load(&enrollment)&&let Ok(link)=ServerLink::from_enrollment(&file)&&let Ok(socket)=link.terminal_stream(id).await{let _=relay(socket,id,|c,r|pty::Pty::attach(&exe,&docker.socket,&sandbox.backend_id,c,r)).await;}tokio::time::sleep(Duration::from_secs(1)).await;}}));
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
    /// A tmux client on the attempt's own session; closing it detaches only this viewer.
    fn viewer(&self, tmux: &Path) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(tmux);
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .args(["-N", "-u", "-S"])
            .arg(self.host.join("tmux.sock"))
            .args(["-T", "RGB", "attach-session", "-t", "aq"]);
        command
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
async fn relay(
    socket: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    id: Uuid,
    attach_pty: impl Fn(u16, u16) -> Result<pty::Pty>,
) -> Result<()> {
    tracing::info!(session_id=%id,"manual terminal relay connected");
    let (mut sink, mut source) = socket.split();
    let mut attach: Option<(Uuid, u64, pty::Pty)> = None;
    let mut redactor = redact::Redactor::default();
    loop {
        tokio::select! {
         frame=source.next()=>{let text=match frame{Some(Ok(Message::Text(text)))=>text,Some(Ok(Message::Ping(_)))=>{sink.flush().await?;continue},Some(Ok(Message::Pong(_)))=>continue,_=>break};let v:Value=serde_json::from_str(&text)?;let connection:Uuid=serde_json::from_value(v["connection_id"].clone())?;let generation=v["generation"].as_u64().context("terminal generation")?;
          if v["kind"]=="attach"{let (c,r)=size(&v)?;drop(attach.take());redactor=redact::Redactor::default();let p=attach_pty(c,r)?;attach=Some((connection,generation,p));sink.send(Message::Text(json!({"kind":"ready","connection_id":connection}).to_string().into())).await?;}
          else if let Some((current,g,p))=&attach && *current==connection&&*g==generation{
           if v["kind"]=="input"{let bytes=v["bytes"].as_array().context("terminal bytes")?;ensure!(bytes.len()<=4096,"terminal input bound");let bytes=bytes.iter().map(|b|b.as_u64().filter(|n|*n<=255).map(|n|n as u8).context("terminal byte")).collect::<Result<Vec<_>>>()?;p.write(&bytes).await?;}
           else if v["kind"]=="resize"{let(c,r)=size(&v)?;p.resize(c,r)?;}
           else if v["kind"]=="detach"{attach=None;}
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
        let viewer = found[0].viewer(Path::new("/usr/bin/tmux"));
        let args: Vec<_> = viewer.as_std().get_args().map(|a| a.to_owned()).collect();
        assert_eq!(args[3], host.join("launch/tmux.sock"));
        assert_eq!(&args[args.len() - 3..], ["attach-session", "-t", "aq"]);
        assert!(!args.iter().any(|a| a == "read-only"));
        std::fs::write(host.join("driver-launch/exit.json"), b"{}").unwrap();
        assert!(
            attempts(root.path()).is_empty(),
            "a finished driver is not served"
        );
    }
}
