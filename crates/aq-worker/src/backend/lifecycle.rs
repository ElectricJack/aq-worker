use super::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LifecyclePhase {
    Intent,
    Created { sandbox: SandboxIdentity },
    Stopped { proof: StoppedWriterProof },
    Archived { proof: StoppedWriterProof },
    Preserved { proof: StoppedWriterProof },
    Destroyed { proof: StoppedWriterProof },
}
impl LifecyclePhase {
    fn sandbox(&self) -> Option<&SandboxIdentity> {
        match self {
            Self::Intent => None,
            Self::Created { sandbox } => Some(sandbox),
            Self::Stopped { proof }
            | Self::Archived { proof }
            | Self::Preserved { proof }
            | Self::Destroyed { proof } => Some(proof.sandbox()),
        }
    }
}

/// Standalone journal facts. The controller must durably record intent before
/// create and fsync each successful transition; C1 does not write its journal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LifecycleRecord {
    pub kind: BackendKind,
    pub request: CreateRequest,
    pub phase: LifecyclePhase,
}
impl LifecycleRecord {
    pub fn intent(kind: BackendKind, request: CreateRequest) -> Self {
        Self {
            kind,
            request,
            phase: LifecyclePhase::Intent,
        }
    }
    fn matches(&self, sandbox: &SandboxIdentity) -> bool {
        self.kind == sandbox.kind
            && self.request.owner == sandbox.owner
            && !sandbox.backend_id.is_empty()
            && self.phase.sandbox().is_none_or(|known| known == sandbox)
    }
    fn confirmed<'a>(
        &self,
        result: &'a Confirmation<StoppedWriterProof>,
        kind: WriterProofKind,
    ) -> Result<&'a StoppedWriterProof, BackendError> {
        match result {
            Confirmation::Confirmed(proof)
                if proof.kind() == kind && self.matches(proof.sandbox()) =>
            {
                Ok(proof)
            }
            _ => Err(BackendError::InvalidState),
        }
    }
    fn inventory_conflict(&self, entry: &InventoryEntry) -> Option<ReconcileAction> {
        if self.request.owner.node_id != entry.sandbox.owner.node_id {
            return Some(ReconcileAction::IgnoreForeign);
        }
        let destroyed = matches!(self.phase, LifecyclePhase::Destroyed { .. });
        if destroyed && entry.state != SandboxState::Destroyed {
            return Some(ReconcileAction::Hold);
        }
        let attributes = (
            &self.request.name,
            &self.request.template_digest,
            self.request.hard_deadline_ms,
        );
        let observed = (&entry.name, &entry.template_digest, entry.hard_deadline_ms);
        let valid = self.request.validate().is_ok() && self.matches(&entry.sandbox);
        if !valid || attributes != observed {
            return Some(ReconcileAction::Hold);
        }
        None
    }
    pub fn record_created(&mut self, sandbox: SandboxIdentity) -> Result<(), BackendError> {
        self.request.validate()?;
        if !self.matches(&sandbox)
            || !matches!(
                self.phase,
                LifecyclePhase::Intent | LifecyclePhase::Created { .. }
            )
        {
            return Err(BackendError::InvalidState);
        }
        self.phase = LifecyclePhase::Created { sandbox };
        Ok(())
    }
    pub fn record_stopped(
        &mut self,
        result: &Confirmation<StoppedWriterProof>,
        archived: bool,
    ) -> Result<(), BackendError> {
        let proof = self.confirmed(result, WriterProofKind::StoppedNoProcesses)?;
        if matches!(
            self.phase,
            LifecyclePhase::Intent | LifecyclePhase::Destroyed { .. }
        ) {
            return Err(BackendError::InvalidState);
        }
        let proof = proof.clone();
        self.phase = if archived {
            LifecyclePhase::Archived { proof }
        } else {
            LifecyclePhase::Stopped { proof }
        };
        Ok(())
    }
    pub fn record_preserved(&mut self) -> Result<(), BackendError> {
        match &self.phase {
            LifecyclePhase::Stopped { proof }
            | LifecyclePhase::Archived { proof }
            | LifecyclePhase::Preserved { proof }
                if proof.kind() == WriterProofKind::StoppedNoProcesses
                    && self.matches(proof.sandbox()) =>
            {
                self.phase = LifecyclePhase::Preserved {
                    proof: proof.clone(),
                };
                Ok(())
            }
            _ => Err(BackendError::InvalidState),
        }
    }
    pub fn record_destroyed(
        &mut self,
        result: &Confirmation<StoppedWriterProof>,
        authorization: &DestroyAuthorization,
    ) -> Result<(), BackendError> {
        let proof = self.confirmed(result, WriterProofKind::Destroyed)?;
        let preserved = matches!(
            self.phase,
            LifecyclePhase::Preserved { .. } | LifecyclePhase::Destroyed { .. }
        );
        if !preserved || !authorization.authorizes(proof.sandbox()) {
            return Err(BackendError::InvalidState);
        }
        self.phase = LifecyclePhase::Destroyed {
            proof: proof.clone(),
        };
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RetentionState {
    Unretained,
    Authorized(DestroyAuthorization),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deadline {
    Active,
    Expired,
    Unknown,
}
impl Deadline {
    pub fn at(now_ms: u64, hard_deadline_ms: Option<u64>) -> Self {
        match hard_deadline_ms {
            Some(deadline) if now_ms < deadline => Self::Active,
            Some(_) => Self::Expired,
            None => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconcileAction {
    /// Recover a mapping/monitor only. Server re-adoption remains mandatory.
    Adopt(SandboxIdentity),
    Stop(SandboxIdentity),
    Preserve(SandboxIdentity),
    Destroy {
        sandbox: SandboxIdentity,
        authorization: DestroyAuthorization,
    },
    /// Missing or contradictory evidence: all resources remain protected.
    Hold,
    IgnoreForeign,
}

/// Inventory MUST come from list_owned(expected_node), or an inspect matched
/// against the journal. Without a journal, the caller supplies that scope check.
/// This is a recommendation, not an authority check, grace-period scheduler or
/// capacity release. In particular expiry can only stop/preserve, never delete.
pub fn reconcile(
    journal: Option<&LifecycleRecord>,
    inventory: Option<&InventoryEntry>,
    retention: &RetentionState,
    deadline: Deadline,
) -> ReconcileAction {
    let Some(entry) = inventory else {
        return ReconcileAction::Hold;
    };
    let sandbox = &entry.sandbox;
    if let Some(action) = journal.and_then(|j| j.inventory_conflict(entry)) {
        return action;
    }
    let active_journal = deadline == Deadline::Active && journal.is_some();
    match entry.writer_proof() {
        // A terminal observation is evidence of past destruction, never authority to destroy again.
        Confirmation::Confirmed(proof) if proof.kind() == WriterProofKind::Destroyed => {
            ReconcileAction::Hold
        }
        Confirmation::Confirmed(_) if active_journal => retained_action(sandbox, retention),
        Confirmation::Confirmed(_) => ReconcileAction::Preserve(sandbox.clone()),
        Confirmation::Unconfirmed(_) if active_journal => live_action(entry),
        Confirmation::Unconfirmed(_) => ReconcileAction::Stop(sandbox.clone()),
    }
}
fn retained_action(sandbox: &SandboxIdentity, retention: &RetentionState) -> ReconcileAction {
    match retention {
        RetentionState::Authorized(authorization) if authorization.authorizes(sandbox) => {
            ReconcileAction::Destroy {
                sandbox: sandbox.clone(),
                authorization: authorization.clone(),
            }
        }
        _ => ReconcileAction::Preserve(sandbox.clone()),
    }
}
fn live_action(entry: &InventoryEntry) -> ReconcileAction {
    match entry.state {
        SandboxState::Created | SandboxState::Running => {
            ReconcileAction::Adopt(entry.sandbox.clone())
        }
        _ => ReconcileAction::Stop(entry.sandbox.clone()),
    }
}
