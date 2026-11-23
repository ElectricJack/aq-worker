//! Closed Codex exec input mapping. Preparing inputs grants no execution or
//! runtime qualification. The concrete host owner must verify them at exec.
use super::*;
use crate::native::runner::sandbox::confinement::{AuthorizedConfinement, ConfinementMode};
use anyhow::Context;
use aq_admission::startup::NativeOffer;
use aq_session_host::LocalCommand;
use aq_session_protocol::HarnessSettings;
use std::{io::Write, path::Path};

pub use aq_codex_mapper::CODEX_CLI_VERSION;
const MAX_EVENT: usize = 32768;
mod chatgpt;
pub(crate) use chatgpt::validated_chatgpt_auth_json;

/// Operator-selected exact settings, not settings inferred from an offer.
pub struct CodexExecProfile {
    pub harness: HarnessSettings,
    /// A template's legacy key: never compared with an offered plan (spec 0014);
    /// it only keeps an installed host profile's digest unchanged.
    pub permissions_digest: Option<Digest>,
    pub renderer: ArtifactRef,
    /// The sandbox's proof for container confinement. `None` maps native only;
    /// a container-declaring harness is refused without a `Container` proof.
    pub confinement: Option<AuthorizedConfinement>,
}

/// The confinement mode the selected harness declares, joined with its
/// authorization (0005 SBX13/SBX19). Exactly one mode may be declared; a
/// harness declaring neither keeps the legacy native mapping, which the frozen
/// manifest's own validation already refuses. Container requires an
/// `AuthorizedConfinement` for `Container` and is unqualified for interactive.
pub(crate) fn selected_confinement(
    harness: &HarnessSettings,
    confinement: Option<&AuthorizedConfinement>,
    interactive: bool,
) -> Result<ConfinementMode> {
    let declares = |id: &str| {
        harness
            .required_capabilities
            .iter()
            .any(|c| c.id.as_str() == id && c.version == 1)
    };
    let container = declares(aq_session_protocol::CONTAINER_CONFINEMENT_CAPABILITY);
    ensure!(
        !(container && declares(aq_session_protocol::NATIVE_CONFINEMENT_CAPABILITY)),
        "Codex harness declares both confinement modes"
    );
    let mode = if container {
        ConfinementMode::Container
    } else {
        ConfinementMode::Native
    };
    match (mode, confinement.map(AuthorizedConfinement::mode)) {
        (ConfinementMode::Native, None | Some(ConfinementMode::Native)) => {}
        (ConfinementMode::Container, Some(ConfinementMode::Container)) => ensure!(
            !interactive,
            "container confinement is not qualified for interactive Codex"
        ),
        (ConfinementMode::Container, None) => {
            anyhow::bail!("container confinement requires an AuthorizedConfinement")
        }
        (declared, Some(authorized)) => anyhow::bail!(
            "declared {} confinement differs from authorized {}",
            declared.as_str(),
            authorized.as_str()
        ),
    }
    Ok(mode)
}

/// Exact retained inputs for the native-specific mapper. This owns no lease.
pub struct CodexExecInput<'a> {
    pub offer: &'a NativeOffer,
    pub expected_manifest: &'a Digest,
    pub payload: Vec<u8>,
    pub workspace: &'a Path,
    pub private_home: &'a Path,
}
struct CodexInstallation<'a> {
    executable: &'a Path,
    adapter: &'a ArtifactRef,
    renderer: &'a ArtifactRef,
}
impl<'a> From<&'a AdapterManifest> for CodexInstallation<'a> {
    fn from(manifest: &'a AdapterManifest) -> Self {
        Self {
            executable: &manifest.executable.path,
            adapter: &manifest.adapter,
            renderer: &manifest.renderer,
        }
    }
}

#[derive(Clone, Copy)]
enum Home {
    Existing,
    Planned,
    InteractivePlanned,
}

fn canonical_directory(path: &Path) -> Result<bool> {
    Ok(path.is_absolute() && path.is_dir() && std::fs::canonicalize(path)? == path)
}
fn validate_startup(offer: &NativeOffer, payload: &[u8]) -> Result<()> {
    let descriptor = &offer.startup;
    let manifest = &offer.offered.offer.manifest;
    descriptor.validate()?;
    descriptor.verify_payload(payload)?;
    ensure!(
        manifest.modules.contains(&descriptor.artifact()?)
            && manifest
                .modules
                .iter()
                .filter(|a| a.id.as_str().starts_with("aq.startup-delivery/"))
                .count()
                == 1
            && payload.len() as u64 <= manifest.context.startup_byte_budget,
        "Codex exact startup descriptor mismatch"
    );
    Ok(())
}

