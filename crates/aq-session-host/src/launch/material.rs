//! Material and installed-environment checks performed before guardian launch.
use super::*;

impl<'a> StartupInput<'a> {
    pub(super) fn document(&self) -> LaunchDocument {
        match self {
            StartupInput::Constituents(m) => (*m).clone().into(),
            StartupInput::Rendered(r) => r.manifest.clone().into(),
            StartupInput::Observed(r) => r.rendered.manifest.clone().into(),
            StartupInput::Plan(p, _) => LaunchDocument::Plan(Box::new((*p).clone())),
        }
    }

    pub(super) fn observed(&self) -> Option<&'a observed::ObservedStartup> {
        match self {
            Self::Observed(value) => Some(value),
            _ => None,
        }
    }

    fn material(
        &self,
        local: &LocalEnvironment,
        manifest: &LaunchDocument,
        prepared: &AdapterLaunch,
    ) -> Result<(Vec<Vec<u8>>, Option<RenderedBinding>)> {
        let rendered = match self {
            StartupInput::Rendered(r) => Some(*r),
            StartupInput::Observed(r) => Some(&r.rendered),
            _ => None,
        };
        if let StartupInput::Observed(observed) = self {
            ensure!(
                prepared.command.executable == observed.program.source(),
                "observed adapter executable mismatch"
            );
        }
        let material = if let Some(rendered) = rendered {
            ensure!(
                local.context_material.is_empty(),
                "rendered startup cannot append constituent material"
            );
            rendered.binding.verify(manifest.task()?, &rendered.bytes)?;
            (vec![rendered.bytes.clone()], Some(rendered.binding.clone()))
        } else if let StartupInput::Plan(plan, bytes) = self {
            ensure!(
                local.context_material.is_empty(),
                "host plan cannot append context material"
            );
            plan.verify_startup(bytes)?;
            (vec![bytes.to_vec()], None)
        } else {
            (constituents(local, manifest)?, None)
        };
        Ok(material)
    }
}
fn constituents(local: &LocalEnvironment, manifest: &LaunchDocument) -> Result<Vec<Vec<u8>>> {
    let mut context = Vec::new();
    for reference in &manifest.task()?.context.references {
        if reference.delivery == aq_session_protocol::ContextDelivery::Startup {
            let material = local
                .context_material
                .iter()
                .find(|c| c.artifact == reference.artifact)
                .context("startup context material unavailable")?;
            let bytes = read_bytes(&material.path, 1048576)?;
            ensure!(
                bytes.len() as u64 == reference.bytes
                    && Digest::of(&bytes) == reference.artifact.digest,
                "startup context digest/size mismatch"
            );
            context.push(bytes);
        }
    }
    Ok(context)
}

fn check_environment(local: &LocalEnvironment) -> Result<()> {
    ensure!(
        local.workspace.is_absolute()
            && local.workspace.is_dir()
            && std::fs::canonicalize(&local.workspace)? == local.workspace,
        "workspace must be canonical existing directory"
    );
    ensure!(
        local.helper_executable.is_absolute()
            && local.helper_executable.is_file()
            && local.tmux_executable.is_absolute()
            && local.tmux_executable.is_file(),
        "helper/tmux unavailable"
    );
    ensure!(
        local.tmux_version.len() <= 128 && !local.tmux_version.is_empty(),
        "tmux version bounds"
    );
    let version = bounded_command(Command::new(&local.tmux_executable).arg("-V"))?;
    ensure!(
        String::from_utf8(version)?.trim() == local.tmux_version,
        "tmux version mismatch"
    );
    Ok(())
}

fn check_authority(
    manifest: &LaunchDocument,
    local: &LocalEnvironment,
    permit: &ExecutionPermit,
) -> Result<()> {
    if let LaunchDocument::Plan(plan) = manifest {
        plan.validate_permit_at(
            permit,
            supervisor::machine()?.0.parse()?,
            ownership::raw_boot_ms(),
        )?;
    }
    for secret in manifest
        .task()
        .map(|m| m.credentials.as_slice())
        .unwrap_or(&[])
    {
        ensure!(
            local
                .credential_profiles
                .contains(&secret.credential_profile),
            "credential profile unavailable"
        );
    }
    Ok(())
}

