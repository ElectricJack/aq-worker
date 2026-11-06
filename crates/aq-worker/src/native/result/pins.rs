//! Immutable selected installation metadata. Data alone is not permission to launch.
use anyhow::{Result, ensure};
use aq_session_protocol::{ArtifactRef, Digest, Validate};
use serde::{Deserialize, Serialize};
/// Native scope construction must authenticate these against the actual retained
/// composite adapter/runtime qualification selected before the execution grant.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResultInstallationPins {
    pub adapter: ArtifactRef,
    pub contract: ArtifactRef,
    pub client_manifest: ArtifactRef,
    pub client_executable: ArtifactRef,
    pub environment: ArtifactRef,
    pub recipe: ArtifactRef,
    pub permission_digest: Digest,
    pub reserved_client_bytes: u64,
}
impl ResultInstallationPins {
    pub fn validate(&self) -> Result<()> {
        for value in [
            &self.adapter,
            &self.contract,
            &self.client_manifest,
            &self.client_executable,
            &self.environment,
            &self.recipe,
        ] {
            value.id.validate()?;
            value.version.validate()?;
        }
        ensure!(
            self.contract == aq_agent_result_local::artifact(),
            "result local contract differs from installed definition"
        );
        ensure!(
            self.reserved_client_bytes == 64 * 1024 * 1024,
            "result client reservation differs from the closed installation profile"
        );
        ensure!(
            self.recipe == aq_codex_mapper::result_local_recipe_artifact()
                || self.recipe == aq_codex_mapper::interactive_result_local_recipe_artifact()
                || self.recipe == crate::native::adapter::claude::docker_local_recipe_artifact(),
            "result recipe identity"
        );
        Ok(())
    }
}
