//! Streaming tmux attach companion: owns disposable PTYs, never agent lifetime.
mod pty;
use anyhow::{Context, Result, ensure};
use aq_node_service::{
    config::ServiceConfig, enrollment_file::EnrollmentFile, layout::Layout, link::ServerLink,
};
use aq_worker::{
    backend::{SandboxIdentity, docker_local::DockerLocalConfig},
    native::runner::ExecutorConfig,
};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path, time::Duration};
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
    let manual = config
        .manual
        .as_ref()
        .context("manual worker configuration absent")?;
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
    for slot in 1..=config.slots {
        let p = layout.slot_enrollment(slot);
        let e = EnrollmentFile::load(&p)?;
        enrollments.insert(e.node_id, p);
    }
    let root = config.state_root.join("manual-agents");
    let mut tasks = BTreeMap::<Uuid, JoinHandle<()>>::new();
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    loop {
        tokio::select! {_=terminate.recv()=>break,_=tokio::signal::ctrl_c()=>break,_=interval.tick()=>{
                tasks.retain(|_,task|!task.is_finished());
                for entry in std::fs::read_dir(&root)? {let state=entry?.path().join("state.json");let Ok(text)=aq_handoff_note::read_private(&state, 65536) else{continue};let Ok(local)=serde_json::from_slice::<Local>(&text) else{continue};if tasks.contains_key(&local.id)||local.status!="running"||!local.desired_running||local.sandbox.is_none(){continue;}
                 let Some(enrollment)=enrollments.get(&local.node_id) else{continue};if tasks.len()>=32{break;}
                 let (id,exe,docker,enrollment)=(local.id,manual.podman_executable.clone(),settings.host.docker_local.clone(),enrollment.clone());
                 tasks.insert(id,tokio::spawn(async move{loop{let Ok(text)=aq_handoff_note::read_private(&state, 65536) else{break};let Ok(local)=serde_json::from_slice::<Local>(&text) else{break};if local.status!="running"||!local.desired_running{break;}let Some(sandbox)=local.sandbox else{break};if sandbox.owner.node_id!=local.node_id||sandbox.owner.attempt_id!=local.id||sandbox.owner.boot_id!=local.id||sandbox.backend_id!=sandbox.owner.deterministic_name(){break;}
        if let Ok(file)=EnrollmentFile::load(&enrollment)&&let Ok(link)=ServerLink::from_enrollment(&file){let _=relay(&link,id,&exe,&docker,&sandbox.backend_id).await;}tokio::time::sleep(Duration::from_secs(1)).await;}}));
                }
               }}
    }
    for (_, task) in tasks {
        task.abort();
    }
    drop(lock);
    Ok(())
}
async fn relay(
    link: &ServerLink,
    id: Uuid,
    exe: &Path,
    docker: &DockerLocalConfig,
    container: &str,
) -> Result<()> {
    let socket = link.terminal_stream(id).await?;
    tracing::info!(session_id=%id,"manual terminal relay connected");
    let (mut sink, mut source) = socket.split();
    let mut attach: Option<(Uuid, u64, pty::Pty)> = None;
    loop {
        tokio::select! {
         frame=source.next()=>{let text=match frame{Some(Ok(Message::Text(text)))=>text,Some(Ok(Message::Ping(_)))=>{sink.flush().await?;continue},Some(Ok(Message::Pong(_)))=>continue,_=>break};let v:Value=serde_json::from_str(&text)?;let connection:Uuid=serde_json::from_value(v["connection_id"].clone())?;let generation=v["generation"].as_u64().context("terminal generation")?;
          if v["kind"]=="attach"{let (c,r)=size(&v)?;drop(attach.take());let p=pty::Pty::attach(exe,&docker.socket,container,c,r)?;attach=Some((connection,generation,p));sink.send(Message::Text(json!({"kind":"ready","connection_id":connection}).to_string().into())).await?;}
          else if let Some((current,g,p))=&attach && *current==connection&&*g==generation{
           if v["kind"]=="input"{let bytes=v["bytes"].as_array().context("terminal bytes")?;ensure!(bytes.len()<=4096,"terminal input bound");let bytes=bytes.iter().map(|b|b.as_u64().filter(|n|*n<=255).map(|n|n as u8).context("terminal byte")).collect::<Result<Vec<_>>>()?;p.write(&bytes).await?;}
           else if v["kind"]=="resize"{let(c,r)=size(&v)?;p.resize(c,r)?;}
           else if v["kind"]=="detach"{attach=None;}
          }
         }
         output=async{match &attach{Some((connection,_,p))=>p.read().await.map(|bytes|(*connection,bytes)),None=>std::future::pending().await}}=>{let(connection,bytes)=output?;let mut data=connection.as_bytes().to_vec();data.extend_from_slice(&bytes);tokio::time::timeout(Duration::from_secs(2),sink.send(Message::Binary(data.into()))).await??;}
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
