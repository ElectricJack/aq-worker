//! Bounded local transport intents. This is not a graph replica or effect ledger.
use crate::files::PrivateDir;
use anyhow::{Context as _, Result, ensure};
use aq_agent_api::{Binding, canonical, digest, valid};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, fs::File, path::Path};
use uuid::Uuid;

const MAX_OPERATIONS: usize = 128;
const MAX_JOURNAL_BYTES: usize = 16 * 1024 * 1024;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Stamp {
    pub boot_id: Uuid,
    pub boottime_ms: u64,
    pub unix_ms: u64,
}
impl Stamp {
    pub fn now() -> Result<Self> {
        let boot_id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?
            .trim()
            .parse()?;
        let mut time = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        ensure!(
            unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut time) } == 0
                && time.tv_sec >= 0,
            "boottime unavailable"
        );
        let unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis()
            .try_into()?;
        Ok(Self {
            boot_id,
            boottime_ms: (time.tv_sec as u64)
                .checked_mul(1000)
                .and_then(|n| n.checked_add(time.tv_nsec as u64 / 1_000_000))
                .context("boottime overflow")?,
            unix_ms,
        })
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Operation {
    pub request: Value,
    pub payload_digest: String,
    pub attempts: u32,
    pub first: Option<Stamp>,
    pub last: Option<Stamp>,
    pub max_attempts: u32,
    pub max_elapsed_ms: u64,
    pub next_retry_ms: u64,
    pub response: Option<Value>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    format_version: u32,
    binding: Binding,
    operations: BTreeMap<Uuid, Operation>,
}
pub(crate) struct Journal {
    dir: PrivateDir,
    _owner: File,
    state: State,
    poisoned: bool,
}
impl Journal {
    pub fn open(root: &Path, binding: Binding, policy: &Value) -> Result<Self> {
        binding.validate()?;
        ensure!(valid("RetryBudget", policy), "retry policy invalid");
        let dir = PrivateDir::open(root)?;
        let owner = dir.lock()?;
        let state = match dir.optional("operations.json", MAX_JOURNAL_BYTES)? {
            Some(bytes) => {
                let aq_agent_api::StrictValue(value) = serde_json::from_slice(&bytes)?;
                let state: State = serde_json::from_value(value)?;
                ensure!(
                    state.format_version == 1
                        && state.binding == binding
                        && state.operations.len() <= MAX_OPERATIONS,
                    "client journal binding/version/bound refused"
                );
                for (id, operation) in &state.operations {
                    ensure!(
                        valid("CommandRequest", &operation.request)
                            && operation.request["operation_id"] == id.to_string()
                            && operation.request["binding"] == serde_json::to_value(&binding)?
                            && digest(&operation.request)? == operation.payload_digest
                            && canonical(&operation.request)?.len()
                                <= aq_agent_api::MAX_DOCUMENT_BYTES
                            && Some(u64::from(operation.max_attempts))
                                == policy["max_attempts"].as_u64()
                            && operation.attempts <= operation.max_attempts
                            && Some(operation.max_elapsed_ms) == policy["max_elapsed_ms"].as_u64()
                            && (operation.attempts == 0) == operation.first.is_none()
                            && operation.first.is_some() == operation.last.is_some(),
                        "corrupt local intent"
                    );
                    if let (Some(first), Some(last)) = (&operation.first, &operation.last) {
                        ensure!(
                            !first.boot_id.is_nil()
                                && first.boot_id == last.boot_id
                                && first.boottime_ms <= last.boottime_ms
                                && first.unix_ms <= last.unix_ms,
                            "corrupt retry clock history"
                        );
                    }
                    if let Some(response) = &operation.response {
                        check_operation_response(
                            response,
                            *id,
                            &operation.payload_digest,
                            &binding,
                        )?;
                    }
                }
                state
            }
            None => State {
                format_version: 1,
                binding,
                operations: BTreeMap::new(),
            },
        };
        Ok(Self {
            dir,
            _owner: owner,
            state,
            poisoned: false,
        })
    }
    fn commit<T>(&mut self, change: impl FnOnce(&mut State) -> Result<T>) -> Result<T> {
        ensure!(
            !self.poisoned,
            "local intent persistence uncertain; reopen required"
        );
        let mut candidate = self.state.clone();
        let result = change(&mut candidate)?;
        let bytes = serde_json::to_vec(&candidate)?;
        ensure!(
            bytes.len() <= MAX_JOURNAL_BYTES,
            "local intent retention full"
        );
        self.poisoned = true;
        self.dir.write("operations.json", &bytes)?;
        self.state = candidate;
        self.poisoned = false;
        Ok(result)
    }
    pub fn prepare(&mut self, request: Value, policy: &Value) -> Result<Uuid> {
        ensure!(
            valid("CommandRequest", &request)
                && valid("RetryBudget", policy)
                && request["binding"] == serde_json::to_value(&self.state.binding)?,
            "command scope/contract refused"
        );
        let id: Uuid = request["operation_id"]
            .as_str()
            .context("operation ID absent")?
            .parse()?;
        let payload_digest = digest(&request)?;
        if let Some(old) = self.state.operations.get(&id) {
            ensure!(
                old.payload_digest == payload_digest && old.request == request,
                "operation ID payload conflict"
            );
            return Ok(id);
        }
        let operation = Operation {
            request,
            payload_digest,
            attempts: 0,
            first: None,
            last: None,
            max_attempts: policy["max_attempts"]
                .as_u64()
                .context("retry count absent")?
                .try_into()?,
            max_elapsed_ms: policy["max_elapsed_ms"]
                .as_u64()
                .context("retry deadline absent")?,
            next_retry_ms: 0,
            response: None,
        };
        self.commit(|s| {
            ensure!(
                s.operations.len() < MAX_OPERATIONS,
                "local intent retention full"
            );
            s.operations.insert(id, operation);
            Ok(id)
        })
    }
    pub fn operation(&self, id: Uuid) -> Result<&Operation> {
        self.state
            .operations
            .get(&id)
            .context("local operation absent")
    }
    /// The attempt count and uncertainty boundary are durable before HTTP.
    pub fn begin_send(&mut self, id: Uuid, now: Stamp, delay_ms: u64) -> Result<Vec<u8>> {
        self.commit(|s| {
            let op = s
                .operations
                .get_mut(&id)
                .context("local operation absent")?;
            ensure!(op.attempts < op.max_attempts, "local retry count exhausted");
            if let Some(first) = &op.first {
                let last = op.last.as_ref().unwrap();
                ensure!(
                    now.boot_id == first.boot_id
                        && now.boottime_ms >= last.boottime_ms
                        && now.unix_ms >= last.unix_ms
                        && now.boottime_ms.saturating_sub(first.boottime_ms) < op.max_elapsed_ms
                        && now.boottime_ms >= op.next_retry_ms,
                    "retry budget expired, clock uncertain or retry too early"
                );
            }
            op.first.get_or_insert_with(|| now.clone());
            op.attempts += 1;
            op.next_retry_ms = now
                .boottime_ms
                .checked_add(delay_ms)
                .context("retry clock overflow")?;
            op.last = Some(now);
            canonical(&op.request).map_err(Into::into)
        })
    }
    pub fn record_response(&mut self, id: Uuid, value: Value) -> Result<()> {
        let op = self.operation(id)?;
        check_operation_response(&value, id, &op.payload_digest, &self.state.binding)?;
        // Confirmed effects cannot be replaced by a later inconclusive answer.
        if let Some(old) = &op.response
            && terminal(old)
        {
            ensure!(old == &value, "terminal operation response changed");
            return Ok(());
        }
        self.commit(|s| {
            s.operations.get_mut(&id).unwrap().response = Some(value);
            Ok(())
        })
    }
}
pub(crate) fn terminal(value: &Value) -> bool {
    matches!(
        value["state"].as_str(),
        Some("confirmed" | "conflict" | "rejected")
    )
}
pub(crate) fn check_operation_response(
    value: &Value,
    id: Uuid,
    payload: &str,
    binding: &Binding,
) -> Result<()> {
    ensure!(
        valid("OperationStatus", value)
            && value["operation_id"] == id.to_string()
            && value["payload_digest"] == payload
            && value["binding"] == serde_json::to_value(binding)?,
        "operation response association refused"
    );
    if value["state"] == "confirmed" {
        ensure!(
            value["receipt"]["workspace_id"] == binding.workspace_id.to_string(),
            "operation receipt workspace mismatch"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;
    fn fixture() -> (Value, Binding, Value) {
        let base: Value =
            serde_json::from_str(include_str!("../tests/fixtures/agent-base.json")).unwrap();
        (
            base["command_requests"]["correct_record"].clone(),
            serde_json::from_value(base["context"]["binding"].clone()).unwrap(),
            json!({"max_attempts":2,"max_elapsed_ms":1000,"initial_delay_ms":100,"max_delay_ms":200}),
        )
    }
    #[test]
    fn restart_cannot_reset_delay_attempts_or_clock_fences() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let (request, binding, policy) = fixture();
        let mut journal = Journal::open(root.path(), binding.clone(), &policy).unwrap();
        let id = journal.prepare(request.clone(), &policy).unwrap();
        let first = Stamp {
            boot_id: Uuid::new_v4(),
            boottime_ms: 1000,
            unix_ms: 5000,
        };
        let bytes = journal.begin_send(id, first.clone(), 200).unwrap();
        drop(journal);
        let mut journal = Journal::open(root.path(), binding, &policy).unwrap();
        // Even a newly supplied wider policy cannot extend the original budget.
        let mut wider = policy.clone();
        wider["max_attempts"] = json!(5);
        assert_eq!(journal.prepare(request, &wider).unwrap(), id);
        for stamp in [
            Stamp {
                boottime_ms: 1100,
                ..first.clone()
            },
            Stamp {
                boot_id: Uuid::new_v4(),
                boottime_ms: 1500,
                ..first.clone()
            },
            Stamp {
                boottime_ms: 1500,
                unix_ms: 4999,
                ..first.clone()
            },
            Stamp {
                boottime_ms: 2000,
                ..first.clone()
            },
        ] {
            assert!(journal.begin_send(id, stamp, 100).is_err());
            assert_eq!(journal.operation(id).unwrap().attempts, 1);
        }
        assert_eq!(
            journal
                .begin_send(
                    id,
                    Stamp {
                        boottime_ms: 1200,
                        ..first.clone()
                    },
                    200
                )
                .unwrap(),
            bytes
        );
        assert!(
            journal
                .begin_send(
                    id,
                    Stamp {
                        boottime_ms: 1400,
                        ..first
                    },
                    200
                )
                .is_err()
        );
        assert_eq!(journal.operation(id).unwrap().attempts, 2);
    }
    #[test]
    fn retained_retry_policy_cannot_widen_the_resolved_profile() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let (request, binding, policy) = fixture();
        let mut journal = Journal::open(root.path(), binding.clone(), &policy).unwrap();
        let id = journal.prepare(request, &policy).unwrap();
        drop(journal);
        let dir = PrivateDir::open(root.path()).unwrap();
        let original: Value =
            serde_json::from_slice(&dir.read("operations.json", MAX_JOURNAL_BYTES).unwrap())
                .unwrap();
        for (field, wider) in [("max_attempts", 3), ("max_elapsed_ms", 2000)] {
            let mut changed = original.clone();
            changed["operations"][id.to_string()][field] = json!(wider);
            dir.write("operations.json", &serde_json::to_vec(&changed).unwrap())
                .unwrap();
            assert!(Journal::open(root.path(), binding.clone(), &policy).is_err());
        }
    }
    #[test]
    fn concurrent_clients_and_full_intent_retention_refuse_without_eviction() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let (mut request, binding, policy) = fixture();
        let mut journal = Journal::open(root.path(), binding.clone(), &policy).unwrap();
        assert!(Journal::open(root.path(), binding.clone(), &policy).is_err());
        let first = journal.prepare(request.clone(), &policy).unwrap();
        for _ in 1..MAX_OPERATIONS {
            request["operation_id"] = json!(Uuid::new_v4());
            journal.prepare(request.clone(), &policy).unwrap();
        }
        let retained = std::fs::read(root.path().join("operations.json")).unwrap();
        request["operation_id"] = json!(Uuid::new_v4());
        assert!(journal.prepare(request, &policy).is_err());
        assert_eq!(
            std::fs::read(root.path().join("operations.json")).unwrap(),
            retained
        );
        drop(journal);
        let journal = Journal::open(root.path(), binding, &policy).unwrap();
        assert!(journal.operation(first).is_ok());
    }
}
