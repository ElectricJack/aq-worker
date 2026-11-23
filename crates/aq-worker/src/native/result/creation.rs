//! Join the retained execution, selected result client, and durable channel intent.
use super::*;
use crate::native::{
    adapter::result_profile::ResultAdapterDefinition,
    journal::{self, Attempt},
};
use execution::RetainedPhaseInputs;

pub(super) struct ChannelIntent {
    pub(super) inputs: RetainedPhaseInputs,
    pub(super) scope: LaunchExecutionScope,
    pub(super) pins: ResultInstallationPins,
    pub(super) root: PathBuf,
}
pub(super) struct RecordedChannel {
    pub(super) binding: wire::Binding,
    pub(super) presence: wire::Presence,
    pub(super) definition: ResultAdapterDefinition,
    pub(super) workspace: PathBuf,
    pub(super) roots: roots::SourceRoots,
}
impl ChannelIntent {
    fn selected_definition(&self, a: &Attempt) -> Result<ResultAdapterDefinition> {
        let m = &a.reservation.offer.offered.offer.manifest;
        let retained = self
            .inputs
            .artifacts()
            .iter()
            .find(|v| v.receipt().artifact == m.harness.adapter)
            .context("selected result Definition bytes absent")?;
        let definition = ResultAdapterDefinition::decode(retained.bytes())?;
        definition.validate_retained(
            &m.harness.adapter,
            &a.reservation.offer.offered.artifacts,
            retained,
        )?;
        definition.validate_manifest(m)?;
        let expected = ResultInstallationPins {
            adapter: m.harness.adapter.clone(),
            contract: definition.contract.clone(),
            client_manifest: definition.client_manifest.clone(),
            client_executable: definition.client_executable.clone(),
            environment: definition.environment.clone(),
            recipe: definition.recipe.clone(),
            permission_digest: journal::canonical_digest(&m.permissions)?,
            reserved_client_bytes: definition.reserved_client_bytes,
        };
        ensure!(
            expected == self.pins && definition.runtime == a.reservation.runtime.runtime,
            "result client pins not selected by original grant"
        );
        Ok(definition)
    }
    fn verify_startup(&self, a: &Attempt) -> Result<()> {
        // Instructions must already have been counted and rendered centrally.
        let startup = wire::startup_artifact();
        ensure!(
            counted_startup(&a.reservation.offer)
                && self
                    .inputs
                    .artifacts()
                    .iter()
                    .any(|v| v.receipt().artifact == startup
                        && v.bytes() == wire::STARTUP_TEXT.as_bytes()),
            "result reporting instructions were not centrally counted"
        );
        Ok(())
    }
    pub(super) fn record(self, owner: &mut Owned) -> Result<RecordedChannel> {
        let id = self.scope.id;
        self.scope.check(owner)?;
        let a = owner.journal.state().result_attempt(id)?;
        let definition = self.selected_definition(a)?;
        let source = a.source.as_ref().context("result source owner absent")?;
        self.verify_startup(a)?;
        let binding = wire::Binding {
            schema_version: 1,
            launch: a.reservation.offer.offered.offer.binding.clone(),
            execution_record_digest: self.scope.digest.clone(),
            expected_outcome: source.spec.outcome_artifact.clone(),
            operation_id: Uuid::new_v4(),
            report_id: Uuid::new_v4(),
            nonce: Uuid::new_v4(),
        };
        let presence = wire::Presence {
            schema_version: 1,
            binding_digest: binding.digest()?,
            kernel_boot_id: owner.journal.state().kernel_boot_id,
            original_execution_deadline_boot_ns: self.scope.deadline,
            original_hard_deadline_boot_ns: self.scope.hard,
        };
        let workspace = self.inputs.workspace().workspace().clone();
        let git = self.inputs.workspace().git();
        let roots = roots::SourceRoots::new(git.root(), &workspace.path)?;
        let (git_pin, workspace_pin) = roots.pins()?;
        let scope = ScopeRecord {
            binding: binding.clone(),
            contract: wire::artifact(),
            installation: self.pins,
            root: self.root,
            storage: StorageStage::Creating,
            git_pin,
            workspace_pin,
            runtime: a.reservation.runtime.clone(),
            kernel_boot_id: presence.kernel_boot_id,
            original_deadline_ns: self.scope.deadline,
            original_hard_deadline_ns: self.scope.hard,
            created_ns: boot_ns()?,
            recorded_revision: 1,
            source: source.binding()?.clone(),
            git_author: a.reservation.git_author.clone(),
            workspace: workspace.clone(),
            managed_root: git.root().into(),
            reserved_disk_bytes: a.reservation.disk_bytes,
            workspace_limit_bytes: git.workspace_byte_limit(),
            artifact_limit_bytes: git.transfer_byte_limit(),
            operations: SourceOperations {
                checkpoint: Uuid::new_v4(),
                export: Uuid::new_v4(),
                transfer: Uuid::new_v4(),
                upload_begin: Uuid::new_v4(),
                upload_finalize: Uuid::new_v4(),
            },
        };
        owner.journal.begin_result(id, pending_history(scope))?;
        Ok(RecordedChannel {
            binding,
            presence,
            definition,
            workspace: workspace.path,
            roots,
        })
    }
}
fn pending_history(scope: ScopeRecord) -> ResultHistory {
    ResultHistory {
        schema_version: 1,
        scope,
        intake_closed: false,
        claim: None,
        stop: None,
        source: SourceHistory::default(),
        report: None,
        receipt: None,
        failure: None,
        salvaged: None,
    }
}
