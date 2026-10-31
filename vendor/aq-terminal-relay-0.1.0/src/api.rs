//! Candidate human relay extensions. Existing aq-session-api requests/views are
//! reused; these asynchronous receipts never fabricate a snapshot or principal.
use crate::draft::{ControlStatus, InputMode};
use aq_session_protocol::{OutputFrame, Snapshot, TerminalIdentity};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TerminalInfo {
    pub terminal: TerminalIdentity,
    pub session_revision: u64,
    pub input_mode: InputMode,
    pub terminal_policy: aq_session_protocol::TerminalPolicy,
    pub control: ControlStatus,
    pub last_peer_observed_at_ms: u64,
    pub last_output_observed_at_ms: u64,
    pub disconnected: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    content = "body",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum MutationReply {
    Registered {
        operation_id: Uuid,
        info: TerminalInfo,
    },
    Control {
        operation_id: Uuid,
        status: ControlStatus,
    },
    Watch {
        terminal: TerminalIdentity,
        operation_id: Uuid,
        subscription_id: Uuid,
        session_revision: u64,
        expires_at_ms: u64,
    },
    Input(aq_session_api::InputReceipt),
    Resize {
        terminal: TerminalIdentity,
        operation_id: Uuid,
        sequence: u64,
    },
    Capture {
        terminal: TerminalIdentity,
        operation_id: Uuid,
        capture_request_id: Uuid,
        expires_at_ms: u64,
    },
    Epoch {
        operation_id: Uuid,
        permit: crate::epoch::Permit,
    },
    Released {
        terminal: TerminalIdentity,
        operation_id: Uuid,
        generation: u64,
    },
    Access {
        terminal: TerminalIdentity,
        operation_id: Uuid,
        inspection_revision: u64,
        control_revision: u64,
    },
}
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    content = "body",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum OutputReply {
    Pending {
        terminal: TerminalIdentity,
        subscription_id: Uuid,
        last_peer_observed_at_ms: u64,
        disconnected: bool,
        resynchronize: bool,
    },
    Ready {
        view: Box<aq_session_api::WatchView>,
        snapshot: Snapshot,
        frames: Vec<OutputFrame>,
        next_sequence: u64,
        more: bool,
    },
}
impl std::fmt::Debug for OutputReply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Pending { .. } => "PendingOutput(<scoped metadata>)",
            Self::Ready { .. } => "ReadyOutput(<sensitive snapshot/frames>)",
        })
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AccessUpdate {
    pub terminal: TerminalIdentity,
    pub operation_id: Uuid,
    pub expected_session_revision: u64,
    pub principal_id: Uuid,
    pub expected_inspection_revision: u64,
    pub expected_control_revision: u64,
    pub can_inspect: bool,
    pub can_control: bool,
    pub expires_at_ms: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ViewQuery {
    pub subscription_id: Uuid,
    pub after_sequence: u64,
}
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "contract",
    content = "body",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum RuntimeDocument {
    Terminal(TerminalInfo),
    Mutation(MutationReply),
    Request(Mutation),
    Output(OutputReply),
    Access(AccessUpdate),
    ViewQuery(ViewQuery),
}

#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    content = "body",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Mutation {
    Watch(aq_session_api::WatchRequest),
    Control(aq_session_api::AcquireControlRequest),
    Renew(crate::draft::RenewControl),
    Release(crate::draft::ReleaseControl),
    Input(aq_session_api::InputRequest),
    EpochInput(crate::epoch::Request),
    OpenEpoch(crate::epoch::Open),
    Resize(aq_session_api::ResizeRequest),
    Capture(crate::draft::HumanCapture),
    Resolve(aq_session_api::ResolveInputRequest),
}
