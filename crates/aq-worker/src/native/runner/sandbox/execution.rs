use super::*;
use aq_admission_sandbox::sandbox::RunnerRenewalRequest;
use aq_session_host::users::AgentIdentity;

pub(super) struct Site<'a> {
    pub(super) worker: &'a WorkerConfig,
    pub(super) frozen: &'a FrozenLaunch,
    pub(super) state_dir: &'a Path,
    pub(super) local: &'a LocalEnvironment,
}

pub(super) struct Driver<'a, T> {
    pub(super) transport: &'a mut T,
    pub(super) shutdown: watch::Receiver<bool>,
    pub(super) options: RunOptions,
}

pub(super) struct Harness<'a, 'l, S: LoginService> {
    pub(super) startup: SandboxStartup<'a>,
    pub(super) agent: Option<AgentIdentity>,
    pub(super) login: Option<SandboxLogin<'l, S>>,
}

struct Admission {
    now: u64,
    deadline: u64,
    hard: u64,
    request: RunnerRenewalRequest,
    authority: Option<auth::RunnerHttp>,
}
impl Admission {
    fn initial_permit(&self, frozen: &FrozenLaunch) -> Result<aq_session_host::ExecutionPermit> {
        local_permit(frozen, self.request.revision, self.now, self.deadline)
    }
}
impl Site<'_> {
    fn authorize<T: RunnerTransport, S: LoginService>(
        &self,
        permit: DelegatedPermit,
        driver: &mut Driver<'_, T>,
        harness: &Harness<'_, '_, S>,
    ) -> Result<Admission> {
        let Self {
            worker,
            frozen,
            state_dir,
            local,
        } = *self;
        let (initial_deadline, hard) = frozen.validate_local(state_dir)?;
        frozen
            .authority
            .validate_handoff(&permit, frozen.delivery.clock.server_time_ms)?;
        ensure!(
            worker.node_id == permit.binding.node.node_id,
            "worker identity required"
        );
        ensure!(
            !driver.options.renewal_interval.is_zero()
                && driver.options.renewal_interval <= Duration::from_secs(15),
            "renewal interval bounds"
        );
        ensure!(
            local.workspace.canonicalize()? == local.workspace
                && (harness.agent.is_some()
                    || (local.workspace.starts_with(&frozen.state_dir)
                        && local.workspace != frozen.state_dir)),
            "workspace must be prepared inside bound state directory"
        );
        let authority = driver.transport.authority_client();
        if let Some(http) = &authority {
            http.bind_execution(frozen, &permit)?;
        }
        harness.startup.validate(frozen, authority.as_ref())?;
        driver.transport.preflight(worker)?;
        ensure!(
            !authority
                .as_ref()
                .is_some_and(auth::RunnerHttp::authority_lost),
            "runner authority lost before launch"
        );
        ensure!(!*driver.shutdown.borrow(), "cancelled before launch");
        let now = boot_ns()?;
        let (deadline, hard) = deadlines(frozen, &permit, (initial_deadline, hard), now)?;
        let request = RunnerRenewalRequest {
            schema_version: 1,
            operation_id: Uuid::new_v4(),
            authority: frozen.authority.clone(),
            permit,
            revision: frozen.revision,
        };
        request.validate()?;
        Ok(Admission {
            now,
            deadline,
            hard,
            request,
            authority,
        })
    }
}
fn deadlines(
    frozen: &FrozenLaunch,
    permit: &DelegatedPermit,
    (initial_deadline, hard): (u64, u64),
    now: u64,
) -> Result<(u64, u64)> {
    let hard = hard.min(
        frozen
            .sent_boot_ns
            .checked_add(
                frozen
                    .manifest
                    .resources
                    .wall_time_ms
                    .checked_mul(1_000_000)
                    .ok_or_else(|| anyhow::anyhow!("wall overflow"))?,
            )
            .ok_or_else(|| anyhow::anyhow!("wall overflow"))?,
    );
    let permit_deadline = permit
        .expires_at_ms
        .checked_sub(frozen.delivery.clock.server_time_ms)
        .and_then(|v| v.checked_mul(1_000_000))
        .and_then(|v| v.checked_add(frozen.sent_boot_ns))
        .ok_or_else(|| anyhow::anyhow!("delegated deadline overflow"))?;
    let deadline = initial_deadline
        .min(permit_deadline)
        .min(hard)
        .min(now.saturating_add(aq_session_host::MAX_PERMIT_MS * 1_000_000));
    Ok((deadline, hard))
}
pub(super) async fn run<S: LoginService, T: RunnerTransport>(
    site: Site<'_>,
    permit: DelegatedPermit,
    mut driver: Driver<'_, T>,
    harness: Harness<'_, '_, S>,
) -> Result<RunOutcome> {
    let admission = site.authorize(permit, &mut driver, &harness)?;
    let mut journal = journal::Journal::create(site.state_dir, site.frozen)?;
    let key = AttemptKey {
        launch_id: site.frozen.launch_id,
        attempt_id: site.frozen.manifest.bindings.attempt_id,
        manifest_digest: aq_session_protocol::canonical_digest(&site.frozen.manifest)?,
    };
    let initial = admission.initial_permit(site.frozen)?;
    journal.record("initial-permit", &initial)?;
    let mut active = start_login(harness.login, harness.agent).await?;
    let login_env = login_environment(&active)?;
    let root = journal.directory.join("hosts");
    let launched = harness
        .startup
        .launch(startup::Launch {
            root: &root,
            frozen: site.frozen,
            permit: initial,
            local: site.local,
            journal: &mut journal,
            agent: harness.agent,
            shutdown: driver.shutdown.clone(),
            login: login_env,
        })
        .await;
    let (handle, output_runtime) = match launched {
        Ok(launched) => launched,
        Err(error) => {
            tracing::warn!("sandbox host launch failed: {error:#}");
            return preserve_failed_launch(&journal, &mut active, &root, admission.deadline).await;
        }
    };
    let mut renewal = renewal::Renewal {
        frozen: site.frozen,
        authority: admission.authority.as_ref(),
        handle: &handle,
        journal: &journal,
        request: admission.request,
        deadline: admission.deadline,
        hard: admission.hard,
    };
    let reason = renewal
        .monitor(
            (site.worker, driver.transport),
            &mut driver.shutdown,
            (admission.now, driver.options.renewal_interval),
        )
        .await;
    let deadline = renewal.deadline;
    completion::Completion {
        key,
        local: site.local,
        handle,
        journal,
        output: output_runtime,
        active,
    }
    .finish(reason, deadline, (site.worker, driver.transport))
    .await
}