impl Home {
    fn recipe(
        self,
        settings: aq_codex_mapper::Settings<'_>,
        paths: aq_codex_mapper::Paths<'_>,
        payload: &[u8],
    ) -> Result<aq_codex_mapper::Recipe> {
        if matches!(self, Self::InteractivePlanned) {
            ensure!(
                !payload.is_empty()
                    && payload.len() <= 64 * 1024
                    && !payload.contains(&0)
                    && std::str::from_utf8(payload).is_ok(),
                "interactive verified startup UTF8/64KiB bound"
            );
            aq_codex_mapper::map_interactive(settings, paths)
        } else {
            aq_codex_mapper::map(settings, paths)
        }
    }

    fn validate(self, workspace: &Path, private_home: &Path) -> Result<()> {
        let planned = !matches!(self, Self::Existing);
        ensure!(
            canonical_directory(workspace)?
                && private_home.is_absolute()
                && (planned || canonical_directory(private_home)?)
                && !private_home.starts_with(workspace),
            "Codex private configuration/workspace paths"
        );
        self.validate_contents(private_home)
    }

    fn validate_contents(self, private_home: &Path) -> Result<()> {
        if !matches!(self, Self::Existing) {
            ensure!(
                matches!(std::fs::symlink_metadata(private_home),Err(ref e) if e.kind()==std::io::ErrorKind::NotFound),
                "private planned home already exists"
            );
        } else {
            use std::os::unix::fs::MetadataExt;
            let meta = std::fs::symlink_metadata(private_home)?;
            ensure!(
                meta.is_dir()
                    && meta.uid() == unsafe { libc::geteuid() }
                    && meta.mode() & 0o777 == 0o700,
                "Codex private home ownership/mode"
            );
            ensure!(
                std::fs::read_dir(private_home)?.next().is_none(),
                "Codex private home must be fresh and empty"
            );
        }
        Ok(())
    }
}

