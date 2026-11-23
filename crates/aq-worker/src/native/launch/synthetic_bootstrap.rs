use super::*;
pub(super) fn bootstrap_synthetic(
    owner: &Arc<Mutex<Owned>>,
    registry: &Registry,
    entry: &Arc<Entry>,
) -> Result<()> {
    let scope = entry.scope.as_ref().context("native launch scope absent")?;
    let Profile::Synthetic(profile) = entry.profile.as_ref().context("native profile absent")?
    else {
        anyhow::bail!("synthetic profile required")
    };
    profile.record_intent(owner, entry, scope)?;
    profile.verify_installation()?;
    {
        let owner = lock(owner)?;
        entry.current(&owner)?;
    }
    let h = lock(&entry.history)?.clone().unwrap();
    let i = &h.intent;
    let elf = registry.pool.load_verified(
        &profile.config.program.path,
        &profile.config.program.artifact,
    )?;
    let startup = aq_session_host::observed::ObservedStartup::new(
        scope.inputs.startup().clone(),
        i.terminal.clone(),
        elf,
        aq_session_host::observed::ObservationProfile::small_elf(
            profile.config.renderer.clone(),
            i.columns,
            i.rows,
        )?,
        scope.hard / 1_000_000,
    )?;
    let permit = bootstrap::permit(scope, i)?;
    let local = profile.local(owner, entry, scope)?;
    let adapter = Adapter {
        profile: profile.clone(),
        launch_id: i.launch_id,
    };
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
    authority.wait_for_running(
        released,
        observer,
        bootstrap::ObservationErrors {
            output: "native bootstrap output bound",
            expired: "native positive exec observation expired",
        },
    )
}

impl SyntheticHostProfile {
    fn record_intent(
        &self,
        owner: &Mutex<Owned>,
        entry: &Entry,
        scope: &LaunchExecutionScope,
    ) -> Result<()> {
        let mut owner = lock(owner)?;
        entry.current(&owner)?;
        let state = owner.journal.state();
        let a = &state.attempts[&entry.id];
        ensure!(
            a.reservation.offer.startup.payload_bytes <= 4096,
            "synthetic startup ceiling"
        );
        let c = &self.config;
        let [executable, helper, tmux] =
            [&c.program, &c.helper, &c.tmux].map(|p| p.artifact.clone());
        let pins = HostPins {
            pid_context: "PID namespace text",
            root: (c.root.clone(), self.root.identity()),
            profile_digest: self.digest.clone(),
            programs: [executable, helper, tmux, c.renderer.clone()],
            size: (c.columns, c.rows),
        };
        let intent = launch_intent(scope, state.revision, a, pins)?;
        #[cfg(test)]
        entry.hook(Checkpoint::Outer, &mut owner.journal)?;
        owner.journal.begin_native_launch(entry.id, intent)?;
        entry.snapshot(&owner.journal)?;
        entry.current(&owner)?;
        Ok(())
    }
    fn local(
        &self,
        owner: &Mutex<Owned>,
        entry: &Entry,
        scope: &LaunchExecutionScope,
    ) -> Result<aq_session_host::LocalEnvironment> {
        let credentials = {
            let owner = lock(owner)?;
            entry.current(&owner)?;
            owner.journal.state().attempts[&entry.id]
                .reservation
                .offer
                .offered
                .offer
                .manifest
                .credentials
                .iter()
                .map(|c| c.credential_profile.clone())
                .collect()
        };
        Ok(aq_session_host::LocalEnvironment {
            resource_policy: aq_session_host::ResourcePolicy::CallerAdmission(ArtifactRef {
                id: aq_session_protocol::Text::new("aq.synthetic-admission-only")?,
                version: aq_session_protocol::Text::new("1")?,
                digest: Digest::of(b"synthetic host admission; no hard resource enforcement"),
            }),
            workspace: scope.inputs.workspace().workspace().path.clone(),
            helper_executable: self.config.helper.path.clone(),
            tmux_executable: self.config.tmux.path.clone(),
            tmux_version: "tmux 3.4".into(),
            credential_profiles: credentials,
            context_material: vec![],
        })
    }
}
