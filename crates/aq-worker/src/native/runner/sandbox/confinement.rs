//! Harness confinement modes (C6a; architecture 0005 §8.3, SBX7, SBX13, SBX19).
//!
//! The mode is read from the frozen launch manifest, never chosen here:
//! `aq.harness.confined-worktree` declares `native` (the harness's own sandbox is
//! active inside our sandbox) and `aq.harness.container-confinement` declares
//! `container` (the harness sandbox is off and the backend sandbox is the boundary).
//! [`authorize`] admits a declared mode against the execution profile's permission,
//! the template × backend qualification record and the attempt's runner/agent
//! separation. A refused `native` is never downgraded to `container` (SBX13).
use anyhow::{Result, bail, ensure};
use aq_session_host::users::AgentIdentity;
use aq_session_protocol::{
    CONTAINER_CONFINEMENT_CAPABILITY, LaunchManifest, NATIVE_CONFINEMENT_CAPABILITY, Validate,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfinementMode {
    Native,
    Container,
}
impl ConfinementMode {
    /// The mode the frozen manifest declares. Validation requires exactly one.
    pub fn declared(manifest: &LaunchManifest) -> Result<Self> {
        manifest.validate()?;
        let has = |id: &str| {
            manifest
                .harness
                .required_capabilities
                .iter()
                .any(|c| c.id.as_str() == id && c.version == 1)
        };
        match (
            has(NATIVE_CONFINEMENT_CAPABILITY),
            has(CONTAINER_CONFINEMENT_CAPABILITY),
        ) {
            (true, false) => Ok(Self::Native),
            (false, true) => Ok(Self::Container),
            _ => bail!("manifest must declare exactly one confinement mode"),
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Container => "container",
        }
    }
}

/// Worker-side mirror of the execution profile's confinement permission. The
/// authoritative field belongs to aq-server's `aq-profiles` `Execution`
/// (CROSS-REPO FOLLOW-UP); until the server sends it the worker defaults to deny.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ExecutionConfinement {
    /// `container` is allowed only when the profile says so explicitly.
    pub permit_container: bool,
    /// The profile refuses any backend where `native` is not qualified.
    pub require_native: bool,
}

/// Whether the backend itself provides the sandbox boundary (SBX10/SBX11).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendBoundary {
    None,
    Container,
}

/// Outcome of the native-mode probe: a write outside the workspace under the
/// harness's own sandbox must be refused for `native` to qualify.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeProbe {
    pub outside_write_refused: bool,
}

/// One qualification record per template × backend (0005 §11 "Harness confinement").
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QualificationRecord {
    pub template: String,
    pub backend: String,
    pub boundary: BackendBoundary,
    /// `native` works: writes outside the workspace were refused.
    pub native_supported: bool,
    /// Preferred mode: `native` when qualified, otherwise `container`.
    pub default_mode: ConfinementMode,
    pub evidence: String,
}
impl QualificationRecord {
    pub fn from_native_probe(
        template: &str,
        backend: &str,
        boundary: BackendBoundary,
        probe: NativeProbe,
        evidence: &str,
    ) -> Self {
        let native_supported = probe.outside_write_refused;
        Self {
            template: template.into(),
            backend: backend.into(),
            boundary,
            native_supported,
            default_mode: if native_supported {
                ConfinementMode::Native
            } else {
                ConfinementMode::Container
            },
            evidence: evidence.into(),
        }
    }
    /// docker-local (rootless Podman on WSL2) is container-only: C2 recorded
    /// `unshare -Ur id` failing with EPERM under the fixed launch policy, so the
    /// harness's bubblewrap sandbox cannot run (0005 §8.3, decision 14).
    pub fn docker_local(template: &str) -> Self {
        Self::from_native_probe(
            template,
            "docker-local",
            BackendBoundary::Container,
            NativeProbe {
                outside_write_refused: false,
            },
            "C2 docker-local probe: unshare -Ur id EPERM (docs/implementation/C2.md)",
        )
    }
}

/// SBX7 inputs. `credentials_attempt_scoped` is the credential installer's
/// statement that the attempt holds only its own leased credentials.
#[derive(Clone, Copy, Debug)]
pub struct AttemptSeparation {
    pub runner_uid: u32,
    pub agent: AgentIdentity,
    pub credentials_attempt_scoped: bool,
}

/// Proof that a declared mode passed [`authorize`]. Fields are private, so a
/// harness configuration cannot be built for an unauthorized mode.
#[derive(Clone, Debug)]
pub struct AuthorizedConfinement {
    mode: ConfinementMode,
    template: String,
    backend: String,
}
impl AuthorizedConfinement {
    pub fn mode(&self) -> ConfinementMode {
        self.mode
    }
    pub fn template(&self) -> &str {
        &self.template
    }
    pub fn backend(&self) -> &str {
        &self.backend
    }
}

/// Profile × backend admission, before any attempt is placed.
pub fn admit_profile(profile: &ExecutionConfinement, record: &QualificationRecord) -> Result<()> {
    ensure!(
        record.native_supported || !profile.require_native,
        "execution profile requires native confinement; {} on {} is container-only",
        record.template,
        record.backend
    );
    ensure!(
        record.native_supported || profile.permit_container,
        "execution profile does not permit container confinement and native is not qualified on {}",
        record.backend
    );
    Ok(())
}

pub fn authorize(
    declared: ConfinementMode,
    profile: &ExecutionConfinement,
    record: &QualificationRecord,
    separation: &AttemptSeparation,
) -> Result<AuthorizedConfinement> {
    admit_profile(profile, record)?;
    match declared {
        ConfinementMode::Native => ensure!(
            record.native_supported,
            "native confinement is not qualified for {} on {}; no fallback to container (SBX13)",
            record.template,
            record.backend
        ),
        ConfinementMode::Container => {
            ensure!(
                profile.permit_container && !profile.require_native,
                "container confinement requires explicit execution profile permission"
            );
            ensure!(
                record.boundary == BackendBoundary::Container,
                "container confinement requires a backend-provided sandbox boundary"
            );
            ensure!(
                separation.agent.uid != 0
                    && separation.agent.gid != 0
                    && separation.agent.uid != separation.runner_uid,
                "container confinement requires a distinct unprivileged agent user (SBX7)"
            );
            ensure!(
                separation.credentials_attempt_scoped,
                "container confinement requires attempt-scoped credentials only"
            );
        }
    }
    Ok(AuthorizedConfinement {
        mode: declared,
        template: record.template.clone(),
        backend: record.backend.clone(),
    })
}
