use super::*;

pub(super) struct Relays {
    pub enrollments: BTreeMap<Uuid, PathBuf>,
    pub tasks: BTreeMap<Uuid, JoinHandle<()>>,
}

impl Relays {
    pub fn start_attempts(&mut self, runtime: &Path, tmux: &Path) {
        for attempt in attempts(runtime) {
            let Some(enrollment) = self.enrollments.get(&attempt.node) else {
                continue;
            };
            if self.tasks.contains_key(&attempt.id) || self.tasks.len() >= 32 {
                continue;
            }
            let id = attempt.id;
            let task = relay_attempt(attempt, tmux.to_owned(), enrollment.clone());
            self.tasks.insert(id, tokio::spawn(task));
        }
    }

    pub fn start_manual(&mut self, root: &Path, executable: &Path, socket: &Path) -> Result<()> {
        for entry in std::fs::read_dir(root).into_iter().flatten() {
            let state = entry?.path().join("state.json");
            let Some(local) = Local::running(&state) else {
                continue;
            };
            if self.tasks.contains_key(&local.id) || local.sandbox.is_none() {
                continue;
            }
            let Some(enrollment) = self.enrollments.get(&local.node_id) else {
                continue;
            };
            if self.tasks.len() >= 32 {
                break;
            }
            let relay = ManualRelay {
                id: local.id,
                state,
                executable: executable.to_owned(),
                socket: socket.to_owned(),
                enrollment: enrollment.clone(),
            };
            self.tasks.insert(local.id, tokio::spawn(relay.serve()));
        }
        Ok(())
    }
}

fn link(enrollment: &Path) -> Result<ServerLink> {
    ServerLink::from_enrollment(&EnrollmentFile::load(enrollment)?)
}

async fn relay_attempt(attempt: Attempt, tmux: PathBuf, enrollment: PathBuf) {
    while attempt.live() {
        if let Ok(link) = link(&enrollment)
            && let Ok(socket) = link.attempt_terminal_stream(attempt.id).await
        {
            let _ = relay(
                socket,
                attempt.id,
                Attachment::Attempt {
                    attempt: &attempt,
                    tmux: &tmux,
                },
            )
            .await;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

impl Local {
    fn running(state: &Path) -> Option<Self> {
        let text = aq_handoff_note::read_private(state, 65536).ok()?;
        let local: Self = serde_json::from_slice(&text).ok()?;
        (local.status == "running" && local.desired_running).then_some(local)
    }

    fn owned_sandbox(self) -> Option<SandboxIdentity> {
        let sandbox = self.sandbox?;
        let owner = &sandbox.owner;
        if (owner.node_id, owner.attempt_id, owner.boot_id) != (self.node_id, self.id, self.id) {
            return None;
        }
        (sandbox.backend_id == owner.deterministic_name()).then_some(sandbox)
    }
}

struct ManualRelay {
    id: Uuid,
    state: PathBuf,
    executable: PathBuf,
    socket: PathBuf,
    enrollment: PathBuf,
}

impl ManualRelay {
    async fn serve(self) {
        while let Some(sandbox) = Local::running(&self.state).and_then(Local::owned_sandbox) {
            if let Ok(link) = link(&self.enrollment)
                && let Ok(socket) = link.terminal_stream(self.id).await
            {
                let target = Attachment::Manual {
                    executable: &self.executable,
                    socket: &self.socket,
                    container: &sandbox.backend_id,
                };
                let _ = relay(socket, self.id, target).await;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
}