async fn start_login<S: LoginService>(
    login: Option<SandboxLogin<'_, S>>,
    agent: Option<AgentIdentity>,
) -> Result<ActiveLogin<'_, S>> {
    // C6b: install the newest leased login before the harness spawn.
    Ok(match login {
        None => None,
        Some(login) => {
            let started = SubscriptionLogin::start_as(
                &mut *login.service,
                login.kind,
                login.codex_home.as_deref(),
                agent,
            )
            .await
            .context("subscription login unavailable")?;
            Some((login, started))
        }
    })
}
fn login_environment<'a, S: LoginService>(
    active: &'a ActiveLogin<'_, S>,
) -> Result<Option<startup::LoginEnv<'a>>> {
    Ok(match active {
        None => None,
        Some((_, started)) => {
            let installed = started.installed();
            Some(match (installed.claude_env(), installed.codex_home()) {
                (Some((name, value)), _) => startup::LoginEnv { name, value },
                (None, Some(home)) => startup::LoginEnv {
                    name: "CODEX_HOME",
                    value: home.to_str().context("Codex login home UTF-8")?,
                },
                (None, None) => anyhow::bail!("installed login has no launch entry"),
            })
        }
    })
}

async fn preserve_failed_launch<S: LoginService>(
    journal: &journal::Journal,
    active: &mut ActiveLogin<'_, S>,
    root: &Path,
    deadline: u64,
) -> Result<RunOutcome> {
    use crate::native::launch::sandbox as host;
    let proof = journal
        .intent
        .as_ref()
        .and_then(|intent| host::recover_stopped(root, intent).ok());
    let wiped = finish_login(active, &journal.directory, deadline).await;
    if let Some(proof) = &proof {
        journal.record("stopped", proof)?;
    }
    Ok(RunOutcome::Preserved {
        reason: if wiped.is_ok() {
            "launch failed"
        } else {
            "login teardown failed"
        },
        stop_proven: proof.is_some(),
    })
}
