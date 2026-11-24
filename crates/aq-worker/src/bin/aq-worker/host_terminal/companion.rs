use super::*;
use std::path::PathBuf;
struct EnrolledNode<'a> {
    link: &'a ServerLink,
    id: Uuid,
    enrollment: &'a Path,
}
struct Companion {
    exe: PathBuf,
    socket: PathBuf,
    root: PathBuf,
    layout: Layout,
    tasks: BTreeMap<Uuid, JoinHandle<()>>,
    last: BTreeMap<Uuid, Instant>,
}
impl Companion {
    fn load(path: &Path) -> Result<(Self, u32, std::fs::File)> {
        let config = ServiceConfig::load(path)?;
        let layout =
            Layout::for_service(path.parent().context("configuration directory")?, &config);
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
        Ok((
            Self {
                exe,
                socket,
                root,
                layout,
                tasks: BTreeMap::new(),
                last: BTreeMap::new(),
            },
            config.nodes,
            lock,
        ))
    }
    async fn session(&self, id: Uuid, action: &str) -> Result<bool> {
        tmux(
            &self.exe,
            &self.socket,
            &[action, "-t", &format!("={}", name(id))],
        )
        .await
    }
    async fn end(&mut self, id: Uuid) {
        if let Some(task) = self.tasks.remove(&id) {
            task.abort();
        }
        let _ = self.session(id, "kill-session").await;
    }
    async fn poll(&mut self, slot: u32) -> Result<()> {
        let enrollment = self.layout.slot_enrollment(slot);
        let Ok(file) = EnrollmentFile::load(&enrollment) else {
            return Ok(());
        };
        let Ok(link) = ServerLink::from_enrollment(&file) else {
            return Ok(());
        };
        let Ok(response) = link.host_terminal_poll(&json!({})).await else {
            return Ok(());
        };
        self.last.insert(file.node_id, Instant::now());
        let node = EnrolledNode {
            link: &link,
            id: file.node_id,
            enrollment: &enrollment,
        };
        if let Some(shells) = response["shells"].as_array().filter(|s| s.len() <= 8) {
            for item in shells {
                if let Ok(id) = serde_json::from_value::<Uuid>(item["id"].clone())
                    && !id.is_nil()
                {
                    self.update(&node, id, item["desired_running"] == true)
                        .await?;
                }
            }
        }
        Ok(())
    }
    async fn update(&mut self, node: &EnrolledNode<'_>, id: Uuid, desired: bool) -> Result<()> {
        let exists = self.session(id, "has-session").await.unwrap_or(false);
        if !desired {
            self.end(id).await;
            let _ = node
                .link
                .host_terminal_poll(&json!({"id":id,"status":"ended"}))
                .await;
            return Ok(());
        }
        let running = self.ensure_running(node.id, id, exists).await?;
        let status = if running { "running" } else { "ended" };
        if node
            .link
            .host_terminal_poll(&json!({"id":id,"status":status}))
            .await
            .is_ok()
            && running
        {
            self.attach(id, node.enrollment);
        }
        Ok(())
    }
    async fn ensure_running(&self, node: Uuid, id: Uuid, exists: bool) -> Result<bool> {
        if exists {
            return Ok(true);
        }
        let marker = self.root.join(id.to_string());
        if marker.exists() {
            return Ok(false);
        }
        // Mark before spawn: a crash can fail a shell but cannot silently respawn it.
        std::fs::write(marker, node.to_string())?;
        Ok(start(&self.exe, &self.socket, id).await.is_ok())
    }
    fn attach(&mut self, id: Uuid, enrollment: &Path) {
        if self.tasks.get(&id).is_some_and(|task| !task.is_finished()) {
            return;
        }
        let (exe, socket, enrollment) =
            (self.exe.clone(), self.socket.clone(), enrollment.to_owned());
        self.tasks.insert(
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
    async fn expire(&mut self) -> Result<()> {
        // Only private host-shell sessions are considered, never an agent socket.
        for entry in std::fs::read_dir(&self.root)? {
            let entry = entry?;
            if let Some((id, node)) = marker_ids(&entry)
                && self
                    .last
                    .get(&node)
                    .is_none_or(|seen| seen.elapsed() > Duration::from_secs(60))
            {
                self.end(id).await;
            }
        }
        Ok(())
    }
}
fn marker_ids(entry: &std::fs::DirEntry) -> Option<(Uuid, Uuid)> {
    let id = entry.file_name().to_string_lossy().parse().ok()?;
    let node = std::fs::read_to_string(entry.path()).ok()?.parse().ok()?;
    Some((id, node))
}
pub(crate) async fn run(path: &Path) -> Result<()> {
    let (mut host, nodes, lock) = Companion::load(path)?;
    let mut tick = tokio::time::interval(Duration::from_secs(2));
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    loop {
        tokio::select! {_=terminate.recv()=>break,_=tokio::signal::ctrl_c()=>break,_=tick.tick()=>{}}

        for slot in 1..=nodes {
            host.poll(slot).await?;
        }
        host.expire().await?;
    }
    for (_, task) in host.tasks {
        task.abort();
    }
    let _ = tmux(&host.exe, &host.socket, &["kill-server"]).await;
    drop(lock);
    Ok(())
}
