//! MS1 production host profile: the private session host runs the node-side
//! docker-local driver (`crate::sandbox_driver`) as its single pane program.
//! The driver, not this process, owns the sandbox; this profile owns only the
//! host, its pinned programs and the driver's private launch inputs.
use super::*;
use crate::backend::docker_local::DockerLocalConfig;
use crate::backend::{Ownership, ResourceLimits, TemplateDigest};
use aq_session_host::{
    AdapterLaunch, HarnessAdapter, HarnessReadiness, LocalCommand,
    observed::{
        MAX_NATIVE_EXEC_BYTES, MAX_NATIVE_EXEC_POOL_BYTES, NativeExecObservationProfile,
        NativeExecPool, ObservedAdapterQualification, ObservedHarnessAdapter,
    },
};
use aq_session_protocol::{ArtifactRef, Validate};
use serde::{Deserialize, Serialize};
use std::{
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PinnedFile {
    pub path: PathBuf,
    pub artifact: ArtifactRef,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaudeDockerHostConfig {
    pub root: PathBuf,
    pub helper: PinnedFile,
    pub tmux: PinnedFile,
    pub renderer: ArtifactRef,
    pub columns: u16,
    pub rows: u16,
    pub docker_local: DockerLocalConfig,
    pub template: TemplateDigest,
    /// Claude Code inside the template image.
    pub claude_executable: PathBuf,
    pub max_workspace_bytes: u64,
    /// Spec 0006 §5.4: every attempt gets an agent channel or does not
    /// launch. Only an explicit `"disabled"` runs agents with no path back.
    #[serde(default, skip_serializing_if = "ChannelMode::is_required")]
    pub agent_channel: ChannelMode,
    /// Per-project warm build caches (`aq_sandbox_archive::build_cache`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_cache: Option<aq_sandbox_archive::build_cache::BuildCacheConfig>,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelMode {
    #[default]
    Required,
    Disabled,
}
impl ChannelMode {
    fn is_required(&self) -> bool {
        *self == Self::Required
    }
}
/// Codex in the same docker-local sandbox template, beside Claude.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodexDockerHost {
    /// Codex CLI inside the template image (`/opt/aq/bin/codex`).
    pub executable: PathBuf,
    /// The pinned Codex helper bundle on the node, uploaded to each Codex sandbox.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_bundle: Option<PathBuf>,
}
/// The harness an offer selects on a docker-local node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DockerHarness {
    Claude,
    Codex,
}
/// The offered manifest's `harness.harness` to the harness this node launches.
/// Codex needs the node's Codex configuration; anything else is refused.
pub fn select_harness(harness: &str, codex_configured: bool) -> Result<DockerHarness> {
    match harness {
        "claude" => Ok(DockerHarness::Claude),
        "codex" if codex_configured => Ok(DockerHarness::Codex),
        "codex" => anyhow::bail!("this docker-local node has no Codex harness configured"),
        other => anyhow::bail!("docker-local harness {other:?} is not supported"),
    }
}
/// In-sandbox private home of a Codex attempt (`CODEX_HOME`).
pub const CODEX_HOME: &str = "/aq/home/codex";
pub struct ClaudeDockerHostProfile {
    pub(super) config: ClaudeDockerHostConfig,
    pub(super) root: RootGuard,
    pub(super) digest: Digest,
    driver: PinnedFile,
    pool: NativeExecPool,
    /// The node's enrollment, for agent-credential issuance. Memory only.
    issuance: Option<aq_agent_channel::NodeLink>,
    conversation_outbox: Option<PathBuf>,
    /// Codex beside Claude; None runs Claude only.
    codex: Option<CodexDockerHost>,
    /// The node's observation store; None leaves attempts unobserved.
    pub(crate) observations: Option<aq_observation_store::node::Observations>,
    pub(crate) transcript_bound: Option<aq_handoff_note::TranscriptBound>,
}
use aq_sandbox_archive::hash_file;
/// The driver is this worker's own executable, so the two are always one build.
pub fn own_driver() -> Result<(PinnedFile, u64)> {
    let path = std::fs::canonicalize(std::env::current_exe()?)?;
    let (digest, bytes) = hash_file(&path, MAX_NATIVE_EXEC_BYTES)?;
    let pin = serde_json::json!({"id": "aq.claude-docker-local-driver", "version": env!("CARGO_PKG_VERSION"), "digest": digest});
    let artifact = serde_json::from_value(pin)?;
    Ok((PinnedFile { path, artifact }, bytes))
}
impl ClaudeDockerHostConfig {
    fn validate_dimensions(&self) -> Result<()> {
        ensure!(
            self.columns >= 80
                && self.columns <= 240
                && self.rows > 0
                && self.rows <= 120
                && u32::from(self.columns) * u32::from(self.rows) <= 12000
                && self.claude_executable.is_absolute()
                && self.max_workspace_bytes > 0,
            "closed Claude docker-local host profile"
        );
        Ok(())
    }
}
impl ClaudeDockerHostProfile {
    pub fn new(config: ClaudeDockerHostConfig) -> Result<Self> {
        config.validate_dimensions()?;
        for pin in [&config.helper, &config.tmux] {
            pin.artifact.id.validate()?;
            pin.artifact.version.validate()?;
            ensure!(
                pin.path.is_absolute() && std::fs::canonicalize(&pin.path)? == pin.path,
                "pinned program path canonical"
            );
        }
        ensure!(
            config
                .docker_local
                .templates
                .iter()
                .any(|t| t.digest == config.template),
            "Claude template is not allowlisted for docker-local"
        );
        // No mount alias may expose the channel directories (0006 §5.1).
        (config.docker_local).check_channel_root(&config.root.join("channels"))?;
        let root = RootGuard::open(&config.root, None)?;
        let digest = crate::native::journal::canonical_digest(&config)?;
        let (driver, driver_bytes) = own_driver()?;
        let pool_bytes = (driver_bytes + (1 << 20)).min(MAX_NATIVE_EXEC_POOL_BYTES);
        let pool = NativeExecPool::new(pool_bytes, 1)?;
        let profile = Self {
            config,
            root,
            digest,
            driver,
            pool,
            issuance: None,
            conversation_outbox: None,
            codex: None,
            observations: None,
            transcript_bound: None,
        };
        profile.verify_installation()?;
        // Channels die with their node process; drop what a dead one left.
        aq_agent_channel::sweep(&profile.channel_root())?;
        Ok(profile)
    }
    pub fn verify_installation(&self) -> Result<()> {
        self.root.verify()?;
        for pin in [&self.config.helper, &self.config.tmux, &self.driver] {
            let (digest, _) =
                hash_file(&pin.path, aq_session_host::observed::MAX_NATIVE_EXEC_BYTES)?;
            ensure!(
                digest == pin.artifact.digest.as_str(),
                "pinned program changed"
            );
        }
        Ok(())
    }
    pub fn config(&self) -> &ClaudeDockerHostConfig {
        &self.config
    }
    pub fn with_issuance(mut self, issuance: aq_agent_channel::NodeLink) -> Self {
        self.issuance = Some(issuance);
        self
    }
    pub(crate) fn with_conversation_outbox(mut self, directory: Option<PathBuf>) -> Self {
        self.conversation_outbox = directory;
        self
    }
    /// Lets offers select Codex as well. The profile digest then covers the Codex
    /// settings too; a Claude-only profile keeps its digest.
    pub fn with_codex(mut self, codex: CodexDockerHost) -> Result<Self> {
        ensure!(
            codex.executable.is_absolute()
                && codex.executable.to_str().is_some()
                && codex.executable != self.config.claude_executable,
            "closed Codex docker-local host profile"
        );
        self.digest = crate::native::journal::canonical_digest(&(&self.config, &codex))?;
        self.codex = Some(codex);
        Ok(self)
    }
    pub fn codex(&self) -> Option<&CodexDockerHost> {
        self.codex.as_ref()
    }
    fn channel_root(&self) -> PathBuf {
        self.config.root.join("channels")
    }
    /// 0006 §5.4 Launch: credential issued and proxy listening before the
    /// driver (and so the sandbox) exists; any failure refuses the launch.
    fn open_channel(
        &self,
        session: &aq_session_protocol::SessionIdentity,
        not_after_ms: u64,
    ) -> Result<aq_agent_channel::AgentChannel> {
        use aq_agent_channel::{AttemptIdentity, UNAVAILABLE};
        let link = self
            .issuance
            .as_ref()
            .with_context(|| format!("{UNAVAILABLE}: the node has no credential issuance"))?;
        let aq_session_protocol::SessionIdentity::Remote {
            node_id,
            boot_id,
            attempt_id,
            assignment_generation,
            session_id,
        } = *session
        else {
            anyhow::bail!("{UNAVAILABLE}: not a remote node session");
        };
        let identity = AttemptIdentity {
            node_id,
            boot_id,
            attempt_id,
            session_id,
            generation: assignment_generation,
        };
        std::fs::DirBuilder::new()
            .mode(0o700)
            .recursive(true)
            .create(self.channel_root())?;
        let directory = self.channel_root().join(attempt_id.to_string());
        tokio::runtime::Handle::try_current()?.block_on(link.open(
            identity,
            directory,
            not_after_ms,
        ))
    }
}
/// `aq environment try` (aq-server spec 0017 §6.2): the running sandbox's
/// `/aq/work`, read as the copy-back reads it, refused on a credential shape,
/// then committed and pushed to the attempt's try ref (aq-node-git).
#[cfg(feature = "project_environments")]
fn try_checkpoint(
    profile: &ClaudeDockerHostProfile,
    node: Arc<aq_node_git::Node>,
    (node_id, attempt): (uuid::Uuid, uuid::Uuid),
    workspace: PathBuf,
    author: Option<aq_worker_protocol::git_author::GitAuthor>,
) -> aq_agent_channel::Checkpoint {
    use crate::backend::{PlacementBackend as _, SandboxState, docker_local as dl};
    use aq_agent_channel::CheckpointError::{PushFailed, Refused};
    let (docker, max) = (
        profile.config.docker_local.clone(),
        profile.config.max_workspace_bytes,
    );
    Arc::new(move |guard| {
        let (docker, node, workspace) = (docker.clone(), node.clone(), workspace.clone());
        let author = author.clone();
        Box::pin(async move {
            let read = async {
                let mut backend = dl::DockerLocalBackend::connect_unix(docker).await?;
                let owned = backend.list_owned(node_id).await?.into_iter();
                let mut running = owned.filter(|e| {
                    e.sandbox.owner.attempt_id == attempt && e.state == SandboxState::Running
                });
                let (Some(sandbox), None) = (running.next(), running.next()) else {
                    anyhow::bail!("no single running sandbox for the attempt");
                };
                let bound = usize::try_from(max)?.saturating_mul(2);
                let archive = backend.get_archive(&sandbox.sandbox, dl::WORK_TARGET, bound);
                anyhow::Ok(dl::tar::read_tree(&archive.await?, max)?)
            };
            let failed = |error: anyhow::Error| {
                tracing::warn!(attempt_id = %attempt, error = %format!("{error:#}"), "try checkpoint failed");
                PushFailed
            };
            let entries = read.await.map_err(failed)?;
            if aq_sandbox_archive::workspace::credential_shaped(&entries) {
                return Err(Refused);
            }
            let fill =
                move |tree: &Path| crate::sandbox_driver::sync_tree(tree, &entries).map(drop);
            let message = format!("AQ attempt {attempt}");
            node.checkpoint(workspace, message, author, fill, guard)
                .await
                .map_err(failed)
        })
    })
}
pub(super) struct Adapter {
    command: LocalCommand,
    executable: ArtifactRef,
    renderer: ArtifactRef,
    manifest_digest: Digest,
}
impl HarnessAdapter for Adapter {
    fn prepare(&self, m: &aq_session_protocol::LaunchManifest, _: &Path) -> Result<AdapterLaunch> {
        ensure!(
            crate::native::journal::canonical_digest(m)? == self.manifest_digest,
            "Claude driver manifest changed"
        );
        Ok(AdapterLaunch {
            resource_qualification: None,
            manifest_digest: self.manifest_digest.clone(),
            adapter: m.harness.adapter.clone(),
            cli_version: m.harness.cli_version.as_str().into(),
            environment: m.harness.environment.clone(),
            capabilities: m.harness.required_capabilities.clone(),
            command: self.command.clone(),
        })
    }
    fn readiness(&self, _: &HostObservation) -> Result<HarnessReadiness> {
        Ok(HarnessReadiness::Unknown)
    }
}
impl ObservedHarnessAdapter for Adapter {
    fn observed_qualification(
        &self,
        _: &aq_session_protocol::LaunchManifest,
    ) -> Result<ObservedAdapterQualification> {
        Ok(ObservedAdapterQualification {
            executable: self.executable.clone(),
            renderer: self.renderer.clone(),
        })
    }
}
use aq_sandbox_archive::{TokenGuard, private_write};
/// What one harness puts into the driver: its private login material and the
/// in-sandbox command.
struct HarnessLaunch {
    token: zeroize::Zeroizing<Vec<u8>>,
    executable: String,
    arguments: Vec<String>,
    environment: std::collections::BTreeMap<String, String>,
    settings_json: String,
    harness: crate::sandbox_driver::DriverHarness,
    codex_runtime_bundle: Option<PathBuf>,
    /// The public half of a leased Codex login, kept for its write-back.
    login_lease: Option<aq_agent_channel::login::LoginLease>,
}
/// Exactly one material of `provider` that this manifest asks for.
fn provider_material<'a>(
    scope: &'a LaunchExecutionScope,
    m: &aq_session_protocol::LaunchManifest,
    provider: &str,
) -> Vec<&'a crate::native::materials::InstalledMaterial> {
    scope
        .inputs
        .materials()
        .iter()
        .filter(|material| {
            let lease = &material.metadata().delivery.lease;
            lease.provider.as_str() == provider
                && m.credentials.iter().any(|r| {
                    r.credential_profile == lease.credential_profile
                        && r.purpose.as_str() == lease.purpose.as_str()
                })
        })
        .collect()
}
fn claude_launch(
    scope: &LaunchExecutionScope,
    m: &aq_session_protocol::LaunchManifest,
    profile: &ClaudeDockerHostProfile,
) -> Result<HarnessLaunch> {
    use crate::native::runner::sandbox::confinement::ConfinementMode;
    // Exactly one Anthropic subscription material for this manifest.
    let logins = provider_material(scope, m, "anthropic");
    ensure!(
        logins.len() == 1,
        "Claude docker-local requires one exact Anthropic login material"
    );
    let token = logins[0].read_private()?;
    let token = crate::sandbox_driver::parse_token(&token)?;
    let mode = ConfinementMode::declared(m)?;
    ensure!(
        mode == ConfinementMode::Container,
        "Claude docker-local runs in container confinement only (C2: no user-namespace mapping)"
    );
    let home = PathBuf::from("/aq/home");
    let recipe = crate::native::adapter::claude::map(
        &m.harness,
        &m.model,
        crate::native::adapter::claude::ClaudePaths {
            executable: &profile.config.claude_executable,
            workspace: Path::new(crate::backend::docker_local::config::WORK_TARGET),
            private_home: &home,
        },
        mode,
    )?;
    // Owner decision 2026-11-18: the interactive TUI on the sandbox's terminal.
    let mut arguments = recipe.arguments;
    let settings_json =
        aq_handoff_note::harness::claude_interactive(&mut arguments, &recipe.settings_json)?;
    Ok(HarnessLaunch {
        token,
        executable: recipe
            .executable
            .to_str()
            .context("Claude executable UTF-8")?
            .into(),
        arguments,
        environment: recipe.environment,
        settings_json,
        harness: crate::sandbox_driver::DriverHarness::Claude,
        codex_runtime_bundle: None,
        login_lease: None,
    })
}
/// Codex in the same template: the ChatGPT login (`auth.json`) is this attempt's
/// private material, delivered like the Claude token; the runner installs it
/// read-only in the attempt's own `CODEX_HOME` and nothing is written back.
fn codex_launch(
    scope: &LaunchExecutionScope,
    m: &aq_session_protocol::LaunchManifest,
    profile: &ClaudeDockerHostProfile,
) -> Result<HarnessLaunch> {
    use crate::native::runner::sandbox::confinement::ConfinementMode;
    let codex = profile
        .codex
        .as_ref()
        .context("Codex docker-local settings absent")?;
    let logins = provider_material(scope, m, "openai");
    ensure!(
        logins.len() == 1,
        "Codex docker-local requires one exact OpenAI ChatGPT login material"
    );
    let auth = logins[0].read_private()?;
    aq_session_host::PrivateLaunchInput::validate_chatgpt_auth_json(&auth)?;
    // C5b: without the stored version there is no base for a write-back.
    let record = logins[0].metadata();
    let lease = &record.delivery.lease;
    let login_lease = (record.login_version)
        .map(|version| {
            aq_agent_channel::login::LoginLease::of(
                &auth,
                serde_json::to_value(&record.worker)?,
                serde_json::to_value(&lease.binding)?,
                lease.lease_id,
                version,
            )
        })
        .transpose()?;
    let mode = ConfinementMode::declared(m)?;
    ensure!(
        mode == ConfinementMode::Container,
        "Codex docker-local runs in container confinement only"
    );
    let channel = profile.config.agent_channel.is_required().then_some(
        aq_codex_mapper::docker_local::AgentChannel {
            socket: aq_agent_channel::AQ_CHANNEL,
            bin_dir: aq_agent_channel::AQ_BIN,
        },
    );
    let recipe = aq_codex_mapper::docker_local::map_docker_local(
        aq_codex_mapper::Settings {
            harness: &m.harness,
            model: &m.model,
            terminal_input_enabled: m.terminal.input_enabled,
        },
        aq_codex_mapper::Paths {
            executable: &codex.executable,
            workspace: Path::new(crate::backend::docker_local::config::WORK_TARGET),
            private_home: Path::new(CODEX_HOME),
        },
        channel,
    )?;
    Ok(HarnessLaunch {
        token: auth,
        executable: recipe
            .executable
            .to_str()
            .context("Codex executable UTF-8")?
            .into(),
        arguments: recipe.arguments,
        environment: recipe.environment,
        settings_json: String::new(),
        harness: crate::sandbox_driver::DriverHarness::Codex,
        codex_runtime_bundle: codex.runtime_bundle.clone(),
        login_lease,
    })
}
/// Registers the launched attempt's transcript with the node's observation store;
/// `account` is the account reference of the launch's login lease of a provider.
/// Nothing here can fail the launch: an attempt without a source runs unobserved.
pub fn observe(
    observations: Option<&aq_observation_store::node::Observations>,
    offer: &aq_session_protocol::ProfiledOffer,
    account: impl Fn(&str) -> Option<String>,
    transcript: PathBuf,
) -> Option<aq_observation_store::node::SourceHandle> {
    use aq_observation_store::node::SourceSpec;
    let (m, attempt_id) = (&offer.manifest, offer.binding.attempt_id);
    let harness = m.harness.harness.as_str();
    let provider = ["anthropic", "openai"][usize::from(harness == "codex")];
    let registered = (|| {
        let account = account(provider).context("login lease absent")?;
        let binding = aq_admission::transport::lifecycle_binding(offer)?;
        let (store, modules) = (observations, &m.modules);
        let spec = SourceSpec::of_launch(binding, modules, harness, transcript, provider, &account);
        let registered = spec?.and_then(|spec| Some(store?.register(spec)));
        registered.transpose()
    })();
    match &registered {
        Ok(None) => tracing::info!(%attempt_id, "no observation store or catalog entry"),
        Err(error) => tracing::warn!(%attempt_id, "transcript not observed: {error:#}"),
        Ok(Some(_)) => {}
    }
    registered.ok().flatten()
}
mod prepare;
pub(super) use prepare::bootstrap;
