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
        let Confirmation::Confirmed(proof) = result else {
            return Err(BackendError::InvalidState);
        };
        if !self.matches(proof.sandbox())
            || proof.kind() != WriterProofKind::StoppedNoProcesses
            || matches!(
                self.phase,
                LifecyclePhase::Intent | LifecyclePhase::Destroyed { .. }
            )
        {
            return Err(BackendError::InvalidState);
        }
        self.phase = if archived {
            LifecyclePhase::Archived {
                proof: proof.clone(),
            }
        } else {
            LifecyclePhase::Stopped {
                proof: proof.clone(),
            }
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
        let Confirmation::Confirmed(proof) = result else {
            return Err(BackendError::InvalidState);
        };
        if !self.matches(proof.sandbox())
            || proof.kind() != WriterProofKind::Destroyed
            || !authorization.authorizes(proof.sandbox())
            || !matches!(
                self.phase,
                LifecyclePhase::Preserved { .. } | LifecyclePhase::Destroyed { .. }
            )
        {
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
    if let Some(journal) = journal {
        if journal.request.owner.node_id != sandbox.owner.node_id {
            return ReconcileAction::IgnoreForeign;
        }
        // A terminal journal cannot be reconciled into a live mapping. Treat
        // stale inventory or backend ID reuse as contradictory evidence.
        if matches!(journal.phase, LifecyclePhase::Destroyed { .. })
            && entry.state != SandboxState::Destroyed
        {
            return ReconcileAction::Hold;
        }
        if journal.request.validate().is_err()
            || !journal.matches(sandbox)
            || journal.request.name != entry.name
            || journal.request.template_digest != entry.template_digest
            || journal.request.hard_deadline_ms != entry.hard_deadline_ms
        {
            return ReconcileAction::Hold;
        }
    }
    let proof = entry.writer_proof();
    if matches!(proof, Confirmation::Confirmed(ref proof) if proof.kind() == WriterProofKind::Destroyed)
    {
        // Already gone. The controller may record that evidence, but this is not
        // a request to destroy again or proof that retention ever succeeded.
        return ReconcileAction::Hold;
    }
    if deadline != Deadline::Active {
        return if proof.is_confirmed() {
            ReconcileAction::Preserve(sandbox.clone())
        } else {
            ReconcileAction::Stop(sandbox.clone())
        };
    }
    if proof.is_confirmed() {
        if journal.is_some()
            && let RetentionState::Authorized(authorization) = retention
            && authorization.authorizes(sandbox)
        {
            return ReconcileAction::Destroy {
                sandbox: sandbox.clone(),
                authorization: authorization.clone(),
            };
        }
        return ReconcileAction::Preserve(sandbox.clone());
    }
    if journal.is_none() || entry.state == SandboxState::Unknown {
        ReconcileAction::Stop(sandbox.clone())
    } else if matches!(entry.state, SandboxState::Created | SandboxState::Running) {
        ReconcileAction::Adopt(sandbox.clone())
    } else {
        // A stopped label with unknown/nonzero writers still needs a real stop.
        ReconcileAction::Stop(sandbox.clone())
    }
}
