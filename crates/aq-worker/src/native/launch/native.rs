//! Production native profile with an exact installed image and retained inputs.
use super::*;
use crate::native::adapter::{
    codex::{CodexExecInput, CodexExecProfile, CodexResultInput, PreparedCodexExec},
    named_exec::NamedExecInstallation,
    native_exec::NativeExecInstallation,
    responses_transport::{ResponsesScope, ScopedResponsesTransport, UpstreamEndpoint},
    result_client::InstalledResultClient,
    result_profile::{ExpectedResultAdapter, ResultAdapterDefinition},
};
use crate::native::{journal::RuntimeIdentity, result::ResultInstallationPins};
use aq_session_host::{
    AdapterLaunch, HarnessAdapter, HarnessReadiness, LocalEnvironment, OriginalPrivateMaterial,
    PrivateLaunchInput, ResourcePolicy, ScopedCodexAuthBinding,
    observed::{
        NamedReadonlyExecImage, NamedReadonlyObservationProfile, NativeExecImage,
        NativeExecObservationProfile, ObservedAdapterQualification, ObservedHarnessAdapter,
        ObservedStartup,
    },
};
use std::path::PathBuf;

pub struct NativeCodexHostProfile {
    pub(super) root: RootGuard,
    root_path: PathBuf,
    installation: Mutex<Installation>,
    mapper: CodexExecProfile,
    digest: Digest,
    columns: u16,
    rows: u16,
    result: Option<ResultInstallation>,
    terminal_clock: Option<aq_session_host::terminal_io::QualifiedClock>,
    #[cfg(any(test, feature = "test-support"))]
    development_provider: Option<String>,
}
struct ResultInstallation {
    client: InstalledResultClient,
    definition: ResultAdapterDefinition,
    pins: ResultInstallationPins,
}
impl NativeCodexHostProfile {
    pub fn new(
        root: PathBuf,
        installation: NativeExecInstallation,
        mapper: CodexExecProfile,
        columns: u16,
        rows: u16,
    ) -> Result<Self> {
        Self::build(
            root,
            Installation::Sealed(installation),
            mapper,
            (columns, rows),
        )
    }
    pub fn new_named(
        root: PathBuf,
        installation: NamedExecInstallation,
        mapper: CodexExecProfile,
        columns: u16,
        rows: u16,
    ) -> Result<Self> {
        // The final CLI environment contains no TMPDIR override, so its release
        // helper-alias policy uses /tmp, not the worker's test TMPDIR.
        ensure!(
            !root.starts_with("/tmp"),
            "named native state root must be outside CLI temporary root"
        );
        ensure!(
            installation.environment() == &mapper.harness.environment,
            "named image environment differs from selected harness"
        );
        NamedReadonlyObservationProfile::new(mapper.renderer.clone(), columns, rows)?;
        Self::build(
            root,
            Installation::Named(installation),
            mapper,
            (columns, rows),
        )
    }
    /// Explicit result-enabled profile. The client and composite definition are
    /// independently selected and retained; no ordinary profile gains a channel.
    pub fn new_named_result(
        root: PathBuf,
        installation: NamedExecInstallation,
        mapper: CodexExecProfile,
        columns: u16,
        rows: u16,
        client: InstalledResultClient,
        definition: ResultAdapterDefinition,
    ) -> Result<Self> {
        let installed = installation.installation();
        let runtime = RuntimeIdentity {
            runtime: installed.runtime.clone(),
            qualification: installed.qualification.clone(),
        };
        definition.validate_selected(ExpectedResultAdapter {
            adapter: &mapper.harness.adapter,
            runtime: &runtime,
            environment: &mapper.harness.environment,
        })?;
        definition.validate_installed_client(&client)?;
        let pins = ResultInstallationPins {
            adapter: mapper.harness.adapter.clone(),
            contract: client.contract().clone(),
            client_manifest: client.manifest_artifact(),
            client_executable: client.artifact().clone(),
            environment: client.environment().clone(),
            recipe: definition.recipe.clone(),
            // Replaced by the offered plan's digest before a result channel is created.
            permission_digest: definition.permission_digest.clone(),
            reserved_client_bytes: client.reserved_bytes(),
        };
        pins.validate()?;
        let mut profile = Self::new_named(root, installation, mapper, columns, rows)?;
        profile.digest = crate::native::journal::canonical_digest(
            &serde_json::json!({"base_profile":profile.digest,"result_installation":pins}),
        )?;
        profile.result = Some(ResultInstallation {
            client,
            definition,
            pins,
        });
        Ok(profile)
    }
    /// Installation metadata only, never an execution/channel constructor.
    pub fn with_interactive_clock(mut self, maximum_skew_ms: u64) -> Result<Self> {
        ensure!(
            lock(&self.installation)?.interactive()
                && self
                    .result
                    .as_ref()
                    .is_some_and(|r| r.definition.interactive()),
            "interactive clock requires explicitly selected interactive result profile"
        );
        self.terminal_clock = Some(
            aq_session_host::terminal_io::QualifiedClock::assume_bounded_skew(maximum_skew_ms)?,
        );
        self.digest = crate::native::journal::canonical_digest(
            &serde_json::json!({"profile":self.digest,"interactive_clock_skew_ms":maximum_skew_ms}),
        )?;
        Ok(self)
    }
    pub fn terminal_clock(&self) -> Option<aq_session_host::terminal_io::QualifiedClock> {
        self.terminal_clock
    }
    pub fn result_installation_pins(&self) -> Option<ResultInstallationPins> {
        self.result.as_ref().map(|result| result.pins.clone())
    }
    fn build(
        root: PathBuf,
        mut installation: Installation,
        mapper: CodexExecProfile,
        size: (u16, u16),
    ) -> Result<Self> {
        let (columns, rows) = size;
        let guard = RootGuard::open(&root, None)?;
        NativeExecObservationProfile::new(mapper.renderer.clone(), columns, rows)?;
        ensure!(
            mapper.harness.harness.as_str() == "codex"
                && mapper.harness.cli_version.as_str()
                    == crate::native::adapter::codex::CODEX_CLI_VERSION,
            "unsupported native Codex settings"
        );
        installation.revalidate()?;
        let m = installation.installation();
        ensure!(
            m.adapter == mapper.harness.adapter && m.renderer == mapper.renderer,
            "native installed mapper references differ"
        );
        let upstream_policy = if installation.interactive() {
            "native-codex-chatgpt-login-v1"
        } else {
            "fixed-openai-responses-https-v1"
        };
        let digest = crate::native::journal::canonical_digest(
            &serde_json::json!({"schema_version":1,"profile":"codex-scoped-responses-native-host-v1","image_profile":installation.kind(),"upstream_policy":upstream_policy,"root":root,"installation":installation.digest(),"harness":mapper.harness,"permissions":mapper.permissions_digest,"renderer":mapper.renderer,"columns":columns,"rows":rows}),
        )?;
        Ok(Self {
            root: guard,
            root_path: root,
            installation: Mutex::new(installation),
            mapper,
            digest,
            columns,
            rows,
            result: None,
            terminal_clock: None,
            #[cfg(any(test, feature = "test-support"))]
            development_provider: None,
        })
    }
    /// Isolated fixture upstream for the owned transport. This cannot enable
    /// the production executor or supply a direct provider route to the child.
    #[cfg(any(test, feature = "test-support"))]
    pub fn with_development_provider(mut self, origin: &str) -> Result<Self> {
        ensure!(
            !lock(&self.installation)?.interactive(),
            "native ChatGPT interactive profile does not use a finite provider override"
        );
        let origin = crate::native::adapter::codex::development_origin(origin)?;
        self.digest = crate::native::journal::canonical_digest(
            &serde_json::json!({"profile":self.digest,"development_provider":origin}),
        )?;
        self.development_provider = Some(origin);
        Ok(self)
    }
    pub fn digest(&self) -> &Digest {
        &self.digest
    }
}
enum Installation {
    Sealed(NativeExecInstallation),
    Named(NamedExecInstallation),
}
impl Installation {
    fn interactive(&self) -> bool {
        matches!(self, Self::Named(i) if i.interactive())
    }
    fn kind(&self) -> &'static str {
        match self {
            Self::Sealed(_) => "sealed-native-v1",
            Self::Named(i) if i.interactive() => "named-readonly-interactive-pty-v1",
            Self::Named(_) => "named-readonly-native-v1",
        }
    }
    fn digest(&self) -> &Digest {
        match self {
            Self::Sealed(i) => i.digest(),
            Self::Named(i) => i.digest(),
        }
    }
    fn installation(&self) -> &crate::native::adapter::AdapterManifest {
        match self {
            Self::Sealed(i) => i.installation(),
            Self::Named(i) => i.installation(),
        }
    }
    fn revalidate(&mut self) -> Result<()> {
        match self {
            Self::Sealed(i) => i.revalidate(),
            Self::Named(i) => i.revalidate(),
        }
    }
    fn material<'a>(
        &self,
        input: &'a crate::native::materials::InstalledMaterial,
        receipt: &aq_admission::MaterialReceipt,
        binding: &aq_session_protocol::LaunchBinding,
    ) -> Result<crate::native::adapter::MaterialHandoff<'a>> {
        match self {
            Self::Sealed(i) => i.material(input, receipt, binding),
            Self::Named(i) => i.material(input, receipt, binding),
        }
    }
    fn prepare(
        &mut self,
        mapper: &CodexExecProfile,
        input: CodexExecInput<'_>,
    ) -> Result<PreparedCodexExec> {
        match self {
            Self::Sealed(i) => mapper.prepare_native_planned(i, input),
            Self::Named(i) => mapper.prepare_named_planned(i, input),
        }
    }
    fn image(&self) -> Result<Image> {
        match self {
            Self::Sealed(i) => Ok(Image::Sealed(i.image()?)),
            Self::Named(i) if i.interactive() => Ok(Image::NamedInteractive(i.image()?)),
            Self::Named(i) => Ok(Image::Named(i.image()?)),
        }
    }
}
enum Image {
    Sealed(NativeExecImage),
    Named(NamedReadonlyExecImage),
    NamedInteractive(NamedReadonlyExecImage),
}
impl Image {
    fn startup(
        self,
        scope: &LaunchExecutionScope,
        intent: &NativeLaunchIntent,
    ) -> Result<ObservedStartup> {
        let rendered = scope.inputs.startup().clone();
        let terminal = intent.terminal.clone();
        let renderer = intent.renderer.clone();
        let (columns, rows) = (intent.columns, intent.rows);
        let hard_ms = scope.hard / 1_000_000;
        match self {
            Self::Sealed(image) => ObservedStartup::native_exec(
                rendered,
                terminal,
                image,
                NativeExecObservationProfile::new(renderer, columns, rows)?,
                hard_ms,
            ),
            Self::NamedInteractive(image) => ObservedStartup::named_readonly_interactive(
                rendered,
                terminal,
                image,
                NamedReadonlyObservationProfile::new(renderer, columns, rows)?,
                hard_ms,
            ),
            Self::Named(image) => ObservedStartup::named_readonly_exec(
                rendered,
                terminal,
                image,
                NamedReadonlyObservationProfile::new(renderer, columns, rows)?,
                hard_ms,
            ),
        }
    }
}
struct Adapter {
    recipe: PreparedCodexExec,
    executable: ArtifactRef,
    renderer: ArtifactRef,
    private: Mutex<Option<PrivateLaunchInput>>,
}
impl HarnessAdapter for Adapter {
    fn prepare(
        &self,
        m: &aq_session_protocol::LaunchManifest,
        _: &std::path::Path,
    ) -> Result<AdapterLaunch> {
        ensure!(
            crate::native::journal::canonical_digest(m)? == *self.recipe.manifest_digest(),
            "native command manifest changed"
        );
        Ok(AdapterLaunch {
            resource_qualification: None,
            manifest_digest: self.recipe.manifest_digest().clone(),
            adapter: m.harness.adapter.clone(),
            cli_version: m.harness.cli_version.as_str().into(),
            environment: m.harness.environment.clone(),
            capabilities: m.harness.required_capabilities.clone(),
            command: self.recipe.command().clone(),
        })
    }
    fn readiness(&self, _: &HostObservation) -> Result<HarnessReadiness> {
        Ok(HarnessReadiness::Unknown)
    }
    fn private_input(
        &self,
        _: &aq_session_protocol::LaunchManifest,
        _: &std::path::Path,
    ) -> Result<Option<PrivateLaunchInput>> {
        Ok(Some(
            lock(&self.private)?
                .take()
                .context("native private input already consumed")?,
        ))
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
mod prepare;
pub(super) use prepare::bootstrap;