/// No Serialize or Debug: payload and derived authentication are sensitive.
/// No spawn/exec method: this is input preparation, not launch authority.
pub struct PreparedCodexExec {
    command: LocalCommand,
    startup: Zeroizing<Vec<u8>>,
    manifest_digest: Digest,
}
impl CodexExecProfile {
    pub fn prepare(
        &self,
        installed: &mut InstalledAdapter,
        offer: &NativeOffer,
        expected_manifest: &Digest,
        payload: Vec<u8>,
        workspace: &Path,
        private_home: &Path,
    ) -> Result<PreparedCodexExec> {
        let prepared = self.prepare_inputs(
            CodexExecInput {
                offer,
                expected_manifest,
                payload,
                workspace,
                private_home,
            },
            CodexInstallation {
                executable: &installed.files[0].pin.path,
                adapter: &installed.adapter,
                renderer: &installed.renderer,
            },
            Home::Existing,
        )?;
        installed.revalidate()?;
        Ok(prepared)
    }
    /// Reuses the independently admitted immutable executable image; no full
    /// 319MiB executable rehash on an attempt's original execution clock.
    pub fn prepare_native(
        &self,
        installation: &mut super::native_exec::NativeExecInstallation,
        input: CodexExecInput<'_>,
    ) -> Result<PreparedCodexExec> {
        self.prepare_native_home(installation, input, Home::Existing)
    }
    pub(crate) fn prepare_native_planned(
        &self,
        installation: &mut super::native_exec::NativeExecInstallation,
        input: CodexExecInput<'_>,
    ) -> Result<PreparedCodexExec> {
        self.prepare_native_home(installation, input, Home::Planned)
    }
    fn prepare_native_home(
        &self,
        installation: &mut super::native_exec::NativeExecInstallation,
        input: CodexExecInput<'_>,
        home: Home,
    ) -> Result<PreparedCodexExec> {
        let prepared = self.prepare_inputs(input, installation.installation().into(), home)?;
        installation.revalidate()?;
        Ok(prepared)
    }
    pub(crate) fn prepare_named_planned(
        &self,
        installation: &mut super::named_exec::NamedExecInstallation,
        input: CodexExecInput<'_>,
    ) -> Result<PreparedCodexExec> {
        let home = if installation.interactive() {
            Home::InteractivePlanned
        } else {
            Home::Planned
        };
        let prepared = self.prepare_inputs(input, installation.installation().into(), home)?;
        installation.revalidate()?;
        Ok(prepared)
    }
    fn prepare_inputs(
        &self,
        input: CodexExecInput<'_>,
        installation: CodexInstallation<'_>,
        home: Home,
    ) -> Result<PreparedCodexExec> {
        let interactive = matches!(home, Home::InteractivePlanned);
        let CodexExecInput {
            offer,
            expected_manifest,
            payload,
            workspace,
            private_home,
        } = input;
        offer.validate()?;
        let manifest = &offer.offered.offer.manifest;
        ensure!(
            super::super::journal::canonical_digest(manifest)? == *expected_manifest
                && manifest.harness == self.harness
                && *installation.adapter == self.harness.adapter
                && *installation.renderer == self.renderer,
            "Codex exact installed settings mismatch"
        );
        let settings = aq_codex_mapper::Settings {
            harness: &self.harness,
            model: &manifest.model,
            terminal_input_enabled: manifest.terminal.input_enabled,
        };
        if interactive {
            aq_codex_mapper::validate_interactive_settings(settings)?;
        } else {
            aq_codex_mapper::validate_settings(settings)?;
        }
        validate_startup(offer, &payload)?;
        home.validate(workspace, private_home)?;
        let paths = aq_codex_mapper::Paths {
            executable: installation.executable,
            workspace,
            private_home,
        };
        let mode = selected_confinement(&self.harness, self.confinement.as_ref(), interactive)?;
        let recipe = match mode {
            ConfinementMode::Container => aq_codex_mapper::map_container(settings, paths)?,
            ConfinementMode::Native => home.recipe(settings, paths, &payload)?,
        };
        Ok(PreparedCodexExec {
            command: LocalCommand {
                executable: recipe.executable,
                arguments: recipe.arguments,
                environment: recipe.environment,
            },
            startup: Zeroizing::new(payload),
            manifest_digest: expected_manifest.clone(),
        })
    }
}
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn development_origin(origin: &str) -> Result<String> {
    scoped_origin(origin)
}
pub(crate) fn scoped_origin(origin: &str) -> Result<String> {
    let url = reqwest::Url::parse(origin)?;
    ensure!(
        url.scheme() == "http"
            && url.host_str() == Some("127.0.0.1")
            && url.port().is_some_and(|p| p > 0)
            && url.username().is_empty()
            && url.password().is_none()
            && url.path() == "/"
            && url.query().is_none()
            && url.fragment().is_none(),
        "development provider requires numeric IPv4 loopback origin"
    );
    Ok(url.as_str().trim_end_matches('/').to_owned())
}
impl PreparedCodexExec {
    pub(crate) fn with_scoped_provider(mut self, origin: &str) -> Result<Self> {
        let origin = scoped_origin(origin)?;
        let position = self.command.arguments.len() - 1;
        let mut overrides = Vec::new();
        let port = reqwest::Url::parse(&origin)?
            .port()
            .context("Codex scoped port")?;
        let endpoint = aq_codex_mapper::LoopbackResponsesEndpoint::new(port)?;
        for setting in aq_codex_mapper::proxy_provider_overrides(endpoint) {
            overrides.push("--config".into());
            overrides.push(setting);
        }
        self.command.arguments.splice(position..position, overrides);
        Ok(self)
    }
    pub(crate) fn with_git_author(
        mut self,
        author: Option<&aq_worker_protocol::git_author::GitAuthor>,
    ) -> Result<Self> {
        if let Some(author) = author {
            aq_codex_mapper::apply_tool_environment(
                &mut self.command.arguments,
                &mut self.command.environment,
                author.tool_environment(),
            )?;
        }
        Ok(self)
    }
    pub fn command(&self) -> &LocalCommand {
        &self.command
    }
    pub fn manifest_digest(&self) -> &Digest {
        &self.manifest_digest
    }
    /// Exactly one payload; no argv prompt and no constituent concatenation.
    /// Consuming the owner prevents an implicit retry after an uncertain write.
    pub fn write_startup(self, writer: &mut impl Write) -> Result<()> {
        writer.write_all(&self.startup)?;
        writer.flush()?;
        Ok(())
    }
}

