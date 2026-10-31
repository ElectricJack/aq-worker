//! Trusted local aq-git socket adapter. Never accepts operations or paths from an agent.
use anyhow::{Context, Result, ensure};
use aq_worker_protocol::{ObjectId, SourceBinding, Validate};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    time::Duration,
};
use uuid::Uuid;
const MAX_REPLY: u64 = 8 * 1024 * 1024;
#[derive(Debug, Clone)]
pub struct GitClient {
    socket: PathBuf,
    root: PathBuf,
    maximum_source_disk_bytes: u64,
    maximum_artifact_bytes: u64,
    maximum_pool_bytes: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingGitOperation {
    pub operation_id: Uuid,
    pub request: Value,
    pub result: Option<Value>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceState {
    pub binding: SourceBinding,
    #[serde(default)]
    pub managed_root: Option<PathBuf>,
    #[serde(default)]
    pub transfer: Option<aq_worker_protocol::TransferManifest>,
    #[serde(default)]
    pub pool_reservation_bytes: u64,
    #[serde(default)]
    pub cache_evicted: bool,
    #[serde(default)]
    pub imported_bases: std::collections::BTreeSet<Uuid>,
    #[serde(default)]
    pub workspace_bytes: u64,
    pub operations: BTreeMap<String, PendingGitOperation>,
    pub workspace: Option<Workspace>,
    pub checkpoint: Option<Checkpoint>,
    pub exported: Option<Value>,
    pub receipt: Option<aq_worker_protocol::CentralRetentionReceipt>,
    pub retired: bool,
}
impl SourceState {
    pub fn new(binding: SourceBinding) -> Self {
        Self {
            binding,
            managed_root: None,
            transfer: None,
            pool_reservation_bytes: 0,
            cache_evicted: false,
            imported_bases: Default::default(),
            workspace_bytes: 0,
            operations: BTreeMap::new(),
            workspace: None,
            checkpoint: None,
            exported: None,
            receipt: None,
            retired: false,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    pub workspace_id: Uuid,
    pub generation: u64,
    pub path: PathBuf,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    pub workspace_id: Uuid,
    pub workspace_generation: u64,
    pub source: ObjectId,
    pub retention_handle: Uuid,
}
#[derive(Debug, Clone, Serialize)]
pub enum GitAction {
    Register,
    Create,
    Import {
        artifact_handle: Uuid,
    },
    Release {
        handle_id: Uuid,
        generation: u64,
        refs: Vec<RetainedRef>,
    },
    Checkpoint {
        workspace_id: Uuid,
        generation: u64,
        head: ObjectId,
    },
    Export {
        source: ObjectId,
        retention_handle: Uuid,
    },
    Retire {
        workspace_id: Uuid,
        generation: u64,
        head: ObjectId,
    },
    Evict {
        storage_generation: u64,
    },
}
impl GitAction {
    fn parts(&self, b: &SourceBinding) -> (&'static str, Value, u64) {
        match self {
            Self::Register => (
                "repo.register",
                json!({"binding":{"identity":b.logical_identity,"mode":"worker","binding_generation":b.binding_generation,"object_format":"sha1"}}),
                0,
            ),
            Self::Import { artifact_handle } => (
                "artifact.import",
                json!({"artifact_handle":artifact_handle}),
                b.binding_generation,
            ),
            Self::Release {
                handle_id,
                generation,
                refs,
            } => (
                "retention.release",
                json!({"handle_id":handle_id,"expected_generation":generation,"refs":refs,"reason":"central handoff retained and local execution released"}),
                b.binding_generation,
            ),
            Self::Create => (
                "workspace.create",
                json!({"base":b.base}),
                b.binding_generation,
            ),
            Self::Checkpoint {
                workspace_id,
                generation,
                head,
            } => (
                "workspace.checkpoint",
                json!({"workspace_id":workspace_id,"expected_generation":generation,"expected_head":head,"quiescent":true,"mode":"all","commit":{"name":"AQ Worker","email":"worker@invalid","message":"Retained execution checkpoint"}}),
                b.binding_generation,
            ),
            Self::Export {
                source,
                retention_handle,
            } => (
                "artifact.export",
                json!({"source":source,"base":b.base,"retention_handle":retention_handle}),
                b.binding_generation,
            ),
            Self::Retire {
                workspace_id,
                generation,
                head,
            } => (
                "workspace.retire",
                json!({"workspace_id":workspace_id,"expected_generation":generation,"expected_head":head,"quiescent":true}),
                b.binding_generation,
            ),
            Self::Evict { storage_generation } => (
                "repo.cache-evict",
                json!({"expected_storage_generation":storage_generation,"reconstructible":true,"reason":"bounded worker cache reclamation"}),
                b.binding_generation,
            ),
        }
    }
    pub fn request(&self, b: &SourceBinding, id: Uuid) -> Value {
        let (op, params, generation) = self.parts(b);
        json!({"protocol_version":1,"request_id":id.to_string(),"operation":op,"repository_id":b.repository_id,"operation_id":id,"expected_binding_generation":generation,"params":params})
    }
}
impl GitClient {
    /// Operator-owned runtime paths only. A caller cannot select a socket per assignment.
    pub fn new(socket: &Path, root: &Path) -> Result<Self> {
        ensure!(
            socket.is_absolute() && root.is_absolute(),
            "aq-git paths must be absolute"
        );
        let root = std::fs::canonicalize(root)?;
        ensure!(
            !std::fs::symlink_metadata(socket)?.file_type().is_symlink(),
            "aq-git socket symlink refused"
        );
        use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
        let meta = std::fs::metadata(socket)?;
        ensure!(
            meta.file_type().is_socket()
                && meta.uid() == unsafe { libc::geteuid() }
                && meta.permissions().mode() & 0o077 == 0,
            "untrusted aq-git socket"
        );
        ensure!(
            std::fs::metadata(&root)?.uid() == unsafe { libc::geteuid() }
                && std::fs::metadata(&root)?.permissions().mode() & 0o077 == 0,
            "aq-git root must be private"
        );
        let mut client = Self {
            socket: socket.into(),
            root,
            maximum_source_disk_bytes: 0,
            maximum_artifact_bytes: 0,
            maximum_pool_bytes: 0,
        };
        let capabilities=client.exchange(&json!({"protocol_version":1,"request_id":Uuid::new_v4().to_string(),"operation":"capabilities","params":{}}))?;
        ensure!(
            capabilities["protocol_version"] == 1
                && capabilities["artifact_format_version"] == 1
                && capabilities["git"]["checkout_profile"] == "no_attributes_v1",
            "unsupported aq-git profile"
        );
        let operations = capabilities["operations"]
            .as_array()
            .context("missing aq-git operation capabilities")?;
        ensure!(
            [
                "repo.show",
                "artifact.import",
                "workspace.create",
                "workspace.checkpoint",
                "workspace.retire",
                "artifact.export",
                "retention.release",
                "repo.cache-evict"
            ]
            .iter()
            .all(|op| operations.iter().any(|v| v == op)),
            "aq-git lacks required workspace operations"
        );
        let expanded = capabilities["limits"]["artifact"]["max_expanded_bytes"]
            .as_u64()
            .context("aq-git must advertise expanded source bound")?;
        let bundle = capabilities["limits"]["artifact"]["max_bundle_bytes"]
            .as_u64()
            .context("aq-git must advertise bundle bound")?;
        client.maximum_artifact_bytes = bundle
            .checked_add(1024 * 1024)
            .context("artifact reservation overflow")?;
        client.maximum_pool_bytes = expanded;
        client.maximum_source_disk_bytes = expanded
            .checked_add(client.maximum_artifact_bytes)
            .context("source reservation overflow")?;
        Ok(client)
    }
    pub fn validate_storage_profile(&self, max_bundle: u64, max_expanded: u64) -> Result<()> {
        ensure!(
            self.maximum_artifact_bytes <= max_bundle.saturating_add(1024 * 1024)
                && self.maximum_pool_bytes <= max_expanded,
            "aq-git storage capabilities exceed configured source profile; install documented artifact_limits.json while admission is stopped"
        );
        Ok(())
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn exchange(&self, request: &Value) -> Result<Value> {
        let mut stream = UnixStream::connect(&self.socket)
            .context("aq-git unavailable; retain operation identity")?;
        stream.set_read_timeout(Some(Duration::from_secs(30)))?;
        stream.set_write_timeout(Some(Duration::from_secs(10)))?;
        let bytes = serde_json::to_vec(request)?;
        ensure!(bytes.len() <= 1024 * 1024, "aq-git request too large");
        stream.write_all(&bytes)?;
        stream.write_all(b"\n")?;
        use std::io::Read;
        let mut frame = Vec::new();
        BufReader::new(stream)
            .take(MAX_REPLY + 1)
            .read_until(b'\n', &mut frame)?;
        ensure!(
            frame.len() as u64 <= MAX_REPLY && frame.last() == Some(&b'\n'),
            "invalid/oversized aq-git reply; outcome unknown"
        );
        let response: Value = serde_json::from_slice(&frame)?;
        ensure!(
            response["protocol_version"] == 1 && response["request_id"] == request["request_id"],
            "aq-git reply binding/version mismatch; outcome unknown"
        );
        if let Some(id) = response.get("operation_id") {
            ensure!(
                Some(id) == request.get("operation_id"),
                "aq-git operation reply mismatch"
            );
        }
        ensure!(
            response["ok"].as_bool().is_some()
                && response.get("result").is_some() != response.get("error").is_some(),
            "contradictory aq-git reply"
        );
        if response["ok"] == true && request.get("operation_id").is_some() {
            ensure!(
                response.get("operation_id") == request.get("operation_id"),
                "successful aq-git mutation lacks matching operation identity"
            );
        }
        ensure!(
            response["ok"] == true,
            "aq-git refused: {} {}",
            response["error"]["code"],
            response["error"]["message"]
        );
        response
            .get("result")
            .cloned()
            .context("missing aq-git result")
    }
    pub fn query(&self, b: &SourceBinding, operation: &str, params: Value) -> Result<Value> {
        ensure!(
            matches!(
                operation,
                "repo.show" | "workspace.inspect" | "retention.inspect" | "inspect"
            ),
            "unsupported local query"
        );
        self.exchange(&json!({"protocol_version":1,"request_id":Uuid::new_v4().to_string(),"operation":operation,"repository_id":b.repository_id,"expected_binding_generation":b.binding_generation,"params":params}))
    }
    pub fn validate_workspace(&self, w: &Workspace) -> Result<()> {
        ensure!(
            !w.workspace_id.is_nil() && w.generation > 0,
            "invalid workspace response"
        );
        let path = std::fs::canonicalize(&w.path)?;
        ensure!(
            path.starts_with(self.root.join("workspaces")) && path != self.root.join("workspaces"),
            "workspace outside aq-git managed root"
        );
        Ok(())
    }
    pub fn base_available(&self, b: &SourceBinding) -> Result<()> {
        self.validate_binding(b)?;
        self.query(b, "inspect", json!({"oid":b.base}))?;
        Ok(())
    }
    pub fn validate_binding(&self, b: &SourceBinding) -> Result<()> {
        b.validate()?;
        let r = self.query(b, "repo.show", json!({}))?;
        ensure!(
            r["repository_id"] == b.repository_id.to_string()
                && r["binding_generation"] == b.binding_generation
                && r["identity"] == b.logical_identity
                && r["mode"] == "worker",
            "local repository binding mismatch"
        );
        Ok(())
    }
}

impl<C: crate::clock::Clock> crate::runtime::Runtime<C> {
    /// Admission uses exactly the ordinary capacity rules, after checking the local binding.
    /// No workspace allocation occurs until prepare_source, and eligibility creates none.
    pub fn reserve_source(
        &mut self,
        offer: aq_worker_protocol::Offer,
        git: &GitClient,
    ) -> Result<aq_worker_protocol::AcceptRequest> {
        if let Some(record) = self.journal.state().attempts.get(&offer.attempt.attempt_id) {
            ensure!(
                record
                    .source
                    .as_ref()
                    .and_then(|s| s.managed_root.as_deref())
                    == Some(git.root()),
                "changed source runtime root"
            );
            return self.reserve_inner(offer, true, git.maximum_artifact_bytes);
        }
        let binding = offer
            .spec
            .bindings
            .as_ref()
            .and_then(|b| b.source.clone())
            .context("source binding required")?;
        ensure!(
            offer.spec.resources.disk_bytes >= git.maximum_source_disk_bytes,
            "source disk reservation below conservative aq-git pool/workspace/artifact bound"
        );
        ensure!(
            self.journal.limits().max_pending_bytes >= git.maximum_artifact_bytes,
            "pending source budget below aq-git artifact bound"
        );
        let has_pool = self.journal.state().attempts.values().any(|r| {
            r.source.as_ref().is_some_and(|s| {
                s.binding.repository_id == binding.repository_id
                    && s.managed_root.as_deref() == Some(git.root())
                    && !s.cache_evicted
            })
        });
        let pool = if has_pool { 0 } else { git.maximum_pool_bytes };
        let projected = offer
            .spec
            .resources
            .disk_bytes
            .checked_add(pool)
            .context("source disk accounting overflow")?;
        ensure!(
            projected <= self.journal.available()?.disk_bytes,
            "source workspace and shared cache budget unavailable"
        );
        ensure!(
            projected
                <= fs2::available_space(git.root())?
                    .saturating_sub(self.journal.limits().minimum_free_disk_bytes)
                    .saturating_sub(self.journal.protected_usage()?.disk_bytes),
            "source volume disk headroom unavailable"
        );
        git.validate_binding(&binding)?;
        // Preserve the frozen offer/digest: the common admission path is told this
        // capability has been checked, using the internal variant below.
        let id = offer.attempt.attempt_id;
        let acceptance = self.reserve_inner(offer, true, git.maximum_artifact_bytes)?;
        self.journal.transaction(|s| {
            let r = s.attempts.get_mut(&id).unwrap();
            if let Some(prior) = &r.source {
                ensure!(prior.binding == binding, "changed source binding");
                ensure!(
                    prior
                        .managed_root
                        .as_ref()
                        .is_none_or(|root| root == git.root()),
                    "changed source runtime root"
                );
            } else {
                r.source = Some(SourceState::new(binding));
            }
            r.source.as_mut().unwrap().managed_root = Some(git.root().to_path_buf());
            r.source.as_mut().unwrap().pool_reservation_bytes = git.maximum_pool_bytes;
            Ok(())
        })?;
        Ok(acceptance)
    }
    fn source_operation(
        &mut self,
        id: Uuid,
        key: &str,
        action: GitAction,
        git: &GitClient,
    ) -> Result<Value> {
        let source = self
            .journal
            .state()
            .attempts
            .get(&id)
            .and_then(|r| r.source.as_ref())
            .context("source state missing")?;
        ensure!(
            source.managed_root.as_deref() == Some(git.root()),
            "source runtime root binding mismatch"
        );
        let prior = source.operations.get(key).cloned();
        let operation_id = prior.as_ref().map_or_else(Uuid::new_v4, |p| p.operation_id);
        let request = action.request(&source.binding, operation_id);
        if let Some(p) = &prior {
            ensure!(p.request == request, "source operation identity conflict");
            if let Some(result) = &p.result {
                return Ok(result.clone());
            }
        } else {
            ensure!(
                source.operations.len() < 64,
                "source operation journal bound reached"
            );
            self.journal.transaction(|s| {
                s.attempts
                    .get_mut(&id)
                    .unwrap()
                    .source
                    .as_mut()
                    .unwrap()
                    .operations
                    .insert(
                        key.into(),
                        PendingGitOperation {
                            operation_id,
                            request: request.clone(),
                            result: None,
                        },
                    );
                Ok(())
            })?;
        }
        // Any socket loss or malformed reply leaves the same durable operation pending.
        let result = git.exchange(&request)?;
        self.journal.transaction(|s| {
            s.attempts
                .get_mut(&id)
                .unwrap()
                .source
                .as_mut()
                .unwrap()
                .operations
                .get_mut(key)
                .unwrap()
                .result = Some(result.clone());
            Ok(())
        })?;
        Ok(result)
    }
    pub fn prepare_source(&mut self, id: Uuid, git: &GitClient) -> Result<Workspace> {
        let r = self
            .journal
            .state()
            .attempts
            .get(&id)
            .context("unknown attempt")?;
        ensure!(
            matches!(
                r.phase,
                aq_worker_protocol::Phase::Authorized | aq_worker_protocol::Phase::Preparing
            ) && self.clock.now_ms() < r.grant_deadline_ms.context("no grant")?,
            "source preparation lacks live authority"
        );
        self.journal.transaction(|s| {
            s.attempts.get_mut(&id).unwrap().phase = aq_worker_protocol::Phase::Preparing;
            Ok(())
        })?;
        let result = self.source_operation(id, "create", GitAction::Create, git)?;
        let workspace: Workspace = serde_json::from_value(result)?;
        git.validate_workspace(&workspace)?;
        let bytes = workspace_bytes(&workspace.path)?;
        self.journal.transaction(|s| {
            let r = s.attempts.get_mut(&id).unwrap();
            r.source.as_mut().unwrap().workspace = Some(workspace.clone());
            r.source.as_mut().unwrap().workspace_bytes = bytes;
            r.phase = if bytes <= git.maximum_pool_bytes {
                aq_worker_protocol::Phase::Authorized
            } else {
                aq_worker_protocol::Phase::Preparing
            };
            Ok(())
        })?;
        ensure!(
            bytes <= git.maximum_pool_bytes,
            "workspace footprint exceeds configured expanded source profile; preserved without launch"
        );
        Ok(workspace)
    }
    pub fn checkpoint_source(&mut self, id: Uuid, git: &GitClient) -> Result<Checkpoint> {
        let r = self
            .journal
            .state()
            .attempts
            .get(&id)
            .context("unknown attempt")?;
        ensure!(
            r.stop_certainty == aq_worker_protocol::StopCertainty::ConfirmedStopped
                && r.stopped_proof
                    .as_ref()
                    .is_some_and(|p| p.phase == crate::guardian::GuardianPhase::Stopped
                        && p.kernel_wait_echild),
            "checkpoint requires guardian stopped-writer proof"
        );
        let source = r.source.as_ref().context("source state missing")?;
        let w = source.workspace.clone().context("workspace absent")?;
        let bytes = match workspace_bytes(&w.path) {
            Ok(bytes) => bytes,
            Err(error) => {
                self.journal.transaction(|s| {
                    s.local_drain = true;
                    Ok(())
                })?;
                return Err(error
                    .context("workspace accounting uncertain; persistent drain preserves source"));
            }
        };
        let binding = source.binding.clone();
        let prior_head = source
            .operations
            .get("checkpoint")
            .map(|o| o.request["params"]["expected_head"].clone());
        self.journal.transaction(|s| {
            s.attempts
                .get_mut(&id)
                .unwrap()
                .source
                .as_mut()
                .unwrap()
                .workspace_bytes = bytes;
            Ok(())
        })?;
        ensure!(
            bytes <= git.maximum_pool_bytes,
            "stopped workspace exceeds expanded source profile; work retained"
        );
        let inspect = git.query(
            &binding,
            "workspace.inspect",
            json!({"workspace_id":w.workspace_id}),
        )?;
        let head: ObjectId =
            serde_json::from_value(prior_head.unwrap_or_else(|| inspect["head"].clone()))?;
        let checkpoint: Checkpoint = serde_json::from_value(self.source_operation(
            id,
            "checkpoint",
            GitAction::Checkpoint {
                workspace_id: w.workspace_id,
                generation: w.generation,
                head,
            },
            git,
        )?)?;
        checkpoint.source.validate()?;
        ensure!(
            checkpoint.workspace_id == w.workspace_id && !checkpoint.retention_handle.is_nil(),
            "checkpoint response mismatch"
        );
        self.journal.transaction(|s| {
            s.attempts
                .get_mut(&id)
                .unwrap()
                .source
                .as_mut()
                .unwrap()
                .checkpoint = Some(checkpoint.clone());
            Ok(())
        })?;
        Ok(checkpoint)
    }
    pub fn export_source(&mut self, id: Uuid, git: &GitClient) -> Result<Value> {
        ensure!(
            self.journal
                .state()
                .attempts
                .get(&id)
                .is_some_and(|r| r.result.is_none()),
            "source report already acknowledged"
        );
        let c = self
            .journal
            .state()
            .attempts
            .get(&id)
            .and_then(|r| r.source.as_ref())
            .and_then(|s| s.checkpoint.clone())
            .context("checkpoint required")?;
        let result = self.source_operation(
            id,
            "export",
            GitAction::Export {
                source: c.source,
                retention_handle: c.retention_handle,
            },
            git,
        )?;
        let manifest: GitManifest = serde_json::from_value(result["manifest"].clone())?;
        let handle: Uuid = serde_json::from_value(result["artifact_handle"].clone())?;
        ensure!(
            manifest.transfer_id == handle
                && manifest.repository_id
                    == self.journal.state().attempts[&id]
                        .source
                        .as_ref()
                        .unwrap()
                        .binding
                        .repository_id,
            "export manifest binding mismatch"
        );
        let directory = git.root.join("artifacts").join(handle.to_string());
        let manifest_bytes = std::fs::read(directory.join("manifest.json"))?;
        let bytes = (manifest_bytes.len() as u64)
            .checked_add(manifest.bundle_size)
            .context("artifact bytes overflow")?;
        use sha2::{Digest, Sha256};
        let artifact = aq_worker_protocol::ArtifactRef {
            artifact_id: handle,
            digest: format!("{:x}", Sha256::digest(&manifest_bytes)),
            bytes,
            central_receipt: self.journal.state().attempts[&id]
                .source
                .as_ref()
                .and_then(|s| s.receipt.as_ref().map(|r| r.receipt_id.to_string())),
        };
        self.journal.transaction(|s| {
            let r = s.attempts.get_mut(&id).unwrap();
            r.source.as_mut().unwrap().exported = Some(result.clone());
            r.pending_bytes = bytes;
            if let Some(report) = r.report.as_mut()
                && report.artifacts != vec![artifact.clone()]
            {
                report.supersedes_report_id = Some(report.report_id);
                report.report_id = Uuid::new_v4();
                report.artifacts = vec![artifact.clone()];
            }
            Ok(())
        })?;
        self.account_staging(id, None, git)?;
        let total = self
            .journal
            .state()
            .attempts
            .values()
            .filter(|r| r.phase != aq_worker_protocol::Phase::Retired)
            .try_fold(0u64, |n, r| n.checked_add(r.pending_bytes))
            .context("pending byte accounting overflow")?;
        ensure!(
            total <= self.journal.limits().max_pending_bytes
                && bytes
                    <= self.journal.state().attempts[&id]
                        .offer
                        .spec
                        .resources
                        .disk_bytes,
            "export exceeds pending/disk bound; source retained and new admission blocked"
        );
        Ok(result)
    }
    pub fn acknowledge_source(
        &mut self,
        id: Uuid,
        receipt: aq_worker_protocol::CentralRetentionReceipt,
    ) -> Result<()> {
        receipt.validate()?;
        self.journal.transaction(|s| {
            let r = s.attempts.get_mut(&id).context("unknown attempt")?;
            ensure!(r.result.is_none(), "source report already acknowledged");
            let source = r.source.as_mut().context("source state absent")?;
            let c = source.checkpoint.as_ref().context("checkpoint absent")?;
            let manifest: GitManifest = serde_json::from_value(
                source.exported.as_ref().context("export required")?["manifest"].clone(),
            )?;
            ensure!(
                receipt.transfer_id
                    == source
                        .transfer
                        .as_ref()
                        .context("transport association required")?
                        .transfer_id
                    && receipt.retention_handle == manifest.transfer_id
                    && receipt.manifest_digest == manifest.canonical_digest()?,
                "central receipt manifest/transfer mismatch"
            );
            ensure!(
                receipt.repository_id == source.binding.repository_id
                    && receipt.binding_generation == source.binding.binding_generation
                    && receipt.source == c.source,
                "central source receipt binding mismatch"
            );
            if let Some(old) = &source.receipt {
                ensure!(old == &receipt, "changed source receipt");
            }
            if source.receipt.is_none()
                && let Some(report) = r.report.as_mut()
            {
                report.supersedes_report_id = Some(report.report_id);
                report.report_id = Uuid::new_v4();
                report.claimed_success =
                    report.outcome == aq_worker_protocol::ProcessOutcome::Success;
                for artifact in &mut report.artifacts {
                    artifact.central_receipt = Some(receipt.receipt_id.to_string());
                }
            }
            source.receipt = Some(receipt);
            Ok(())
        })
    }
    pub fn retire_source(&mut self, id: Uuid, git: &GitClient) -> Result<()> {
        let r = self
            .journal
            .state()
            .attempts
            .get(&id)
            .context("unknown attempt")?;
        if r.stop_certainty == aq_worker_protocol::StopCertainty::NotStarted
            && r.launch_id.is_none()
            && !r.start_intent
            && r.source
                .as_ref()
                .is_some_and(|s| !s.operations.contains_key("create") && s.workspace.is_none())
        {
            ensure!(
                r.result.as_ref().is_some_and(|r| r.retention_release),
                "explicit nonstart retention release required"
            );
            return self.journal.transaction(|s| {
                s.attempts
                    .get_mut(&id)
                    .unwrap()
                    .source
                    .as_mut()
                    .unwrap()
                    .retired = true;
                Ok(())
            });
        }
        ensure!(
            r.result.as_ref().is_some_and(|x| x.retention_release)
                && r.stop_certainty == aq_worker_protocol::StopCertainty::ConfirmedStopped,
            "retirement release/stopped proof required"
        );
        let source = r.source.as_ref().context("source absent")?;
        ensure!(source.receipt.is_some(), "central retention required");
        let c = source.checkpoint.clone().context("checkpoint required")?;
        self.source_operation(
            id,
            "retire",
            GitAction::Retire {
                workspace_id: c.workspace_id,
                generation: c.workspace_generation,
                head: c.source,
            },
            git,
        )?;
        self.journal.transaction(|s| {
            s.attempts
                .get_mut(&id)
                .unwrap()
                .source
                .as_mut()
                .unwrap()
                .retired = true;
            s.attempts
                .get_mut(&id)
                .unwrap()
                .source
                .as_mut()
                .unwrap()
                .workspace_bytes = 0;
            Ok(())
        })
    }
}

/// Public artifact-v1 field order is significant for aq-git's canonical digest.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitManifest {
    pub format_version: u32,
    pub repository_id: Uuid,
    pub repository_identity: String,
    pub binding_generation: u64,
    pub transfer_id: Uuid,
    pub object_format: String,
    pub base: ObjectId,
    pub source: ObjectId,
    pub prerequisites: Vec<ObjectId>,
    pub bundle_size: u64,
    pub bundle_sha256: Option<String>,
    pub producer_installation_id: Uuid,
    pub reference_only: bool,
    #[serde(default)]
    pub caller_correlation: Value,
}
impl GitManifest {
    pub fn canonical_digest(&self) -> Result<String> {
        use sha2::{Digest, Sha256};
        Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(self)?)))
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetainedRef {
    pub name: String,
    pub oid: String,
}
#[derive(Debug, Clone, Deserialize)]
struct RetainedHandle {
    id: Uuid,
    generation: u64,
    refs: Vec<RetainedRef>,
}
impl<C: crate::clock::Clock> crate::runtime::Runtime<C> {
    /// Only imports a controller-staged completed artifact in aq-git's incoming area.
    /// HTTP transfer/download is a separate consumer of this local operation.
    pub fn import_base(&mut self, id: Uuid, artifact_handle: Uuid, git: &GitClient) -> Result<()> {
        ensure!(!artifact_handle.is_nil(), "nil base artifact");
        let r = self
            .journal
            .state()
            .attempts
            .get(&id)
            .context("unknown attempt")?;
        ensure!(
            matches!(
                r.phase,
                aq_worker_protocol::Phase::Authorized | aq_worker_protocol::Phase::Preparing
            ) && r.launch_id.is_none()
                && self.clock.now_ms() < r.grant_deadline_ms.context("grant missing")?,
            "base import requires live unstarted attempt"
        );
        let binding = r
            .source
            .as_ref()
            .context("source binding required")?
            .binding
            .clone();
        if r.source
            .as_ref()
            .unwrap()
            .imported_bases
            .contains(&artifact_handle)
        {
            return Ok(());
        }
        let manifest_path = git
            .root
            .join("artifacts/incoming")
            .join(artifact_handle.to_string())
            .join("manifest.json");
        ensure!(
            std::fs::metadata(&manifest_path)?.len() <= 1024 * 1024,
            "base manifest exceeds bound"
        );
        let manifest: GitManifest = serde_json::from_slice(&std::fs::read(manifest_path)?)?;
        ensure!(
            manifest.repository_id == binding.repository_id
                && manifest.repository_identity == binding.logical_identity
                && manifest.binding_generation == binding.binding_generation
                && manifest.base == binding.base
                && manifest.source == binding.base,
            "base artifact does not match frozen binding"
        );
        self.account_staging(id, Some(artifact_handle), git)?;
        let result = self.source_operation(
            id,
            &format!("import-base-{artifact_handle}"),
            GitAction::Import { artifact_handle },
            git,
        )?;
        ensure!(
            result["retention_state"] == "active"
                && result["repository_id"] == binding.repository_id.to_string()
                && result["binding_generation"] == binding.binding_generation
                && result["source"] == serde_json::to_value(&binding.base)?
                && result["manifest_digest"] == manifest.canonical_digest()?,
            "base import lacks current matching retained source; provision a fresh transfer"
        );
        self.journal.transaction(|s| {
            s.attempts
                .get_mut(&id)
                .unwrap()
                .source
                .as_mut()
                .unwrap()
                .imported_bases
                .insert(artifact_handle);
            Ok(())
        })?;
        let incoming = git
            .root
            .join("artifacts/incoming")
            .join(artifact_handle.to_string());
        ensure!(
            !std::fs::symlink_metadata(&incoming)?
                .file_type()
                .is_symlink(),
            "incoming staging symlink refused"
        );
        std::fs::remove_dir_all(&incoming)?;
        std::fs::File::open(incoming.parent().unwrap())?.sync_all()?;
        self.account_staging(id, None, git)?;
        Ok(())
    }
    /// Explicit bounded cache reclamation; no active or unacknowledged owner is evicted.
    /// aq-git remains authoritative for unknown dependents and storage generation.
    pub fn reclaim_source(
        &mut self,
        id: Uuid,
        storage_generation: u64,
        git: &GitClient,
    ) -> Result<()> {
        let r = self
            .journal
            .state()
            .attempts
            .get(&id)
            .context("unknown attempt")?;
        let source = r.source.as_ref().context("source absent")?;
        let repo = source.binding.repository_id;
        ensure!(
            self.journal
                .state()
                .attempts
                .values()
                .filter_map(|r| r.source.as_ref().map(|s| (r, s)))
                .filter(|(_, s)| s.binding.repository_id == repo)
                .all(|(r, s)| r.phase == aq_worker_protocol::Phase::Retired
                    && s.retired
                    && s.receipt.is_some()),
            "repository has protected owners"
        );
        let mut handles = Vec::new();
        for (key, op) in &source.operations {
            if (key == "export" || key == "checkpoint" || key.starts_with("import-base-"))
                && op
                    .result
                    .as_ref()
                    .is_some_and(|r| r.get("retention_state").is_none_or(|s| s == "active"))
                && let Some(h) = op
                    .result
                    .as_ref()
                    .and_then(|r| r.get("retention_handle"))
                    .and_then(Value::as_str)
            {
                let h = Uuid::parse_str(h)?;
                if !handles.contains(&h) {
                    handles.push(h);
                }
            }
        }
        handles.sort_by_key(|h| {
            if source
                .operations
                .get("export")
                .and_then(|o| o.result.as_ref())
                .and_then(|r| r.get("retention_handle"))
                .and_then(Value::as_str)
                == Some(h.to_string().as_str())
            {
                0
            } else if source
                .checkpoint
                .as_ref()
                .is_some_and(|c| c.retention_handle == *h)
            {
                1
            } else {
                2
            }
        });
        let binding = source.binding.clone();
        for h in handles {
            let key = format!("release-{h}");
            let prior = self.journal.state().attempts[&id]
                .source
                .as_ref()
                .unwrap()
                .operations
                .get(&key);
            let action = if let Some(op) = prior {
                let p = &op.request["params"];
                GitAction::Release {
                    handle_id: h,
                    generation: p["expected_generation"]
                        .as_u64()
                        .context("release generation missing")?,
                    refs: serde_json::from_value(p["refs"].clone())?,
                }
            } else {
                let value = git.query(
                    &binding,
                    "retention.inspect",
                    json!({"handle_id":h,"limit":1}),
                )?;
                let handle: RetainedHandle = serde_json::from_value(value)?;
                ensure!(handle.id == h, "retention handle mismatch");
                GitAction::Release {
                    handle_id: h,
                    generation: handle.generation,
                    refs: handle.refs,
                }
            };
            self.source_operation(id, &key, action, git)?;
        }
        self.source_operation(id, "evict", GitAction::Evict { storage_generation }, git)?;
        // These are documented runtime transport staging directories, not private
        // repository metadata. Delete only UUID handles after aq-git releases.
        let source = self.journal.state().attempts[&id].source.as_ref().unwrap();
        let mut staged = Vec::new();
        if let Some(id) = source
            .exported
            .as_ref()
            .and_then(|r| r.get("artifact_handle"))
            .and_then(Value::as_str)
        {
            staged.push(
                git.root
                    .join("artifacts")
                    .join(Uuid::parse_str(id)?.to_string()),
            );
        }
        for key in source
            .operations
            .keys()
            .filter_map(|key| key.strip_prefix("import-base-"))
        {
            staged.push(
                git.root
                    .join("artifacts/incoming")
                    .join(Uuid::parse_str(key)?.to_string()),
            );
        }
        for directory in staged {
            if directory.exists() {
                ensure!(
                    !std::fs::symlink_metadata(&directory)?
                        .file_type()
                        .is_symlink(),
                    "staging symlink refused"
                );
                std::fs::remove_dir_all(&directory)?;
                std::fs::File::open(directory.parent().unwrap())?.sync_all()?;
            }
        }
        self.journal.transaction(|s| {
            for r in s.attempts.values_mut() {
                if let Some(source) = r.source.as_mut()
                    && source.binding.repository_id == repo
                    && source.managed_root.as_deref() == Some(git.root())
                {
                    source.cache_evicted = true;
                }
            }
            s.attempts.get_mut(&id).unwrap().pending_bytes = 0;
            Ok(())
        })?;
        Ok(())
    }
}

/// Explicit operator-selected executable pin. No service starts implicitly during polling.
pub struct GitService {
    child: std::process::Child,
    pub client: GitClient,
}
impl GitService {
    pub fn start(executable: &Path, expected_sha256: &str, root: &Path) -> Result<Self> {
        Self::start_with_git(executable, expected_sha256, root, Path::new("/usr/bin/git"))
    }
    pub fn start_with_git(
        executable: &Path,
        expected_sha256: &str,
        root: &Path,
        git_executable: &Path,
    ) -> Result<Self> {
        use sha2::{Digest, Sha256};
        ensure!(
            git_executable.is_absolute() && std::fs::metadata(git_executable)?.is_file(),
            "trusted absolute Git executable required"
        );
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        ensure!(
            executable.is_absolute() && root.is_absolute() && expected_sha256.len() == 64,
            "absolute trusted paths and binary SHA256 required"
        );
        let executable = std::fs::canonicalize(executable)?;
        ensure!(
            std::fs::metadata(&executable)?.permissions().mode() & 0o022 == 0,
            "aq-git binary writable by another identity"
        );
        ensure!(
            format!("{:x}", Sha256::digest(std::fs::read(&executable)?)) == expected_sha256,
            "aq-git executable pin mismatch"
        );
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(root)?;
        ensure!(
            !std::fs::symlink_metadata(root)?.file_type().is_symlink()
                && std::fs::metadata(root)?.permissions().mode() & 0o077 == 0,
            "aq-git root must be private"
        );
        let socket = root.join(format!("git-{}.sock", Uuid::new_v4()));
        let mut command = std::process::Command::new(executable);
        use std::os::unix::process::CommandExt;
        let runtime_pid = std::process::id() as libc::pid_t;
        unsafe {
            command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::getppid() != runtime_pid {
                    libc::raise(libc::SIGTERM);
                }
                Ok(())
            });
        }
        let mut child = command
            .arg("--state-dir")
            .arg(root)
            .arg("--listen")
            .arg(&socket)
            .arg("--git-executable")
            .arg(git_executable)
            .arg("serve")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        let ready = (|| -> Result<GitClient> {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                ensure!(
                    child.try_wait()?.is_none(),
                    "aq-git service failed startup; inspect fence/configuration"
                );
                if socket.exists() {
                    return GitClient::new(&socket, root);
                }
                ensure!(
                    std::time::Instant::now() < deadline,
                    "aq-git service readiness deadline"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
        })();
        match ready {
            Ok(client) => Ok(Self { child, client }),
            Err(e) => {
                stop_service(&mut child);
                Err(e)
            }
        }
    }
}
impl Drop for GitService {
    fn drop(&mut self) {
        stop_service(&mut self.child);
    }
}

fn stop_service(child: &mut std::process::Child) {
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Err(_) => return,
            Ok(None) => {}
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}
impl<C: crate::clock::Clock> crate::runtime::Runtime<C> {
    pub fn bind_source_transfer(
        &mut self,
        id: Uuid,
        transfer: aq_worker_protocol::TransferManifest,
    ) -> Result<()> {
        transfer.validate()?;
        self.journal.transaction(|s| {
            let r = s.attempts.get_mut(&id).context("unknown attempt")?;
            let source = r.source.as_mut().context("source absent")?;
            let manifest: GitManifest = serde_json::from_value(
                source.exported.as_ref().context("export required")?["manifest"].clone(),
            )?;
            ensure!(
                transfer.role == aq_worker_protocol::SourceRole::Checkpoint
                    && transfer.attempt == r.offer.attempt
                    && transfer.resolved_spec_digest == r.offer.spec.resolved_spec_digest
                    && transfer.source == source.binding
                    && transfer.artifact_id == manifest.transfer_id
                    && transfer.checkpoint == manifest.source
                    && transfer.bundle_bytes == manifest.bundle_size
                    && transfer.bundle_digest == manifest.bundle_sha256,
                "transport manifest association mismatch"
            );
            let path = source
                .managed_root
                .as_ref()
                .context("source root absent")?
                .join("artifacts")
                .join(manifest.transfer_id.to_string())
                .join("manifest.json");
            let bytes = std::fs::read(path)?;
            use sha2::{Digest, Sha256};
            ensure!(
                transfer.manifest_bytes == bytes.len() as u64
                    && transfer.manifest_digest == format!("{:x}", Sha256::digest(bytes)),
                "transport exact manifest bytes mismatch"
            );
            if let Some(old) = &source.transfer {
                ensure!(old == &transfer, "changed source transport identity");
            }
            source.transfer = Some(transfer);
            Ok(())
        })
    }
}
fn workspace_bytes(root: &Path) -> Result<u64> {
    let mut pending = vec![(root.to_path_buf(), 0usize)];
    let mut entries = 0usize;
    let mut bytes = 0u64;
    while let Some((directory, depth)) = pending.pop() {
        ensure!(depth < 64, "workspace accounting depth bound");
        for entry in std::fs::read_dir(directory)? {
            entries += 1;
            ensure!(entries <= 100000, "workspace accounting entry bound");
            let entry = entry?;
            let meta = std::fs::symlink_metadata(entry.path())?;
            bytes = bytes
                .checked_add(meta.len().max(4096))
                .context("workspace byte accounting overflow")?;
            if meta.is_dir() {
                pending.push((entry.path(), depth + 1));
            }
        }
    }
    Ok(bytes)
}
impl GitClient {
    fn staged_bytes(&self, source: &SourceState, extra: Option<Uuid>) -> Result<u64> {
        let mut handles = std::collections::BTreeSet::new();
        if let Some(id) = extra {
            handles.insert(id);
        }
        for key in source
            .operations
            .keys()
            .filter_map(|k| k.strip_prefix("import-base-"))
        {
            handles.insert(Uuid::parse_str(key)?);
        }
        let mut total = 0u64;
        for handle in handles {
            for file in ["manifest.json", "objects.bundle"] {
                let path = self
                    .root
                    .join("artifacts/incoming")
                    .join(handle.to_string())
                    .join(file);
                match std::fs::symlink_metadata(path) {
                    Ok(meta) => {
                        ensure!(
                            meta.file_type().is_file(),
                            "incoming artifact file must be regular"
                        );
                        total = total
                            .checked_add(meta.len())
                            .context("incoming byte accounting overflow")?;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e.into()),
                }
            }
        }
        Ok(total)
    }
}
impl<C: crate::clock::Clock> crate::runtime::Runtime<C> {
    fn account_staging(&mut self, id: Uuid, extra: Option<Uuid>, git: &GitClient) -> Result<()> {
        let source = self
            .journal
            .state()
            .attempts
            .get(&id)
            .and_then(|r| r.source.as_ref())
            .context("source absent")?;
        let incoming = git.staged_bytes(source, extra)?;
        let outgoing = if let Some(exported) = &source.exported {
            let handle: Uuid = serde_json::from_value(exported["artifact_handle"].clone())?;
            std::fs::metadata(
                git.root
                    .join("artifacts")
                    .join(handle.to_string())
                    .join("manifest.json"),
            )?
            .len()
            .checked_add(
                exported["manifest"]["bundle_size"]
                    .as_u64()
                    .context("bundle size absent")?,
            )
            .context("outgoing byte accounting overflow")?
        } else {
            git.maximum_artifact_bytes
        };
        let bytes = incoming
            .checked_add(outgoing)
            .context("source staging byte accounting overflow")?;
        self.journal.transaction(|s| {
            s.attempts.get_mut(&id).unwrap().pending_bytes = bytes;
            Ok(())
        })?;
        let total = self
            .journal
            .state()
            .attempts
            .values()
            .filter(|r| r.phase != aq_worker_protocol::Phase::Retired)
            .try_fold(0u64, |n, r| n.checked_add(r.pending_bytes))
            .context("pending byte accounting overflow")?;
        ensure!(
            total <= self.journal.limits().max_pending_bytes,
            "protected source staging exceeds pending bound; admission held"
        );
        Ok(())
    }
}
