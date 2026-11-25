use super::*;
use crate::sandbox_driver::DriverConfig;
use aq_admission::startup::NativeOffer;
use aq_session_host::observed::{ObservedStartup, ReleasedObservedSession};
use aq_session_host::terminal_output::OutputObserver;
use aq_worker_protocol::git_author::GitAuthor;

struct Launch<'a> {
    entry: &'a Arc<Entry>,
    scope: &'a LaunchExecutionScope,
    offer: &'a NativeOffer,
    intent: &'a NativeLaunchIntent,
    profile: &'a ClaudeDockerHostProfile,
}
struct DriverFiles {
    guard: TokenGuard,
    driver_file: PathBuf,
    driver: DriverConfig,
    transcript: PathBuf,
}
struct DriverAdmission {
    node_id: Uuid,
    boot_id: Uuid,
    deadline_ms: u64,
    docker_local: DockerLocalConfig,
    template: TemplateDigest,
    agent_channel: bool,
}
struct ReleasedDriver {
    session: ReleasedObservedSession,
    observer: OutputObserver,
}
pub(in super::super) fn bootstrap(
    owner: &Arc<Mutex<Owned>>,
    entry: &Arc<Entry>,
    profile: &Arc<ClaudeDockerHostProfile>,
) -> Result<()> {
    let scope = entry.scope.as_ref().context("native launch scope absent")?;
    let (offer, git_author) = profile.record_intent(owner, entry, scope)?;
    profile.verify_installation()?;
    let h = lock(&entry.history)?
        .clone()
        .context("native launch intent absent")?;
    let i = &h.intent;
    let launch = Launch {
        entry,
        scope,
        offer: &offer,
        intent: i,
        profile,
    };
    let files = launch.prepare_driver(git_author)?;
    launch.release(owner, files)
}
impl ClaudeDockerHostProfile {
    fn record_intent(
        &self,
        owner: &Mutex<Owned>,
        entry: &Entry,
        scope: &LaunchExecutionScope,
    ) -> Result<(NativeOffer, Option<GitAuthor>)> {
        let mut owner = lock(owner)?;
        entry.current(&owner)?;
        let state = owner.journal.state();
        let a = &state.attempts[&entry.id];
        let (offer, git_author) = (
            a.reservation.offer.clone(),
            a.reservation.git_author.clone(),
        );
        let c = &self.config;
        let [executable, helper, tmux] =
            [&self.driver, &c.helper, &c.tmux].map(|p| p.artifact.clone());
        let pins = HostPins {
            pid_context: "PID namespace text",
            root: (c.root.clone(), self.root.identity()),
            profile_digest: self.digest.clone(),
            programs: [executable, helper, tmux, c.renderer.clone()],
            size: (c.columns, c.rows),
        };
        let intent = launch_intent(scope, state.revision, a, pins)?;
        owner.journal.begin_native_launch(entry.id, intent)?;
        entry.snapshot(&owner.journal)?;
        entry.current(&owner)?;
        Ok((offer, git_author))
    }
}
impl Launch<'_> {
    fn prepare_driver(&self, git_author: Option<GitAuthor>) -> Result<DriverFiles> {
        let Self {
            entry: _,
            scope,
            offer,
            intent: i,
            profile,
        } = *self;
        let m = &offer.offered.offer.manifest;
        let launch = match select_harness(m.harness.harness.as_str(), profile.codex.is_some())? {
            DockerHarness::Claude => claude_launch(scope, m, profile)?,
            DockerHarness::Codex => codex_launch(scope, m, profile)?,
        };
        let (node_id, boot_id) = match &i.binding.session {
            aq_session_protocol::SessionIdentity::Remote {
                node_id, boot_id, ..
            } => (*node_id, *boot_id),
            _ => anyhow::bail!("Claude docker-local requires a remote node session"),
        };
        let now = boot_ns()?;
        let hard = scope.deadline.min(scope.hard);
        let deadline_ms = aq_handoff_note::now_ms()
            .checked_add(hard.saturating_sub(now) / 1_000_000)
            .context("sandbox deadline overflow")?;
        let (docker_local, template, agent_channel) =
            self.open_channel(deadline_ms, node_id, &git_author)?;
        self.write_driver(
            launch,
            git_author,
            DriverAdmission {
                node_id,
                boot_id,
                deadline_ms,
                docker_local,
                template,
                agent_channel,
            },
        )
    }
    fn write_driver(
        &self,
        launch: HarnessLaunch,
        git_author: Option<GitAuthor>,
        admission: DriverAdmission,
    ) -> Result<DriverFiles> {
        let mut docker_local = admission.docker_local;
        let m = &self.offer.offered.offer.manifest;
        let (private, transcript) = self.transcript_directory(&mut docker_local, &launch)?;
        let token_file = private.join("token");
        let guard = TokenGuard(Some(token_file.clone()));
        private_write(&token_file, &launch.token)?;
        drop(launch.token);
        if let Some(lease) = &launch.login_lease {
            let file = private.join(aq_agent_channel::login::LEASE_FILE);
            private_write(&file, &serde_json::to_vec(lease)?)?;
        }
        let result_dir = self
            .entry
            .result_channel
            .as_ref()
            .map(|channel| channel.root().to_owned());
        let template = admission.template;
        #[cfg_attr(not(feature = "project_environments"), allow(unused_mut))]
        let mut driver = crate::sandbox_driver::DriverConfig {
            docker_local,
            template: template.clone(),
            owner: Ownership {
                node_id: admission.node_id,
                boot_id: admission.boot_id,
                attempt_id: self.intent.binding.attempt_id,
            },
            hard_deadline_ms: admission.deadline_ms,
            resources: ResourceLimits {
                cpu_millis: u32::try_from(m.resources.cpu_millis)?,
                memory_bytes: m.resources.memory_bytes,
                pids: m.resources.process_limit,
                disk_bytes: None,
            },
            workspace: self.scope.inputs.workspace().workspace().path.clone(),
            token_file,
            executable: launch.executable,
            arguments: launch.arguments,
            environment: launch.environment,
            settings_json: launch.settings_json,
            result_dir,
            max_workspace_bytes: self.profile.config.max_workspace_bytes,
            agent_channel: admission.agent_channel,
            postgres: false,
            conversation_outbox: self.profile.conversation_outbox.clone(),
            transcript_bound: self.profile.transcript_bound,
            harness: launch.harness,
            codex_runtime_bundle: launch.codex_runtime_bundle,
            build_cache: (self.profile.config.build_cache.as_ref())
                .and_then(|c| c.slot(m.bindings.project_id, m.bindings.task_id, template.as_str())),
        };
        #[cfg(feature = "project_environments")]
        if let (Some(cache), Some(held)) = (
            &mut driver.build_cache,
            driver.docker_local.environments.get(&m.harness.environment),
        ) {
            cache.warm_from_environment(held.warm);
        }
        let (driver, driver_file) = save_driver(driver, git_author, &private)?;
        Ok(DriverFiles {
            guard,
            driver,
            driver_file,
            transcript,
        })
    }
    fn transcript_directory(
        &self,
        docker_local: &mut DockerLocalConfig,
        launch: &HarnessLaunch,
    ) -> Result<(PathBuf, PathBuf)> {
        let private = self
            .profile
            .config
            .root
            .join(format!("driver-{}", self.intent.launch_id));
        std::fs::DirBuilder::new().mode(0o700).create(&private)?;
        // Of the harness's private home, the node sees its transcript directory only.
        let transcript = private.join("transcript");
        std::fs::DirBuilder::new().mode(0o700).create(&transcript)?;
        let target = launch.harness.transcript_dir().to_owned();
        docker_local.transcript = Some((transcript.clone(), target));
        Ok((private, transcript))
    }
    /// Opens the agent channel when required, and picks the offer's template.
    #[cfg_attr(not(feature = "project_environments"), allow(unused_variables))]
    fn open_channel(
        &self,
        deadline_ms: u64,
        node_id: Uuid,
        git_author: &Option<GitAuthor>,
    ) -> Result<(DockerLocalConfig, TemplateDigest, bool)> {
        let Self {
            entry,
            profile,
            intent: i,
            scope,
            offer,
        } = *self;
        let mut docker_local = profile.config.docker_local.clone();
        docker_local.workspace_bind_paths = docker_local.project_bind_paths();
        let template = profile.config.template.clone();
        #[cfg(feature = "project_environments")]
        let template = docker_local
            .offered_template(&offer.offered.offer.manifest.harness.environment, &template)?;
        let agent_channel = profile.config.agent_channel.is_required();
        if agent_channel {
            let channel = profile.open_channel(&i.binding.session, deadline_ms)?;
            #[cfg(feature = "project_environments")]
            if let Some(node) = scope.inputs.workspace().git().node.clone() {
                let owner = (node_id, i.binding.attempt_id);
                let workspace = scope.inputs.workspace().workspace().path.clone();
                let author = git_author.clone();
                channel.set_checkpoint(try_checkpoint(profile, node, owner, workspace, author));
            }
            let mut physical = lock(&entry.physical)?;
            if entry.stop.load(Ordering::SeqCst) {
                channel.close();
            }
            physical.channel = Some(channel);
            docker_local.channel_root = Some(profile.channel_root());
        }
        Ok((docker_local, template, agent_channel))
    }
    fn startup(
        &self,
    ) -> Result<(
        ObservedStartup,
        ExecutionPermit,
        aq_session_host::LocalEnvironment,
    )> {
        let Self {
            scope,
            profile,
            intent: i,
            ..
        } = *self;
        let m = &self.offer.offered.offer.manifest;
        let hard = scope.deadline.min(scope.hard);
        let image = profile
            .pool
            .load_verified(&profile.driver.path, &profile.driver.artifact)?;
        let startup = aq_session_host::observed::ObservedStartup::native_exec(
            scope.inputs.startup().clone(),
            i.terminal.clone(),
            image,
            NativeExecObservationProfile::new(i.renderer.clone(), i.columns, i.rows)?,
            scope.hard / 1_000_000,
        )?;
        let permit = ExecutionPermit {
            session: i.binding.session.clone(),
            manifest_digest: i.binding.launch_manifest_digest.clone(),
            sequence: 1,
            issued_boot_ms: boot_ns()? / 1_000_000,
            // Non-interactive local permits are capped; the executor renews them.
            expires_boot_ms: (hard / 1_000_000)
                .min(boot_ns()? / 1_000_000 + aq_session_host::MAX_PERMIT_MS - 5_000),
        };
        let local = aq_session_host::LocalEnvironment {
            resource_policy: aq_session_host::ResourcePolicy::CallerAdmission(
                m.harness.environment.clone(),
            ),
            workspace: scope.inputs.workspace().workspace().path.clone(),
            helper_executable: profile.config.helper.path.clone(),
            tmux_executable: profile.config.tmux.path.clone(),
            tmux_version: "tmux 3.4".into(),
            credential_profiles: m
                .credentials
                .iter()
                .map(|c| c.credential_profile.clone())
                .collect(),
            context_material: vec![],
        };
        Ok((startup, permit, local))
    }
    fn adapter(&self, driver: &DriverConfig, driver_file: &Path) -> Result<Adapter> {
        let Self {
            profile, intent: i, ..
        } = *self;
        let m = &self.offer.offered.offer.manifest;
        let mut environment = std::collections::BTreeMap::new();
        if let Some(dir) = &driver.result_dir {
            environment.insert(
                "AQ_AGENT_RESULT_DIR".to_string(),
                dir.to_str().context("result dir UTF-8")?.to_string(),
            );
        }
        environment.insert("RUST_LOG".to_string(), "warn".to_string());
        Ok(Adapter {
            command: LocalCommand {
                executable: profile.driver.path.clone(),
                arguments: vec![
                    "sandbox-harness".into(),
                    "--driver-config".into(),
                    driver_file.to_str().context("driver path UTF-8")?.into(),
                ],
                environment,
            },
            executable: profile.driver.artifact.clone(),
            renderer: i.renderer.clone(),
            manifest_digest: crate::native::journal::canonical_digest(m)?,
        })
    }
    fn release(&self, owner: &Arc<Mutex<Owned>>, files: DriverFiles) -> Result<()> {
        let Self {
            entry,
            profile,
            intent: i,
            ..
        } = *self;
        let (startup, permit, local) = self.startup()?;
        let adapter = self.adapter(&files.driver, &files.driver_file)?;
        let mut authority = authority::Authority {
            owner: owner.clone(),
            entry: entry.clone(),
        };
        let pending = SessionHost::prepare_observed(
            &profile.config.root,
            i.launch_id,
            &startup,
            permit,
            &local,
            &adapter,
            &mut authority,
        )?;
        let (observer, baseline) = pending.open_baseline_observer()?;
        let released = pending.release_after_baseline(&baseline, &mut authority)?;
        self.observe_running(
            ReleasedDriver {
                session: released,
                observer,
            },
            &mut authority,
            files,
        )
    }
    fn attach_observation(&self, state: &mut Physical, transcript: PathBuf) {
        let account = |provider: &str| {
            let lease =
                &provider_material(self.scope, &self.offer.offered.offer.manifest, provider)
                    .first()?
                    .metadata()
                    .delivery;
            Some(lease.lease.account_reference.as_str().to_owned())
        };
        let store = self.profile.observations.as_ref();
        state.observation = observe(store, &self.offer.offered.offer, account, transcript);
    }
    fn observe_running(
        &self,
        observed: ReleasedDriver,
        authority: &mut authority::Authority,
        files: DriverFiles,
    ) -> Result<()> {
        let ReleasedDriver {
            session: mut released,
            mut observer,
        } = observed;
        let entry = self.entry;
        let hard = self.scope.deadline.min(self.scope.hard);
        let DriverFiles {
            mut guard,
            transcript,
            ..
        } = files;
        let until = Instant::now() + Duration::from_secs(5);
        let mut bootstrap_output = Vec::new();
        let mut bootstrap_bytes = 0usize;
        loop {
            let events = observer.poll()?;
            bootstrap_bytes += events
                .iter()
                .map(|e| match e {
                    aq_session_host::terminal_output::ControlEvent::Bytes(b) => b.len(),
                    _ => 0,
                })
                .sum::<usize>();
            ensure!(
                bootstrap_bytes <= 65536 && bootstrap_output.len() + events.len() <= 64,
                "Claude driver bootstrap output bound"
            );
            bootstrap_output.extend(events);
            if entry.stop.load(Ordering::SeqCst) {
                released.stop()?;
                anyhow::bail!("Claude driver host stopped before Running");
            }
            if let Some(handle) = released.poll_started(authority)? {
                let mut state = lock(&entry.physical)?;
                state.handle = Some(handle);
                state.observer = Some(observer);
                state.pending_output = bootstrap_output;
                state.outcome = Outcome::Running;
                self.attach_observation(&mut state, transcript);
                guard.disarm();
                return Ok(());
            }
            ensure!(
                boot_ns()? < hard && Instant::now() < until,
                "Claude driver exec observation expired"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

fn save_driver(
    mut driver: DriverConfig,
    git_author: Option<GitAuthor>,
    private: &Path,
) -> Result<(DriverConfig, PathBuf)> {
    if let Some(author) = git_author {
        driver.apply_git_author(&author)?;
    }
    let driver_file = private.join("driver.json");
    private_write(&driver_file, &serde_json::to_vec(&driver)?)?;
    Ok((driver, driver_file))
}
