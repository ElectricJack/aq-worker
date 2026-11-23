use super::*;

/// A record that the service must keep the sandbox. Retain the `lost-<attempt>.json`
/// path so a running older service also preserves a quarantined sandbox.
pub(super) fn preserved_sandbox_record(state_dir: &Path, attempt: Uuid) -> PathBuf {
    state_dir.join(format!("lost-{attempt}.json"))
}
/// The proof first sent for a lost attempt (`lost-proof-<attempt>.json`), written before the
/// first post and reused by every retry and restart: the server accepts one proof per attempt.
fn sent_proof(
    state_dir: &Path,
    attempt: Uuid,
    found: aq_protocol::StopProof,
) -> Result<aq_protocol::StopProof> {
    let path = state_dir.join(format!("lost-proof-{attempt}.json"));
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).context("stored lost-attempt proof"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            write_private(&path, &serde_json::to_vec(&found)?)?;
            Ok(found)
        }
        Err(e) => Err(e).context("read the stored lost-attempt proof"),
    }
}

pub(super) struct LostAttempt<'a> {
    pub(super) attempt: Uuid,
    pub(super) proof: aq_protocol::StopProof,
    pub(super) claim_unreported: bool,
    pub(super) why: &'a str,
}
/// D2 ruling C: a lost attempt with an outside stop proof. Its sandboxes are stopped (kept),
/// the proof is stored, then the server is asked to fail it and retry its task
/// (`LostAttemptRelease`). `lost-<attempt>.json` in `state_dir` then records that its unsaved
/// work was not salvaged (and whether a claim recorded before the cut left its work in the
/// archived worktree) and marks its sandbox as kept. The slot is freed (`Lost`) once the
/// server released the task for its retry (or had released it with an earlier proof); while
/// the server holds the task (an exhausted budget) or refuses, the slot is held.
pub(super) async fn release_lost(
    stop: impl AsyncFnOnce() -> Result<()>,
    post: impl AsyncFnOnce(aq_protocol::LostAttemptRelease) -> Result<aq_protocol::LostAttemptReleased>,
    state_dir: &Path,
    loss: LostAttempt<'_>,
) -> RecoverOutcome {
    let LostAttempt {
        attempt,
        proof,
        claim_unreported,
        why,
    } = loss;
    let held = |why: String| {
        tracing::error!(%attempt, "recovery held: {why}");
        RecoverOutcome::Held(why)
    };
    if let Err(e) = stop().await {
        return held(format!(
            "the lost attempt's sandbox could not be stopped: {e:#}; {why}"
        ));
    }
    let proof = match sent_proof(state_dir, attempt, proof) {
        Ok(proof) => proof,
        Err(e) => return held(format!("{e:#}; {why}")),
    };
    let request = aq_protocol::LostAttemptRelease {
        schema_version: 1,
        attempt_id: attempt,
        proof: proof.clone(),
    };
    let mut record = loss_record(attempt, &proof, claim_unreported);
    let outcome = server_outcome(post(request).await, &mut record, why);
    // Written on every outcome: the record also marks the sandbox as kept, so without it the
    // slot is held (settle would otherwise release, and destroy, a claimed attempt's sandbox).
    let path = preserved_sandbox_record(state_dir, attempt);
    if let Err(e) = write_private(&path, record.to_string().as_bytes()) {
        return held(format!(
            "the lost-attempt record was not written: {e:#}; {why}"
        ));
    }
    match &outcome {
        RecoverOutcome::Lost => tracing::warn!(%attempt, %record,
            "lost attempt released: its unsaved work was not salvaged; its sandbox is kept for release-sandbox"),
        _ => tracing::error!(%attempt, %record, "lost attempt not released; the slot is held"),
    }
    outcome
}

fn loss_record(
    attempt: Uuid,
    proof: &aq_protocol::StopProof,
    claim_unreported: bool,
) -> serde_json::Value {
    let mut record = serde_json::json!({"schema_version": 1, "attempt_id": attempt, "proof": proof,
        "salvaged": false, "claim_unreported": claim_unreported,
        "sandbox": "stopped and kept; release it with `aq-worker service release-sandbox`"});
    if claim_unreported {
        record["note"] =
            "a result claim was recorded but never reported: its work is in the archived worktree"
                .into();
    }
    record
}

fn server_outcome(
    response: Result<aq_protocol::LostAttemptReleased>,
    record: &mut serde_json::Value,
    why: &str,
) -> RecoverOutcome {
    match response {
        Ok(r) => {
            (record["reason"], record["charged"]) = (json_of(&r.reason), r.charged.into());
            (record["released"], record["held"]) = (r.released.into(), json_of(&r.held));
            match (r.released, &r.held) {
                (true, _) => RecoverOutcome::Lost,
                (false, held) => RecoverOutcome::Held(format!(
                    "the server failed the lost attempt but holds its task: {}",
                    held.as_deref().unwrap_or("unknown")
                )),
            }
        }
        Err(e) if format!("{e:#}").contains("lost_attempt_released_with_another_proof") => {
            record["released"] = "earlier, with another proof".into();
            RecoverOutcome::Lost
        }
        Err(e) => RecoverOutcome::Held(format!(
            "the server refused the lost-attempt release: {e:#}; {why}"
        )),
    }
}
fn json_of<T: serde::Serialize>(v: &T) -> serde_json::Value {
    serde_json::to_value(v).unwrap_or_default()
}
/// The claimed attempts whose sandboxes the service may destroy: not those that went through
/// the lost path (their sandboxes are kept for the operator).
pub(super) fn destroyable(state_dir: &Path, claimed: Vec<Uuid>) -> Vec<Uuid> {
    claimed
        .into_iter()
        .filter(|a| !preserved_sandbox_record(state_dir, *a).exists())
        .collect()
}
