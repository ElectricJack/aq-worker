//! Durable launch ordering for both task and host-plan startup.
use super::*;
use std::os::fd::OwnedFd;
mod material;

pub(super) struct LaunchRequest<'a> {
    pub(super) root: &'a Path,
    pub(super) launch_id: Uuid,
    pub(super) startup: StartupInput<'a>,
    pub(super) permit: ExecutionPermit,
    pub(super) local: &'a LocalEnvironment,
    pub(super) agent_identity: Option<users::AgentIdentity>,
}

struct PreparedLaunch {
    directory: PathBuf,
    config: SealedLaunch,
    publication: Option<private_inputs::Publication>,
    context: Vec<Vec<u8>>,
}

impl<'a> LaunchRequest<'a> {
    pub(super) fn start(
        self,
        adapter: Adapter<'_>,
        authority: &mut impl LaunchAuthority,
    ) -> Result<(SessionHandle, Option<observed::gate::Client>)> {
        let manifest = self.startup.document();
        let slot = reaper::Slot::reserve()?;
        let observed = self.startup.observed();
        let prepared = self.prepare(manifest, adapter)?;
        prepared.start(slot, observed, authority)
    }
}

impl PreparedLaunch {
    fn start(
        self,
        slot: reaper::Slot,
        observed: Option<&observed::ObservedStartup>,
        authority: &mut impl LaunchAuthority,
    ) -> Result<(SessionHandle, Option<observed::gate::Client>)> {
        let Self {
            directory,
            config,
            publication,
            context,
        } = self;
        let intent = config.intent()?;
        publish(&directory, &config, context)?;
        authority.record_launch_intent(&intent)?;
        authority.checkpoint("launch-intent-durable")?;
        let private_source = publication
            .map(|p| p.publish(&config.workspace))
            .transpose()?;
        private_inputs::verify_at_exec(&config, &directory)?;
        let guardian_fd = slot.track(spawn(&config, &directory)?)?;
        authority.checkpoint("guardian-spawned")?;
        let identity = wait_guardian(&directory, &guardian_fd, &intent)?;
        let handle = SessionHandle {
            directory,
            identity,
            private_source,
        };
        authority.record_guardian(&handle.identity)?;
        authority.checkpoint("guardian-identity-durable")?;
        let observed_client = observed
            .map(|startup| {
                observed::gate::Client::admit(
                    &handle.directory,
                    &handle.identity,
                    &startup.program,
                    config.permit.expires_boot_ms.min(config.wall_deadline()?),
                )
            })
            .transpose()?;
        authority.record_start_intent(&intent, &handle.identity)?;
        authority.checkpoint("start-intent-durable")?;
        handle.request("start.request", &config.permit)?;
        authority.checkpoint("start-sent")?;
        if let Some(observation) = wait_start(&handle, &config, observed_client.as_ref())? {
            authority.record_started(&observation)?;
            authority.checkpoint("start-receipt-durable")?;
            return Ok((handle, None));
        }
        Ok((handle, observed_client))
    }
}
fn publish(directory: &Path, config: &SealedLaunch, context: Vec<Vec<u8>>) -> Result<()> {
    durable_new(&directory.join("launch.json"), &serde_json::to_vec(config)?)?;
    if config.agent_identity.is_some() {
        users::publish_context(config.launch_id, &context)?;
    }
    for (index, bytes) in context.into_iter().enumerate() {
        durable_new(&directory.join(format!("context-{index}.bin")), &bytes)?;
    }
    Ok(())
}

fn spawn(config: &SealedLaunch, directory: &Path) -> Result<Child> {
    let mut command = Command::new(&config.helper);
    command
        .arg("guardian")
        .arg(directory)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(feature = "test-support")]
    if config.observed.is_some() {
        // Private synthetic-test bootstrap diagnostics; never enabled by a
        // production build and never attached to native harness stderr.
        command.stderr(
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(directory.join("guardian-test-stderr.log"))?,
        );
    }
    #[cfg(feature = "test-support")]
    if let Some(stage) = std::env::var_os("AQ_SESSION_HOST_FAULT_STAGE") {
        command.env("AQ_SESSION_HOST_FAULT_STAGE", stage);
    }
    command.spawn().context("guardian spawn uncertain")
}

fn wait_guardian(
    directory: &Path,
    guardian_fd: &OwnedFd,
    intent: &LaunchIntent,
) -> Result<GuardianIdentity> {
    let end = Instant::now() + Duration::from_secs(3);
    loop {
        if let Ok(id) = read_json::<GuardianIdentity>(&directory.join("identity.json")) {
            ensure!(id.intent == *intent, "guardian intent mismatch");
            return Ok(id);
        }
        ensure!(
            ownership::pidfd_is_live(guardian_fd)?,
            "guardian exited before identity"
        );
        ensure!(Instant::now() < end, "guardian identity timeout");
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn wait_start(
    handle: &SessionHandle,
    config: &SealedLaunch,
    observed_client: Option<&observed::gate::Client>,
) -> Result<Option<HostObservation>> {
    let end = Instant::now() + Duration::from_secs(3);
    loop {
        let s = handle.observe()?;
        if let (Some(client), Some(spec)) = (observed_client, &config.observed)
            && client
                .closed(
                    spec.baseline_id,
                    config.permit.expires_boot_ms.min(config.wall_deadline()?),
                )?
                .is_some()
        {
            let current = handle.observe()?;
            ensure!(
                current.phase == HostPhase::Starting,
                "observed gate must remain Starting"
            );
            return Ok(None);
        }
        if s.phase == HostPhase::Running {
            return Ok(Some(s));
        }
        ensure!(
            !matches!(s.phase, HostPhase::Stopped | HostPhase::StopUncertain),
            "session ended before startup: {:?}",
            s.stop_reason
        );
        ensure!(Instant::now() < end, "tmux startup timeout");
        std::thread::sleep(Duration::from_millis(2));
    }
}
