use super::*;

struct ResultFiles<'a> {
    client: &'a PathBuf,
    digest: &'a Digest,
    definition: &'a PathBuf,
}
struct Selection<'a> {
    settings: &'a CodexSettings,
    environment: &'a ArtifactRef,
    files: Option<ResultFiles<'a>>,
    clock: Option<u64>,
}
impl<'a> Selection<'a> {
    fn from_config(config: &'a NativeConfig) -> Result<Self> {
        let interactive_clock = match &config.executor {
            Some(ExecutorConfig::ChatgptInteractiveResultLocal {
                terminal_clock_skew_ms,
                ..
            }) => {
                ensure!(
                    *terminal_clock_skew_ms <= 1000,
                    "interactive operator clock bound"
                );
                Some(*terminal_clock_skew_ms)
            }
            _ => None,
        };
        let (settings, environment, result) = match &config.executor {
            Some(ExecutorConfig::TrustedNodeNamedReadonly {
                settings,
                environment,
            }) => (settings, environment, None),
            Some(ExecutorConfig::TrustedNodeNamedReadonlyResultLocal {
                settings,
                environment,
                client_manifest,
                client_manifest_digest,
                composite_definition,
            })
            | Some(ExecutorConfig::ChatgptInteractiveResultLocal {
                settings,
                environment,
                client_manifest,
                client_manifest_digest,
                composite_definition,
                ..
            }) => (
                settings,
                environment,
                Some(ResultFiles {
                    client: client_manifest,
                    digest: client_manifest_digest,
                    definition: composite_definition,
                }),
            ),
            Some(ExecutorConfig::ClaudeDockerLocalResultLocal { .. }) => {
                anyhow::bail!("Claude docker-local settings select the Claude executor")
            }
            Some(ExecutorConfig::Codex { .. }) => {
                anyhow::bail!(
                    "legacy sealed-memfd executor is not qualified for Codex tools; no admission attempted"
                )
            }
            None => anyhow::bail!("closed Codex executor settings absent"),
        };
        Ok(Self {
            settings,
            environment,
            files: result,
            clock: interactive_clock,
        })
    }
    fn validate(&self, config: &NativeConfig, executor: &CodexExecutor) -> Result<()> {
        self.settings.validate(config)?;
        // Before any installation work: container needs the sandbox's proof.
        let mode = crate::native::adapter::codex::selected_confinement(
            &self.settings.harness,
            executor.confinement.as_ref(),
            self.clock.is_some(),
        )?;
        ensure!(
            self.files.is_none() || mode == ConfinementMode::Native,
            "result-local Codex mapping is qualified for native confinement only"
        );
        ensure!(
            config.adapter.id.as_str() != "aq.codex-result-local-adapter" || self.files.is_some(),
            "known result adapter requires the result-enabled executor"
        );
        ensure!(
            self.environment == &self.settings.harness.environment,
            "trusted-node environment differs from selected harness"
        );
        ensure!(
            self.files.is_none() || executor.result_transfer.is_some(),
            "native result source transport absent"
        );
        Ok(())
    }
    fn definition(&self, config: &NativeConfig) -> Result<Option<ResultAdapterDefinition>> {
        let Some(files) = &self.files else {
            return Ok(None);
        };
        let definition =
            ResultAdapterDefinition::decode(&config::private_file(files.definition, 16384)?)?;
        ensure!(
            definition.interactive() == self.clock.is_some(),
            "executor and selected result mode differ"
        );
        ensure!(
            !definition.is_claude_docker() && !definition.is_codex_docker(),
            "docker-local result definitions select the docker-local executor"
        );
        definition.validate_selected(ExpectedResultAdapter {
            adapter: &config.adapter,
            runtime: &config.runtime,
            environment: self.environment,
        })?;
        ensure!(
            definition.client_manifest.digest == *files.digest,
            "result client installation differs from selected adapter"
        );
        Ok(Some(definition))
    }
    fn installation(
        &self,
        config: &NativeConfig,
    ) -> Result<(NamedReadonlyEnvironment, NamedExecInstallation)> {
        // An artifact reference is not a kernel observation. This retained owner
        // checks cap/NNP and namespace premises; installation loading checks actual
        // readonly mount/FD/path identities. External provisioning qualification
        // remains independently pinned by the selected environment artifact.
        let environment_owner = NamedReadonlyEnvironment::inspect(self.environment.clone())?;
        let pool = NamedReadonlyExecPool::new(
            CODEX_INSTALLATION_BUDGET_BYTES,
            1,
            environment_owner.clone(),
        )?;
        let manifest =
            NamedExecManifest::decode(&config::private_file(&config.adapter_manifest, 16384)?)?;
        let installed = NamedExecInstallation::open(
            manifest,
            ExpectedAdapter {
                manifest_digest: &config.adapter_manifest_digest,
                adapter: &config.adapter,
                runtime: &config.runtime.runtime,
                qualification: &config.runtime.qualification,
                renderer: &config.renderer,
            },
            self.environment,
            &pool,
        )?;
        ensure!(
            installed.interactive() == self.clock.is_some(),
            "executor and selected named image mode differ"
        );
        Ok((environment_owner, installed))
    }
    fn profile(
        &self,
        config: &NativeConfig,
        executor: &CodexExecutor,
        definition: Option<ResultAdapterDefinition>,
    ) -> Result<NativeCodexHostProfile> {
        let (environment_owner, installed) = self.installation(config)?;
        let mapper = CodexExecProfile {
            harness: self.settings.harness.clone(),
            permissions_digest: self.settings.permissions_digest.clone(),
            renderer: config.renderer.clone(),
            confinement: executor.confinement.clone(),
        };
        let profile = match (&self.files, definition) {
            (Some(files), Some(definition)) => {
                let manifest =
                    ResultClientManifest::decode(&config::private_file(files.client, 16384)?)?;
                let client = InstalledResultClient::open_selected(
                    manifest,
                    definition.expected_client()?,
                    &definition.client_manifest,
                    &environment_owner,
                )?;
                NativeCodexHostProfile::new_named_result(
                    self.settings.host_root.clone(),
                    installed,
                    mapper,
                    self.settings.columns,
                    self.settings.rows,
                    client,
                    definition,
                )?
            }
            (None, None) => NativeCodexHostProfile::new_named(
                self.settings.host_root.clone(),
                installed,
                mapper,
                self.settings.columns,
                self.settings.rows,
            )?,
            _ => anyhow::bail!("native result factory selection differs"),
        };
        Ok(profile)
    }
}
impl CodexExecutor {
    pub(super) fn initialize(&mut self, config: &NativeConfig) -> Result<()> {
        ensure!(self.profile.is_none(), "Codex executor already preflighted");
        let selection = Selection::from_config(config)?;
        selection.validate(config, self)?;
        let definition = selection.definition(config)?;
        let selected_client_manifest = definition.as_ref().map(|d| d.client_manifest.clone());
        let profile = selection.profile(config, self, definition)?;
        let profile = if let Some(skew) = selection.clock {
            profile.with_interactive_clock(skew)?
        } else {
            profile
        };
        #[cfg(feature = "test-support")]
        let profile = if let Some(origin) = &self.development_provider {
            profile.with_development_provider(origin)?
        } else {
            profile
        };
        let result_pins = profile.result_installation_pins();
        ensure!(
            result_pins
                .as_ref()
                .is_none_or(|pins| pins.adapter == config.adapter)
                && result_pins.as_ref().map(|pins| &pins.client_manifest)
                    == selected_client_manifest.as_ref(),
            "native result installation differs from selected definition revisions"
        );
        self.installation_reserved_bytes = Some(
            CODEX_INSTALLATION_BUDGET_BYTES
                .checked_add(
                    result_pins
                        .as_ref()
                        .map_or(0, |pins| pins.reserved_client_bytes),
                )
                .context("native installation image reservation overflow")?,
        );
        self.result_root = result_pins.map(|_| config.state_dir.join("result"));
        self.profile = Some(Arc::new(profile));
        Ok(())
    }
}