/// The future private configuration owner writes these bytes to owner-only
/// auth.json. Never place this returned value in LocalCommand or public records.
pub fn api_key_auth(
    material: &MaterialHandoff<'_>,
    manifest: &aq_session_protocol::LaunchManifest,
) -> Result<Zeroizing<Vec<u8>>> {
    auth_bytes(&validated_api_key(material, manifest)?)
}
pub(crate) fn validated_api_key(
    material: &MaterialHandoff<'_>,
    manifest: &aq_session_protocol::LaunchManifest,
) -> Result<Zeroizing<Vec<u8>>> {
    manifest.validate()?;
    let lease = &material.installed.metadata().delivery.lease;
    ensure!(
        lease.binding.launch_manifest_digest == super::super::journal::canonical_digest(manifest)?
            && lease.provider.as_str() == "openai"
            && manifest
                .credentials
                .iter()
                .any(|r| r.credential_profile == lease.credential_profile
                    && r.purpose.as_str() == lease.purpose.as_str()),
        "Codex private material provider/profile binding"
    );
    let key = material.read_private()?;
    ensure!(
        !key.is_empty() && key.len() <= 4096 && key.iter().all(|b| b.is_ascii_graphic()),
        "Codex API key format"
    );
    Ok(key)
}
fn auth_bytes(key: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
    ensure!(
        !key.is_empty() && key.len() <= 4096 && key.iter().all(|b| b.is_ascii_graphic()),
        "Codex API key format"
    );
    let mut bytes = Zeroizing::new(Vec::with_capacity(key.len() + 32));
    bytes.extend_from_slice(b"{\"OPENAI_API_KEY\":");
    serde_json::to_writer(&mut *bytes, std::str::from_utf8(key)?)?;
    bytes.push(b'}');
    Ok(bytes)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CodexEvent {
    ThreadStarted(uuid::Uuid),
    TurnStarted(uuid::Uuid),
    TurnCompleted,
    Failed,
}
/// Bounded literal JSONL parser. Events are observations only; no start receipt,
/// model qualification, process identity or execution authority is produced.
#[derive(Default)]
pub struct CodexEvents {
    pending: Vec<u8>,
    thread: Option<uuid::Uuid>,
    turn: bool,
    failed: bool,
    finished: bool,
}
impl CodexEvents {
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<CodexEvent>> {
        if bytes.len() > MAX_EVENT {
            self.failed = true;
        }
        ensure!(
            !self.failed && bytes.len() <= MAX_EVENT,
            "Codex event reader refused"
        );
        let result = self.read(bytes);
        if result.is_err() {
            self.pending.clear();
            self.failed = true;
        }
        result
    }
    fn read(&mut self, bytes: &[u8]) -> Result<Vec<CodexEvent>> {
        let mut out = Vec::new();
        for byte in bytes {
            if *byte != b'\n' {
                ensure!(self.pending.len() < MAX_EVENT, "Codex event byte bound");
                self.pending.push(*byte);
                continue;
            }
            while self.pending.last() == Some(&b'\r') {
                self.pending.pop();
            }
            let value: serde_json::Value = serde_json::from_slice(&self.pending)?;
            self.pending.clear();
            match value.get("type").and_then(|v| v.as_str()) {
                Some("thread.started") => {
                    ensure!(
                        self.thread.is_none() && !self.turn,
                        "Codex duplicate thread event"
                    );
                    let id = uuid::Uuid::parse_str(
                        value
                            .get("thread_id")
                            .and_then(|v| v.as_str())
                            .context("Codex thread identity")?,
                    )?;
                    ensure!(!id.is_nil(), "Codex nil thread");
                    self.thread = Some(id);
                    out.push(CodexEvent::ThreadStarted(id));
                }
                Some("turn.started") => {
                    ensure!(!self.finished, "Codex extra turn after completion");
                    let id = self.thread.context("Codex turn before thread")?;
                    ensure!(!self.turn, "Codex duplicate turn event");
                    self.turn = true;
                    out.push(CodexEvent::TurnStarted(id));
                }
                Some("turn.completed") => {
                    ensure!(self.turn, "Codex completion before turn");
                    self.turn = false;
                    self.finished = true;
                    out.push(CodexEvent::TurnCompleted);
                }
                Some("turn.failed" | "error") => {
                    self.failed = true;
                    out.push(CodexEvent::Failed);
                }
                Some(kind) if kind.starts_with("item.") => {
                    ensure!(self.turn, "Codex item outside turn");
                }
                _ => anyhow::bail!("Codex unsupported event"),
            }
            if self.failed {
                break;
            }
        }
        Ok(out)
    }
    pub fn finish(&mut self) -> Result<()> {
        ensure!(
            self.pending.is_empty() && !self.turn && !self.failed && self.finished,
            "Codex incomplete/failed event stream"
        );
        Ok(())
    }
}

mod result_local;
pub(crate) use result_local::CodexResultInput;

#[cfg(test)]
mod tests;