fn check_adapter(
    manifest: &LaunchDocument,
    digest: &Digest,
    prepared: &AdapterLaunch,
    local: &LocalEnvironment,
) -> Result<()> {
    ensure!(
        prepared.manifest_digest == *digest
            && prepared.adapter == manifest.harness().adapter
            && prepared.cli_version == manifest.harness().cli_version.as_str()
            && prepared.environment == manifest.harness().environment,
        "adapter/CLI/environment mismatch"
    );
    require_capabilities(
        &manifest.harness().required_capabilities,
        &prepared.capabilities,
    )?;
    if let ResourcePolicy::RequireQualifiedEnvironment(required) = &local.resource_policy {
        ensure!(
            prepared.resource_qualification.as_ref() == Some(required),
            "hard resources require exact installed-environment qualification"
        );
    }
    prepared.command.validate()?;
    Ok(())
}

fn create_directory(root: &Path, launch_id: Uuid) -> Result<PathBuf> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(root)?;
    ensure!(
        !std::fs::symlink_metadata(root)?.file_type().is_symlink(),
        "host root symlink"
    );
    let metadata = std::fs::metadata(root)?;
    ensure!(
        metadata.uid() == unsafe { libc::geteuid() } && metadata.permissions().mode() & 0o077 == 0,
        "host root must be owner-only"
    );
    let root = std::fs::canonicalize(root)?;
    let directory = root.join(launch_id.to_string());
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&directory)
        .context("launch directory already exists; never reuse")?;
    ensure!(
        directory.join("tmux.sock").as_os_str().len() < 104,
        "tmux socket path too long"
    );
    File::open(&root)?.sync_all()?;
    Ok(directory)
}

impl LaunchRequest<'_> {
    pub(super) fn prepare(
        self,
        manifest: LaunchDocument,
        adapter: Adapter<'_>,
    ) -> Result<PreparedLaunch> {
        let Self {
            root,
            launch_id,
            startup,
            permit,
            local,
            agent_identity,
        } = self;
        manifest.validate()?;
        ensure!(!launch_id.is_nil(), "nil launch ID");
        let digest = canonical_digest(&manifest)?;
        let interactive = manifest.terminal().input_enabled
            && matches!(&startup, StartupInput::Observed(value) if value.spec.interactive());
        permit.validate_at_scoped(
            manifest.session(),
            &digest,
            ownership::raw_boot_ms(),
            interactive,
        )?;

        check_environment(local)?;
        check_authority(&manifest, local, &permit)?;
        let adapter = adapter.bind(&manifest)?;
        let prepared = adapter.prepare(&local.workspace)?;
        check_adapter(&manifest, &digest, &prepared, local)?;
        let private_input = adapter.private_input(&local.workspace)?;
        ensure!(
            local.context_material.len() <= 128,
            "context material bounds"
        );

        let (context, rendered_startup) = startup.material(local, &manifest, &prepared)?;
        let directory = create_directory(root, launch_id)?;
        let publication = private_input
            .map(|input| private_inputs::Publication::reserve(&directory, input))
            .transpose()?;
        let config = SealedLaunch {
            agent_identity,
            launch_id,
            manifest,
            permit,
            workspace: local.workspace.clone(),
            helper: local.helper_executable.clone(),
            tmux: local.tmux_executable.clone(),
            tmux_version: local.tmux_version.clone(),
            command: prepared.command,
            resource_policy: local.resource_policy.clone(),
            resource_qualification: prepared.resource_qualification,
            rendered_startup,
            private_input: publication.as_ref().map(|p| p.record()),
            observed: match &startup {
                StartupInput::Observed(r) => Some(r.spec.clone()),
                _ => None,
            },
        };

        validate_config(&config, &directory)?;
        Ok(PreparedLaunch {
            directory,
            config,
            publication,
            context,
        })
    }
}
fn validate_config(config: &SealedLaunch, directory: &Path) -> Result<()> {
    if let Some(spec) = &config.observed {
        spec.validate(config)?;
    }
    if config.private_input.is_some() {
        ensure!(
            config.observed.as_ref().is_some_and(|s| s.native_exec())
                && config.command.environment.get("CODEX_HOME")
                    == Some(
                        &directory
                            .join("codex-home")
                            .to_str()
                            .context("private home UTF8")?
                            .to_owned()
                    ),
            "private input requires exact native home"
        );
    }
    Ok(())
}
