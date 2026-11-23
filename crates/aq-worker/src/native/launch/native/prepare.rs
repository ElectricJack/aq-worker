use super::*;
use crate::native::{adapter::AdapterManifest, materials::InstalledMaterial};
use aq_admission::startup::NativeOffer;
use aq_worker_protocol::git_author::GitAuthor;

struct Launch<'a> {
    entry: &'a Arc<Entry>,
    scope: &'a LaunchExecutionScope,
    profile: &'a NativeCodexHostProfile,
    offer: &'a NativeOffer,
    intent: &'a NativeLaunchIntent,
}
struct PreparedLaunch {
    image: Image,
    installation: AdapterManifest,
    recipe: PreparedCodexExec,
    private: PrivateLaunchInput,
}
pub(in super::super) fn bootstrap(
    owner: &Arc<Mutex<Owned>>,
    entry: &Arc<Entry>,
    profile: &Arc<NativeCodexHostProfile>,
) -> Result<()> {
    let scope = entry.scope.as_ref().context("native launch scope absent")?;
    let (offer, git_author) = profile.record_intent(owner, entry, scope)?;
    let h = lock(&entry.history)?
        .clone()
        .context("native launch intent absent")?;
    let i = &h.intent;
    let launch = Launch {
        entry,
        scope,
        profile,
        offer: &offer,
        intent: i,
    };
    let prepared = launch.prepare()?;
    launch.release(owner, prepared, git_author.as_ref())
}
impl NativeCodexHostProfile {
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
        a.phase_scope()?; // Its refusal precedes the installation check, as it always did.
        let m = lock(&self.installation)?.installation().clone();
        ensure!(
            a.reservation.runtime.runtime == m.runtime
                && a.reservation.runtime.qualification == m.qualification,
            "native runtime installation differs from grant"
        );
        let pins = HostPins {
            pid_context: "native PID namespace UTF8",
            root: (self.root_path.clone(), self.root.identity()),
            profile_digest: self.digest.clone(),
            programs: [
                m.executable.artifact,
                m.helper.artifact,
                m.tmux.artifact,
                self.mapper.renderer.clone(),
            ],
            size: (self.columns, self.rows),
        };
        let intent = launch_intent(scope, state.revision, a, pins)?;
        #[cfg(test)]
        entry.hook(Checkpoint::Outer, &mut owner.journal)?;
        owner.journal.begin_native_launch(entry.id, intent)?;
        entry.snapshot(&owner.journal)?;
        entry.current(&owner)?;
        Ok((offer, git_author))
    }
}
impl Launch<'_> {
    fn prepare(&self) -> Result<PreparedLaunch> {
        let Self {
            scope,
            profile,
            offer,
            intent: i,
            ..
        } = *self;
        let home = profile
            .root_path
            .join(i.launch_id.to_string())
            .join("codex-home");
        let payload = scope
            .inputs
            .startup()
            .verified_payload(
                &offer.offered.offer.manifest,
                &offer.startup.artifact()?,
                &offer.startup.payload,
            )?
            .to_vec();
        let m = &offer.offered.offer.manifest;
        let provider = self.provider()?;
        let mut installed = lock(&profile.installation)?;
        let handoff = installed.material(provider, &provider.receipt()?, &i.binding)?;
        let interactive = installed.interactive();
        if interactive {
            drop(crate::native::adapter::codex::validated_chatgpt_auth_json(
                &handoff, m,
            )?);
        } else {
            drop(crate::native::adapter::codex::validated_api_key(
                &handoff, m,
            )?);
        }
        let input = CodexExecInput {
            offer,
            expected_manifest: &i.binding.launch_manifest_digest,
            payload,
            workspace: &scope.inputs.workspace().workspace().path,
            private_home: &home,
        };
        let recipe = self.prepare_recipe(&mut installed, input)?;
        let image = installed.image()?;
        let installation = installed.installation().clone();
        drop(installed);
        let (private, recipe) = self.private_recipe(interactive, provider, recipe)?;
        Ok(PreparedLaunch {
            image,
            installation,
            recipe,
            private,
        })
    }
    fn prepare_recipe(
        &self,
        installed: &mut Installation,
        input: CodexExecInput<'_>,
    ) -> Result<PreparedCodexExec> {
        match (&self.profile.result, self.entry.result_channel.as_deref()) {
            (None, None) => {
                ensure!(
                    self.profile.mapper.harness.adapter.id.as_str()
                        != "aq.codex-result-local-adapter",
                    "known result adapter requires the result-enabled profile and actual channel"
                );
                installed.prepare(&self.profile.mapper, input)
            }
            (Some(result), Some(channel)) => {
                let Installation::Named(named) = installed else {
                    anyhow::bail!("result mapping requires the named-readonly image profile")
                };
                self.profile.mapper.prepare_named_result_planned(
                    named,
                    input,
                    CodexResultInput {
                        binding: &self.intent.binding,
                        execution_digest: &self.scope.digest,
                        definition: &result.definition,
                        client: &result.client,
                        channel,
                    },
                )
            }
            _ => anyhow::bail!("result profile and actual execution-owned channel differ"),
        }
    }
    fn provider(&self) -> Result<&InstalledMaterial> {
        let scope = self.scope;
        let m = &self.offer.offered.offer.manifest;
        let providers: Vec<_> = scope
            .inputs
            .materials()
            .iter()
            .filter(|material| {
                let lease = &material.metadata().delivery.lease;
                lease.provider.as_str() == "openai"
                    && m.credentials.iter().any(|r| {
                        r.credential_profile == lease.credential_profile
                            && r.purpose.as_str() == lease.purpose.as_str()
                    })
            })
            .collect();
        ensure!(
            providers.len() == 1,
            "native Codex requires one exact provider material"
        );
        Ok(providers[0])
    }
    fn private_recipe(
        &self,
        interactive: bool,
        provider: &InstalledMaterial,
        recipe: PreparedCodexExec,
    ) -> Result<(PrivateLaunchInput, PreparedCodexExec)> {
        let Self {
            scope, intent: i, ..
        } = *self;
        let (private, recipe) = if interactive {
            let private = PrivateLaunchInput::codex_chatgpt_auth_json(
                provider.source_directory()?,
                original_material(provider)?,
                i.binding.clone(),
                i.launch_id,
                scope.deadline.min(scope.hard) / 1_000_000,
            )?;
            (private, recipe)
        } else {
            let (origin, capability) = self.start_transport(provider)?;
            let private = PrivateLaunchInput::codex_scoped_capability(
                provider.source_directory()?,
                original_material(provider)?,
                ScopedCodexAuthBinding {
                    binding: i.binding.clone(),
                    launch_id: i.launch_id,
                    origin: origin.clone(),
                    deadline_boot_ms: scope.deadline.min(scope.hard) / 1_000_000,
                },
                capability,
            )?;
            let recipe = recipe.with_scoped_provider(&origin)?;
            (private, recipe)
        };
        Ok((private, recipe))
    }
    fn start_transport(
        &self,
        provider: &InstalledMaterial,
    ) -> Result<(String, zeroize::Zeroizing<Vec<u8>>)> {
        let m = &self.offer.offered.offer.manifest;
        let upstream = UpstreamEndpoint::openai();
        #[cfg(any(test, feature = "test-support"))]
        let upstream = if let Some(origin) = &self.profile.development_provider {
            UpstreamEndpoint::development_loopback(origin)?
        } else {
            upstream
        };
        let transport = tokio::runtime::Handle::try_current()?.block_on(
            ScopedResponsesTransport::start_retained(
                ResponsesScope::new(
                    self.intent.binding.clone(),
                    &m.model,
                    self.scope.deadline.min(self.scope.hard),
                )?,
                upstream,
                provider.retained_copy()?,
                m,
            ),
        )?;
        let origin = transport.origin().to_owned();
        let capability = transport.local_capability();
        {
            let mut physical = lock(&self.entry.physical)?;
            ensure!(
                physical.transport.is_none(),
                "native transport already owned"
            );
            if self.entry.stop.load(Ordering::SeqCst) {
                transport.cancel();
            }
            physical.transport = Some(transport);
        }
        Ok((origin, capability))
    }
    fn local(&self, installation: AdapterManifest) -> LocalEnvironment {
        let m = &self.offer.offered.offer.manifest;
        LocalEnvironment {
            resource_policy: ResourcePolicy::CallerAdmission(m.harness.environment.clone()),
            workspace: self.scope.inputs.workspace().workspace().path.clone(),
            helper_executable: installation.helper.path,
            tmux_executable: installation.tmux.path,
            tmux_version: "tmux 3.4".into(),
            credential_profiles: m
                .credentials
                .iter()
                .map(|c| c.credential_profile.clone())
                .collect(),
            context_material: vec![],
        }
    }
    fn release(
        &self,
        owner: &Arc<Mutex<Owned>>,
        prepared: PreparedLaunch,
        git_author: Option<&GitAuthor>,
    ) -> Result<()> {
        let PreparedLaunch {
            image,
            installation,
            recipe,
            private,
        } = prepared;
        let Self {
            entry,
            scope,
            profile,
            intent: i,
            ..
        } = *self;
        {
            let owner = lock(owner)?;
            entry.current(&owner)?;
        }
        let startup = image.startup(scope, i)?;
        let permit = bootstrap::permit(scope, i)?;
        let local = self.local(installation);
        let adapter = Adapter {
            recipe: recipe.with_git_author(git_author)?,
            executable: i.executable.clone(),
            renderer: i.renderer.clone(),
            private: Mutex::new(Some(private)),
        };
        let mut authority = authority::Authority {
            owner: owner.clone(),
            entry: entry.clone(),
        };
        let pending = SessionHost::prepare_observed(
            &profile.root_path,
            i.launch_id,
            &startup,
            permit,
            &local,
            &adapter,
            &mut authority,
        )?;
        let (observer, baseline) = pending.open_baseline_observer()?;
        {
            let owner = lock(owner)?;
            entry.current(&owner)?;
        }
        let released = pending.release_after_baseline(&baseline, &mut authority)?;
        authority.wait_for_running(
            released,
            observer,
            bootstrap::ObservationErrors {
                output: "native bootstrap output bound exceeded",
                expired: "native exec observation expired",
            },
        )
    }
}

fn original_material(provider: &InstalledMaterial) -> Result<OriginalPrivateMaterial> {
    let lease = &provider.metadata().delivery.lease;
    Ok(OriginalPrivateMaterial {
        digest: lease.material_digest.clone(),
        bytes: lease.material_bytes,
        record_digest: provider.receipt()?.private_binding.digest,
    })
}
