//! `aq environment try` (aq-server spec 0017 §6.2). The agent has no git, so
//! for `POST /v1/agent/environments/try` the node pushes the agent's live
//! workspace and the channel sets the body's `commit` to the pushed commit.
//! A `commit` the agent sent is never forwarded.
use crate::forward::{Credentials, Failure};
use std::{future::Future, pin::Pin, sync::Arc};

/// The one route whose body the channel rewrites.
pub const TRY_PATH: &str = "/v1/agent/environments/try";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckpointError {
    /// The workspace holds a credential shape; nothing was pushed.
    Refused,
    /// The snapshot, commit or push failed, or outran the request timeout.
    PushFailed,
}

pub type CheckpointFuture = Pin<Box<dyn Future<Output = Result<String, CheckpointError>> + Send>>;
/// Pushes the attempt's live workspace and answers the pushed commit.
pub type Checkpoint = Arc<dyn Fn() -> CheckpointFuture + Send + Sync>;

/// The body to forward: the agent's JSON object, re-serialized, with `commit`
/// set to the node's checkpoint (or dropped when the attempt has none, which
/// aq-server answers `workspace_not_pushed`).
pub(crate) async fn attest(body: &[u8], credentials: &dyn Credentials) -> Result<Vec<u8>, Failure> {
    let Ok(serde_json::Value::Object(mut object)) = serde_json::from_slice(body) else {
        return Err(Failure::Framing);
    };
    object.remove("commit");
    if let Some(pushed) = credentials.checkpoint() {
        let commit = pushed.await.map_err(Failure::Checkpoint)?;
        let hex = |b: u8| b.is_ascii_digit() || (b'a'..=b'f').contains(&b);
        if commit.len() != 40 || !commit.bytes().all(hex) {
            return Err(Failure::Checkpoint(CheckpointError::PushFailed));
        }
        object.insert("commit".into(), commit.into());
    }
    serde_json::to_vec(&object).map_err(|_| Failure::Framing)
}
