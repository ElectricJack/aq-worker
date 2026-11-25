use super::*;
pub(super) struct Drive<'a, T: DockerTransport> {
    pub(super) backend: &'a mut DockerLocalBackend<T>,
    pub(super) config: &'a DriverConfig,
    pub(super) sandbox: &'a SandboxIdentity,
    pub(super) container_id: &'a str,
    pub(super) login: &'a Login,
    pub(super) handoff: &'a Path,
    pub(super) runner_may_have_started: bool,
    pub(super) workspace_budget: u64,
    pub(super) cache: Option<&'a aq_sandbox_archive::build_cache::Opened>,
}
pub(super) struct Signals {
    pub(super) hup: tokio::signal::unix::Signal,
    pub(super) term: tokio::signal::unix::Signal,
}
struct Ended {
    exit: RunnerExit,
    stream: Vec<u8>,
    handoff: Option<aq_handoff_note::Stop>,
}
impl<T: DockerTransport> Drive<'_, T> {
    pub(super) async fn run<A, AF, S>(
        &mut self,
        prompt: &[u8],
        terminal: A,
        signals: &mut Signals,
    ) -> Result<Drove>
    where
        A: FnOnce(PathBuf, String) -> AF,
        AF: std::future::Future<Output = Result<(S, Pane)>>,
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + 'static,
    {
        let workspace = collect_tree(&self.config.workspace, self.workspace_budget)?;
        let login = self.login;
        let tokens = login.redact.iter().map(|t| t.as_slice()).collect();
        let secrets = Secrets::of_tokens(tokens, workspace.iter().flat_map(tree_bytes));
        let projects = projects::writable_trees(
            &self.config.workspace,
            projects::after_tree(self.workspace_budget, &workspace)?,
        )?;
        self.stage(prompt, &workspace, &projects).await?;
        // The sandbox's terminal on this pane, from here until the sandbox stops.
        let id = &self.sandbox.backend_id;
        let attach = format!(
            "{}/containers/{id}/attach?stream=1&stdin=1&stdout=1&stderr=1",
            crate::backend::docker_local::API_VERSION
        );
        let (stream, pane) = terminal(self.config.docker_local.socket.clone(), attach)
            .await
            .context("sandbox terminal attach")?;
        let relay = pane::Relay::start(stream, pane, login.redact.clone());
        let ended = self.wait_for_end(signals).await?;
        let pane_leak = relay.finish().await;
        self.copy_back(ended, &secrets, pane_leak).await
    }
    pub(super) async fn preserve(
        &mut self,
        error: anyhow::Error,
        config_path: &Path,
    ) -> Result<i32> {
        // Keep the disk: stop and preserve, never destroy unretained work.
        let stopped = self.backend.stop(self.sandbox).await;
        println!(
            "aq-sandbox-harness: preserved sandbox {} after error: {error:#} (stop confirmed: {})",
            self.sandbox.backend_id,
            stopped.is_confirmed()
        );
        if self.runner_may_have_started {
            save_conversation(self.backend, self.config, self.sandbox, &self.login.token).await;
        } else if let Err(error) = staging::record(
            &self.config.workspace,
            self.sandbox,
            self.container_id,
            staging::Phase::PrepareFailed,
        ) {
            tracing::warn!(%error, "sandbox preparation failure receipt unavailable");
        }
        let backend_unavailable = matches!(
            error.downcast_ref::<BackendError>(),
            Some(BackendError::Unavailable(_) | BackendError::Uncertain(_))
        );
        if backend_unavailable && !self.runner_may_have_started {
            aq_handoff_note::write_platform_failure(config_path);
        } else {
            write_exit(config_path, false);
        }
        // The marker precedes the claim: the node never destroys a preserved sandbox.
        let Some(claim) = preserved_claim(&error, &self.sandbox.backend_id, config_path) else {
            println!(
                "aq-sandbox-harness: no claim (incomplete copy-back or no preserved marker); salvage copies the preserved sandbox again"
            );
            return Ok(5);
        };
        submit_claim(self.config, claim);
        Ok(3)
    }
    async fn stage(
        &mut self,
        prompt: &[u8],
        workspace: &[TreeEntry],
        projects: &[projects::ProjectTree],
    ) -> Result<()> {
        self.backend
            .put_archive(
                self.sandbox,
                tree_archive("aq/in", &input_entries(self.config, prompt)?)?,
            )
            .await
            .context("sandbox launch input upload")?;
        let image = self
            .config
            .docker_local
            .template_image(&self.config.template);
        let daemon = (self.config.docker_local.socket.as_path(), image);
        let archive = aq_sandbox_archive::build_cache::work_archive(self.cache, workspace, daemon)
            .await
            .context("prepare sandbox workspace archive")?;
        self.backend
            .put_archive(self.sandbox, archive)
            .await
            .context("sandbox workspace upload")?;
        for tree in projects {
            let target = aq_node_git::workspaces::sandbox_path(tree.project);
            self.backend
                .put_archive(
                    self.sandbox,
                    tree_archive(target.trim_start_matches('/'), &tree.entries)?,
                )
                .await
                .context("sandbox related project upload")?;
        }
        self.backend
            .deliver_private_material(
                self.sandbox,
                PrivateMaterial::new(self.login.token.to_vec()),
            )
            .await
            .context("sandbox private material upload")?;
        ensure!(
            now_ms() < self.config.hard_deadline_ms,
            "sandbox preparation reached the attempt deadline"
        );
        self.runner_may_have_started = true;
        self.backend
            .start_runner(self.sandbox)
            .await
            .context("sandbox runner start")?;
        ensure!(
            self.backend.container_id(self.sandbox).await?.as_deref() == Some(self.container_id),
            "sandbox container changed during staging"
        );
        staging::record(
            &self.config.workspace,
            self.sandbox,
            self.container_id,
            staging::Phase::Started,
        )
        .context("record staged sandbox start")?;
        if self.config.postgres {
            self.backend
                .start_task_postgres(self.sandbox)
                .await
                .context("start required task PostgreSQL")?;
        }
        println!("aq-sandbox-harness: runner started");
        Ok(())
    }
    async fn wait_for_end(&mut self, signals: &mut Signals) -> Result<Ended> {
        let config = self.config;
        let sandbox = self.sandbox;
        let backend = &mut self.backend;
        let mut resized =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())?;
        // A resize also makes a TUI that started before the attach paint again.
        let mut size = pane::window_size();
        let mut handoff_reason = None;
        let (exit, stream) = loop {
            if let Some(size) = size.take() {
                let _ = backend.resize(sandbox, size).await;
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(2)) => {},
                _ = resized.recv() => { size = pane::window_size(); continue; },
                _ = signals.term.recv() => bail!("stop requested"),
                _ = signals.hup.recv() => bail!("pane closed"),
            }
            let mount = (config.docker_local.transcript.as_ref()).zip(config.transcript_bound);
            let over = || mount.and_then(|((directory, _), bound)| bound.exceeded(directory));
            let over = || over().or_else(|| self.cache.and_then(|c| c.exceeded()));
            if let Some(reason) = read_handoff_marker(self.handoff)
                .map(Into::into)
                .or_else(over)
            {
                handoff_reason = Some(reason);
                // A stopped container still serves get_archive for the copy-back.
                let _ = backend.stop(sandbox).await;
                break (RunnerExit::Signalled, Vec::new());
            }
            // The harness's stop hook: the turn ended and this is its final message.
            // The interactive harness does not exit by itself, so the turn's end is
            // the attempt's end.
            let turn = backend.read_file(sandbox, STOP_MARKER).await;
            if let Some(stream) = turn.ok().as_deref().and_then(stop_stream) {
                let _ = backend.stop(sandbox).await;
                break (RunnerExit::Exited(0), stream);
            }
            // The harness left by itself (owner exit, crash): no final message.
            if let Ok(exit) = backend.runner_exit(sandbox).await {
                break (exit, Vec::new());
            }
            if now_ms() >= config.hard_deadline_ms {
                bail!("sandbox deadline reached");
            }
        };
        Ok(Ended {
            exit,
            stream,
            handoff: handoff_reason,
        })
    }
    async fn copy_back(
        &mut self,
        ended: Ended,
        secrets: &Secrets<'_>,
        pane_leak: bool,
    ) -> Result<Drove> {
        let container_id = self
            .backend
            .container_id(self.sandbox)
            .await?
            .context("sandbox container absent")?;
        staging::require_started(&self.config.workspace, self.sandbox, &container_id)
            .context("verify staged sandbox start")?;
        let Ended {
            exit,
            stream,
            handoff: handoff_reason,
        } = ended;
        let config = self.config;
        // Below the TUI's last screen, so the driver's own lines do not overwrite it.
        print!("\r\n\x1b[J");
        let rc = match exit {
            RunnerExit::Exited(code) => Some(code),
            RunnerExit::Signalled => None,
        };
        let archive = self
            .backend
            .get_archive(
                self.sandbox,
                "/aq/work",
                usize::try_from(self.workspace_budget)?.saturating_mul(2),
            )
            .await
            .context("workspace copy-back")?;
        let entries = read_tree(&archive, self.workspace_budget)?;
        let siblings = projects::returned_trees(
            self.backend,
            self.sandbox,
            &config.workspace,
            projects::after_tree(self.workspace_budget, &entries)?,
        )
        .await?;
        ensure!(
            self.backend.container_id(self.sandbox).await?.as_deref()
                == Some(container_id.as_str()),
            "sandbox container changed during copy-back"
        );
        let leak = token_leak(&entries, &stream, secrets)
            .or_else(|| {
                siblings
                    .iter()
                    .find_map(|tree| token_leak(&tree.entries, &[], secrets))
            })
            .or(pane_leak.then_some("a login secret appeared in the sandbox terminal output"));
        if leak.is_some() {
            // Nothing the sandbox produced is copied back.
            return Ok(Drove {
                rc,
                changed: Vec::new(),
                stream: Vec::new(),
                leak,
                handoff: None,
            });
        }
        let mut changed = sync_tree(&config.workspace, &entries)?;
        changed.extend(projects::apply(&siblings)?);
        Ok(Drove {
            rc,
            changed,
            stream,
            leak: None,
            handoff: handoff_reason,
        })
    }
}
