//! Content-addressed result adapter definition selected by an admitted manifest.
//! All methods validate data/retained bytes only. The actual execution owner must
//! separately verify authenticated scope, grant and installed runtime qualification.
use super::{result_client::*, *};
use crate::native::{VerifiedArtifact, journal::RuntimeIdentity};
use aq_admission::{ArtifactKind, RetainedArtifact};
use aq_session_protocol::LaunchManifest;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ResultAdapterKind {
    #[serde(rename = "codex-0.166-trusted-node-named-readonly-result-local-v1")]
    Codex0166TrustedNodeNamedReadonlyLocalV1,
    #[serde(rename = "codex-0.166-chatgpt-interactive-result-local-v1")]
    Codex0166ChatgptInteractiveLocalV1,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResultAdapterDefinition {
    pub schema_version: u32,
    pub profile: ResultAdapterKind,
    pub runtime: ArtifactRef,
    pub environment: ArtifactRef,
    pub client_manifest: ArtifactRef,
    pub client_executable: ArtifactRef,
    pub contract: ArtifactRef,
    pub recipe: ArtifactRef,
    pub permission_digest: Digest,
    pub reserved_client_bytes: u64,
}
/// Independently configured early selection. This does not supply a grant.
pub struct ExpectedResultAdapter<'a> {
    pub adapter: &'a ArtifactRef,
    pub runtime: &'a RuntimeIdentity,
    pub environment: &'a ArtifactRef,
    pub permission_digest: &'a Digest,
}
impl ResultAdapterDefinition {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut value = serde_json::to_value(self)?;
        value.sort_all_objects();
        Ok(serde_json::to_vec(&value)?)
    }
    pub fn artifact(&self) -> Result<ArtifactRef> {
        self.artifact_at_revision(&aq_session_protocol::Text::new("1")?)
    }
    /// Definition instance revision comes from the independently selected ref;
    /// it does not change this body's schema or its public recipe/contract.
    pub fn artifact_at_revision(
        &self,
        revision: &aq_session_protocol::Text,
    ) -> Result<ArtifactRef> {
        revision.validate()?;
        Ok(ArtifactRef {
            id: aq_session_protocol::Text::new("aq.codex-result-local-adapter")?,
            version: revision.clone(),
            digest: Digest::of(&self.encode()?),
        })
    }
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        ensure!(
            !bytes.is_empty() && bytes.len() <= MAX_MANIFEST,
            "result adapter definition bound"
        );
        let value: Self = serde_json::from_slice(bytes)?;
        ensure!(
            value.encode()? == bytes,
            "result adapter definition must be exact canonical bytes"
        );
        value.validate()?;
        Ok(value)
    }
    pub fn interactive(&self) -> bool {
        matches!(
            self.profile,
            ResultAdapterKind::Codex0166ChatgptInteractiveLocalV1
        )
    }
    pub fn recipe_artifact(&self) -> ArtifactRef {
        if self.interactive() {
            aq_codex_mapper::interactive_result_local_recipe_artifact()
        } else {
            aq_codex_mapper::result_local_recipe_artifact()
        }
    }
    fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == 1
                && self.encode()?.len() <= MAX_MANIFEST
                && self.contract == aq_agent_result_local::artifact()
                && self.recipe == self.recipe_artifact()
                && self.reserved_client_bytes == MAX_RESULT_CLIENT_INSTALLATION_BYTES
                && self.client_manifest.id.as_str() == "aq.agent-result-client-installation",
            "unsupported result adapter contract/recipe/install reservation"
        );
        for r in [
            &self.runtime,
            &self.environment,
            &self.client_manifest,
            &self.client_executable,
            &self.contract,
            &self.recipe,
        ] {
            r.id.validate()?;
            r.version.validate()?;
        }
        Ok(())
    }
    pub fn validate_selected(&self, expected: ExpectedResultAdapter<'_>) -> Result<()> {
        self.validate()?;
        ensure!(
            self.artifact_at_revision(&expected.adapter.version)? == *expected.adapter
                && self.runtime == expected.runtime.runtime
                && self.environment == *expected.environment
                && self.permission_digest == *expected.permission_digest,
            "result adapter differs from independently selected runtime/settings"
        );
        // The qualifier is checked against the actual installed native runtime
        // and actual grant by its existing owner, not inferred from this body.
        expected.runtime.qualification.id.validate()?;
        expected.runtime.qualification.version.validate()?;
        Ok(())
    }
    pub fn expected_client(&self) -> Result<ExpectedResultClient<'_>> {
        self.validate()?;
        Ok(ExpectedResultClient {
            manifest_digest: &self.client_manifest.digest,
            contract: &self.contract,
            environment: &self.environment,
            executable: &self.client_executable,
        })
    }
    pub fn validate_installed_client(&self, client: &InstalledResultClient) -> Result<()> {
        self.validate()?;
        ensure!(
            client.manifest_artifact() == self.client_manifest
                && client.artifact() == &self.client_executable
                && client.environment() == &self.environment
                && client.contract() == &self.contract
                && client.reserved_bytes() == self.reserved_client_bytes,
            "result adapter installed client differs"
        );
        client.revalidate()
    }
    pub fn validate_manifest(&self, manifest: &LaunchManifest) -> Result<()> {
        self.validate()?;
        manifest.validate()?;
        ensure!(
            manifest.harness.adapter
                == self.artifact_at_revision(&manifest.harness.adapter.version)?
                && manifest.harness.environment == self.environment
                && super::super::journal::canonical_digest(&manifest.permissions)?
                    == self.permission_digest,
            "result adapter manifest opt-in/settings/permissions differ"
        );
        let settings = aq_codex_mapper::Settings {
            harness: &manifest.harness,
            model: &manifest.model,
            terminal_input_enabled: manifest.terminal.input_enabled,
        };
        if self.interactive() {
            aq_codex_mapper::validate_interactive_settings(settings)
        } else {
            aq_codex_mapper::validate_settings(settings)
        }
    }
    /// The caller must obtain this body and descriptor list from its actual
    /// retained execution inputs. Assembly/JSON equality alone grants no scope.
    pub fn validate_retained(
        &self,
        selected_adapter: &ArtifactRef,
        descriptors: &[RetainedArtifact],
        retained: &VerifiedArtifact,
    ) -> Result<()> {
        self.validate()?;
        let reference = self.artifact_at_revision(&selected_adapter.version)?;
        ensure!(
            descriptors.len() <= 128,
            "result adapter descriptor count bound"
        );
        let matching: Vec<_> = descriptors
            .iter()
            .filter(|d| d.artifact == reference)
            .collect();
        ensure!(
            matching.len() == 1,
            "result adapter retained descriptor absent or duplicated"
        );
        let descriptor = matching[0];
        let receipt = retained.receipt();
        ensure!(
            reference == *selected_adapter
                && descriptor.kind == ArtifactKind::Definition
                && serde_json::to_value(descriptor)? == serde_json::to_value(&retained.descriptor)?
                && descriptor.bytes == retained.bytes().len() as u64
                && receipt.artifact == reference
                && receipt.digest == reference.digest
                && receipt.bytes == descriptor.bytes
                && retained.bytes() == self.encode()?,
            "result adapter requires exact retained Definition-role bytes"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests;
