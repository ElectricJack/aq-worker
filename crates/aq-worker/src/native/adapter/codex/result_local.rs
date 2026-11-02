//! Named-result mapping through the actual execution-owned local channel.
//! No disk/history constructor and no startup payload modification.
use super::*;
use crate::native::{
    adapter::{
        named_exec::NamedExecInstallation,
        result_client::InstalledResultClient,
        result_profile::{ExpectedResultAdapter, ResultAdapterDefinition},
    },
    journal::RuntimeIdentity,
    result::{ResultInstallationPins, VerifiedResultChannel},
};
use aq_session_protocol::LaunchBinding;

pub(crate) struct CodexResultInput<'a> {
    pub binding: &'a LaunchBinding,
    pub execution_digest: &'a Digest,
    pub definition: &'a ResultAdapterDefinition,
    pub client: &'a InstalledResultClient,
    pub channel: &'a VerifiedResultChannel,
}
impl CodexExecProfile {
    pub(crate) fn prepare_named_result_planned(
        &self,
        installation: &mut NamedExecInstallation,
        input: CodexExecInput<'_>,
        result: CodexResultInput<'_>,
    ) -> Result<PreparedCodexExec> {
        let manifest = &input.offer.offered.offer.manifest;
        ensure!(
            result.binding.launch_manifest_digest == *input.expected_manifest,
            "result mapping binding differs from exact manifest"
        );
        let installed = installation.installation();
        let runtime = RuntimeIdentity {
            runtime: installed.runtime.clone(),
            qualification: installed.qualification.clone(),
        };
        result.definition.validate_selected(ExpectedResultAdapter {
            adapter: &self.harness.adapter,
            runtime: &runtime,
            renderer: &self.renderer,
            environment: &self.harness.environment,
            permission_digest: &self.permissions_digest,
        })?;
        result.definition.validate_manifest(manifest)?;
        result.definition.validate_installed_client(result.client)?;
        let pins = ResultInstallationPins {
            adapter: result.definition.artifact()?,
            contract: result.client.contract().clone(),
            client_manifest: result.client.manifest_artifact(),
            client_executable: result.client.artifact().clone(),
            environment: result.client.environment().clone(),
            recipe: aq_codex_mapper::result_local_recipe_artifact(),
            permission_digest: self.permissions_digest.clone(),
            reserved_client_bytes: result.client.reserved_bytes(),
        };
        let workspace = input.workspace;
        let outcome = manifest.bindings.outcome.clone();
        result.channel.verify_for(
            result.binding,
            result.execution_digest,
            &outcome,
            workspace,
            &pins,
        )?;
        let recipe = aq_codex_mapper::map_with_result_channel(
            aq_codex_mapper::Settings {
                harness: &self.harness,
                model: &self.model,
                terminal_input_enabled: manifest.terminal.input_enabled,
            },
            aq_codex_mapper::Paths {
                executable: &installed.executable.path,
                workspace,
                private_home: input.private_home,
            },
            aq_codex_mapper::ResultChannel {
                root: result.channel.root(),
                client: result.client.path(),
            },
        )?;
        // Keep all old remote/manifest/settings/startup/private-home and
        // named-image checks. Only the explicitly opted-in command is replaced.
        let mut prepared = self.prepare_named_planned(installation, input)?;
        result.client.revalidate()?;
        result.channel.verify_for(
            result.binding,
            result.execution_digest,
            &outcome,
            workspace,
            &pins,
        )?;
        prepared.command = LocalCommand {
            executable: recipe.executable,
            arguments: recipe.arguments,
            environment: recipe.environment,
        };
        Ok(prepared)
    }
}
