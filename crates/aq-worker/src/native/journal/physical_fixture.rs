//! Shared actual-byte/source/material/Git-peer test fixture for exact typed profiles.
macro_rules! physical_fixture {($phase:path,$terminal:path,$http:ident,$acceptance:ident,$preparation:ident,$propose:ident,$begin:ident,$accept:ident,$record:ident)=> {
// Synthetic authenticated coordinator and finite public aq-git socket peer.
// Real Git bundle qualification is separately explicit/ignored by default.
use super::*;
use $phase as phase;
use $terminal as terminal;
use crate::{
    native::{
        http::{NativeClient, control::Preparation},
        materials::MaterialStore,
        preparation::InputPipeline,
    },
    source::{GitClient, GitManifest},
};
use aq_admission::{
    ArtifactKind,
    control::{ControlReply, PreparationDelivery},
    execution_spec::ProfiledExecutionSpec,
    transport::{ArtifactFetch, DeliveryClock, MaterialDelivery, MaterialFetch},
};
use aq_worker_protocol::{
    AttemptRef, ObjectAlgorithm, ObjectId, SourceBinding, SourceRole, TransferManifest,
};
use axum::{
    Json, Router,
    extract::{Path as HttpPath, Query},
    http::HeaderMap,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixListener,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering},
    },
};
struct Export {
    manifest: TransferManifest,
    manifest_bytes: Vec<u8>,
    bundle: Vec<u8>,
}
fn synthetic() -> Export {
    let id = Uuid::new_v4();
    let source = SourceBinding {
        repository_id: Uuid::from_u128(41),
        binding_generation: 1,
        logical_identity: "synthetic".into(),
        base: ObjectId {
            algorithm: ObjectAlgorithm::Sha1,
            hex: "a".repeat(40),
        },
    };
    let bundle = vec![b'x'; 300000];
    let bd = format!("{:x}", Sha256::digest(&bundle));
    let native = GitManifest {
        format_version: 1,
        repository_id: source.repository_id,
        repository_identity: source.logical_identity.clone(),
        binding_generation: 1,
        transfer_id: id,
        object_format: "sha1".into(),
        base: source.base.clone(),
        source: source.base.clone(),
        prerequisites: vec![],
        bundle_size: bundle.len() as u64,
        bundle_sha256: Some(bd.clone()),
        producer_installation_id: Uuid::new_v4(),
        reference_only: false,
        caller_correlation: json!({}),
    };
    let bytes = serde_json::to_vec(&native).unwrap();
    let manifest = TransferManifest {
        transfer_id: id,
        artifact_id: id,
        role: SourceRole::Base,
        attempt: AttemptRef {
            attempt_id: Uuid::from_u128(1),
            session_id: Uuid::from_u128(1),
            generation: 1,
        },
        source: source.clone(),
        checkpoint: source.base,
        resolved_spec_digest: "a".repeat(64),
        manifest_bytes: bytes.len() as u64,
        manifest_digest: format!("{:x}", Sha256::digest(&bytes)),
        bundle_bytes: bundle.len() as u64,
        bundle_digest: Some(bd),
    };
    Export {
        manifest,
        manifest_bytes: bytes,
        bundle,
    }
}
struct Inputs {
    pipeline: Option<Arc<InputPipeline>>,
    result_pins: Option<crate::native::result::ResultInstallationPins>,
    preparation: Option<Arc<Preparation>>,
    phase: Option<Arc<crate::native::http::$http::AcceptedPreparation>>,
    phase_window: Arc<AtomicU64>,
    proposed: Option<crate::native::http::$http::ProposedPreparation>,
    client: NativeClient,
    reference: ArtifactRef,
    id: Uuid,
    root: PathBuf,
    _dir: tempfile::TempDir,
    task: tokio::task::JoinHandle<()>,
    mode: Arc<AtomicU8>,
    range_entered: Arc<AtomicBool>,
    inline_entered: Arc<tokio::sync::Semaphore>,
    inline_release: Arc<tokio::sync::Semaphore>,
    inline_max_active: Arc<AtomicUsize>,
    grant_mode: Arc<AtomicU8>,
    grant_window_ms: Arc<AtomicU64>,
    scoped_calls: Arc<Mutex<Vec<(String, Value)>>>,
    scoped_mode: Arc<AtomicU8>,
    host_report_mode: Arc<AtomicU8>,
    host_report_calls: Arc<Mutex<Vec<aq_admission::started::Request>>>,
    phase_terminal_calls: Arc<Mutex<Vec<(String, Value)>>>,
    scoped_entered: Arc<tokio::sync::Semaphore>,
    scoped_release: Arc<tokio::sync::Semaphore>,
}
struct ActiveInline(Arc<AtomicUsize>);
impl Drop for ActiveInline {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
impl Drop for Inputs {
    fn drop(&mut self) {
        self.task.abort()
    }
}
fn phase_execution_delivery(
    a: &Attempt,
    q: &phase::PrepareRequest,
    nonce: Uuid,
    window: u64,
) -> phase::ExecutionDelivery {
    use phase::{ExecutionDelivery, FactValue, OwnerFact};
    let fixture: Value = published_phase_fixture();
    let mut d: ExecutionDelivery = serde_json::from_value(fixture["delivery"].clone()).unwrap();
    let intent = a.$acceptance.as_ref().unwrap();
    let p = &intent.proposal;
    let accepted = &a.$preparation.as_ref().unwrap().delivery.receipt;
    let c = &p.choice;
    let r = &mut d.receipt;
    r.operation_id = q.original.operation_id;
    r.request_digest = q.digest().unwrap();
    r.accepted_receipt_digest = accepted.digest().unwrap();
    r.negotiation = q.negotiation.clone();
    r.issued_at_ms = 2000;
    r.grant.binding = q.original.binding.clone();
    r.grant.expires_at_ms = 2000 + window;
    r.execution_deadline_ms = 22000;
    r.versions.record_revision = accepted.versions.record_revision + 1;
    r.versions.authority_revision = accepted.versions.authority_revision + 1;
    r.versions.grant_revision = 1;
    r.confirmation.prepare_digest = q.digest().unwrap();
    r.confirmation.accepted_receipt_digest = accepted.digest().unwrap();
    r.confirmation.checked_at_ms = 2000;
    for f in &mut r.confirmation.facts {
        f.body.binding = q.original.binding.clone();
        f.body.choice_digest = accepted.lease.choice_digest.clone();
        f.body.observed_at_ms = 2000;
        f.body.expires_at_ms = 2000 + p.policy.maximum_observation_ms;
        f.body.authority = p
            .owners
            .iter()
            .find(|o| o.kind == f.body.value.kind())
            .unwrap()
            .authority
            .clone();
        refresh_registry_fact(f,c,accepted);
        match &mut f.body.value {
            FactValue::Project {
                project_id,
                revision,
                catalog,
                permissions_digest,
            } => {
                *project_id = p.spec.lifecycle.project_id;
                *revision = p.spec.project_revision;
                *catalog = p.original.offered.offer.manifest.catalog.clone();
                *permissions_digest = c.permission_digest.clone();
            }
            FactValue::Registry { .. } => {}
            FactValue::Principal { principal, scope } => {
                *principal = c.principal.clone();
                *scope = p.spec.graph_access_profile.clone();
            }
            FactValue::Inventory {
                node_id,
                boot_id,
                model,
                harness,
                capabilities,
                credential_profile,
                credential_revision,
                ..
            } => {
                *node_id = accepted.lease.identity.node_id;
                *boot_id = accepted.lease.identity.boot_id;
                *model = c.model.clone();
                *harness = c.harness.clone();
                *capabilities = p.original.offered.offer.required_capabilities.clone();
                *credential_profile = c.credential_profile.clone();
                *credential_revision = c.credential_revision;
            }
            FactValue::Availability {
                provider,
                account_reference,
                endpoint_reference,
                ..
            } => {
                *provider = c.model.provider.clone();
                *account_reference = c.account_reference.clone();
                *endpoint_reference = c.endpoint_reference.clone();
            }
            FactValue::Budget {
                lineage_id,
                maximum_lineage_elapsed_ms,
                ..
            } => {
                *lineage_id = p.spec.lifecycle.lineage_id;
                *maximum_lineage_elapsed_ms = c.maximum_lineage_elapsed_ms;
            }
            FactValue::Context {
                task,
                context,
                full_context_digest,
                graph_policy,
            } => {
                *task = p.spec.task.clone();
                *context = p.spec.task_context.clone();
                *full_context_digest = p.full_context_digest.clone();
                *graph_policy = p.spec.graph_access_profile.clone();
            }
            FactValue::Git {
                repository_id,
                binding_generation,
                base,
                policy,
                manifest,
                bundle,
                ..
            } => {
                *repository_id = p.spec.source.repository_id;
                *binding_generation = p.spec.source.repository_binding_generation;
                *base = p.spec.source.resolved_object.clone();
                *policy = p.spec.source.policy.clone();
                *manifest = q
                    .original
                    .artifacts
                    .iter()
                    .find(|x| x.artifact.id.as_str().starts_with("aq.source-manifest/"))
                    .unwrap()
                    .clone();
                *bundle = q
                    .original
                    .artifacts
                    .iter()
                    .find(|x| x.artifact.id.as_str().starts_with("aq.source-bundle/"))
                    .unwrap()
                    .clone();
            }
            FactValue::PrivateMaterialBacking {
                lease_id,
                backing_digest,
                installation_receipt_digest,
            } => {
                *lease_id = accepted.lease.materials[0].lease_id;
                *backing_digest = accepted.lease.materials[0].material_digest.clone();
                *installation_receipt_digest =
                    Digest::of(&canonical_bytes(&q.original.materials[0]).unwrap());
            }
        }
        f.record.digest = OwnerFact::record_digest(&f.body).unwrap();
        f.record.version = aq_session_protocol::Text::new(f.body.revision.to_string()).unwrap();
    }
    d.delivery.nonce = nonce;
    d.delivery.clock = DeliveryClock::new(2000, 2000 + window).unwrap();
    d.validate_for(q, accepted, &intent.request, p, nonce)
        .unwrap();
    d
}
fn configure_result_fixture(offered:&mut NativeOffer)->(crate::native::result::ResultInstallationPins,BTreeMap<String,Vec<u8>>){
    use crate::native::adapter::result_profile::{ResultAdapterDefinition,ResultAdapterKind};
    use crate::native::result::ResultInstallationPins;
    use aq_session_protocol::{ContextDelivery,Text};
    let mut bodies=BTreeMap::new();
    let old_outcome=offered.offered.offer.manifest.bindings.outcome.clone();
    let outcome:aq_admission::outcome::ExpectedOutcomeDefinition=serde_json::from_slice(include_bytes!("../../../../../vendor/aq-admission-0.1.10/artifacts/outcome.fixture.json")).unwrap();
    let expected=outcome.artifact().unwrap();let outcome_bytes=outcome.canonical_bytes().unwrap();
    offered.offered.offer.manifest.bindings.outcome=expected.clone();
    let old_payload=offered.startup.payload.clone();let old_descriptor=offered.startup.artifact().unwrap();
    let instruction=aq_agent_result_local::startup_artifact();let text=aq_agent_result_local::STARTUP_TEXT.as_bytes();
    let mut payload=b"instructions\n\ntask-context\n\n".to_vec();payload.extend_from_slice(text);
    let m=&mut offered.offered.offer.manifest;
    let mut context=m.context.references[0].clone();context.artifact=instruction.clone();context.bytes=text.len()as u64;context.tokens=text.len()as u64;context.required=true;context.delivery=ContextDelivery::Startup;
    m.context.references.push(context);m.modules.push(instruction.clone());
    offered.startup.payload.digest=Digest::of(&payload);offered.startup.payload_bytes=payload.len()as u64;offered.startup.payload_tokens_upper_bound=payload.len()as u64;
    offered.startup.startup_inputs.push(aq_admission::startup::InputSpan{artifact:instruction.clone(),offset:28,bytes:text.len()as u64});
    let reference=|id:&str|ArtifactRef{id:Text::new(id).unwrap(),version:Text::new("1").unwrap(),digest:Digest::of(id.as_bytes())};
    let definition=ResultAdapterDefinition{schema_version:1,profile:ResultAdapterKind::Codex0166TrustedNodeNamedReadonlyLocalV1,runtime:qualification().runtime,
        environment:m.harness.environment.clone(),client_manifest:reference("aq.agent-result-client-installation"),client_executable:reference("synthetic-client"),
        contract:aq_agent_result_local::artifact(),recipe:aq_codex_mapper::result_local_recipe_artifact(),permission_digest:canonical_digest(&m.permissions).unwrap(),reserved_client_bytes:64*1024*1024};
    let adapter=definition.artifact().unwrap();m.harness.adapter=adapter.clone();
    let new_descriptor=offered.startup.artifact().unwrap();
    for module in &mut offered.offered.offer.manifest.modules {if *module==old_descriptor{*module=new_descriptor.clone();}}
    let descriptor_bytes=canonical_bytes(&offered.startup).unwrap();
    for d in &mut offered.offered.artifacts {
        if d.artifact==old_outcome {d.artifact=expected.clone();d.bytes=outcome_bytes.len()as u64;}
        if d.artifact==old_payload {d.artifact=offered.startup.payload.clone();d.bytes=payload.len()as u64;}
        if d.artifact==old_descriptor {d.artifact=new_descriptor.clone();d.bytes=descriptor_bytes.len()as u64;}
    }
    let template=offered.offered.artifacts[0].clone();
    for (kind,artifact,bytes) in [(ArtifactKind::Context,instruction,text.to_vec()),(ArtifactKind::Definition,adapter.clone(),definition.encode().unwrap())]{
        let mut d=template.clone();d.kind=kind;d.artifact=artifact.clone();d.bytes=bytes.len()as u64;offered.offered.artifacts.push(d);bodies.insert(artifact.digest.as_str().to_string(),bytes);
    }
    bodies.insert(expected.digest.as_str().into(),outcome_bytes);bodies.insert(offered.startup.payload.digest.as_str().into(),payload);bodies.insert(new_descriptor.digest.as_str().into(),descriptor_bytes);
    offered.offered.offer.binding.launch_manifest_digest=canonical_digest(&offered.offered.offer.manifest).unwrap();
    let pins=ResultInstallationPins{adapter,contract:definition.contract,client_manifest:definition.client_manifest,client_executable:definition.client_executable,environment:definition.environment,recipe:definition.recipe,permission_digest:definition.permission_digest,reserved_client_bytes:definition.reserved_client_bytes};
    (pins,bodies)
}
impl Inputs {
    fn pipeline(&self) -> &Arc<InputPipeline> {
        self.pipeline.as_ref().unwrap()
    }
    fn preparation(&self) -> &Arc<Preparation> {
        self.preparation.as_ref().unwrap()
    }
    async fn new(export: Export) -> Self {
        Self::new_mode(export, false).await
    }
    async fn new_phase(export: Export) -> Self {
        Self::new_mode(export, true).await
    }
    async fn new_mode(export: Export, phase_mode: bool) -> Self {
        Self::new_count(export, phase_mode, 0).await
    }
    #[cfg(feature = "test-support")]
    async fn new_phase_output(export: Export) -> Self {
        Box::pin(Self::new_count_policy(export, true, 0, true, false)).await
    }
    #[cfg(feature = "test-support")]
    async fn new_phase_codex(export: Export) -> Self {
        Box::pin(Self::new_count_policy(export, true, 0, true, true)).await
    }
    async fn new_count(export: Export, phase_mode: bool, count: usize) -> Self {
        Box::pin(Self::new_count_policy(
            export, phase_mode, count, false, false,
        ))
        .await
    }
    async fn new_collect(export:Export)->Self {
        Box::pin(Self::new_count_policy_impl(export,true,0,false,false,true,false)).await
    }
    async fn new_result(export:Export)->Self { Box::pin(Self::new_count_policy_impl(export,true,0,true,true,true,true)).await }
    async fn new_count_policy(
        export: Export,
        phase_mode: bool,
        count: usize,
        output_only: bool,
        codex: bool,
    ) -> Self {
        Box::pin(Self::new_count_policy_impl(export,phase_mode,count,output_only,codex,false,false)).await
    }
    async fn new_count_policy_impl(
        mut export: Export,
        phase_mode: bool,
        count: usize,
        output_only: bool,
        codex: bool,
        defer_phase_accept: bool,
        result:bool,
    ) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("journal");
        let materials = dir.path().join("materials");
        fs::create_dir(&materials).unwrap();
        fs::set_permissions(&materials, fs::Permissions::from_mode(0o700)).unwrap();
        let mut offered = offer();
        if codex {
            let m = &mut offered.offered.offer.manifest;
            m.harness.harness = aq_session_protocol::Text::new("codex").unwrap();
            m.harness.cli_version = aq_session_protocol::Text::new("0.166.0").unwrap();
            m.model.provider = aq_session_protocol::Text::new("openai").unwrap();
            m.model.model = aq_session_protocol::Text::new("gpt-5.1-codex-mini").unwrap();
            m.model.reasoning = aq_session_protocol::Text::new("medium").unwrap();
            m.model.spending_budget = None;
        }
        if output_only {
            offered.offered.offer.manifest.terminal.input_enabled = false;
            offered.offered.offer.binding.launch_manifest_digest =
                aq_session_protocol::canonical_digest(&offered.offered.offer.manifest).unwrap();
        }
        offered.offered.reservation_ttl_ms = 30000;
        let (result_pins,result_bodies)=if result {let (p,b)=configure_result_fixture(&mut offered);(Some(p),b)}else{(None,BTreeMap::new())};
        let mut spec: ProfiledExecutionSpec = serde_json::from_value(
            serde_json::from_slice::<Value>(include_bytes!(
                "../../../../../vendor/aq-admission-0.1.7/artifacts/execution-spec.fixture.json"
            ))
            .unwrap()["spec"]
                .clone(),
        )
        .unwrap();
        spec.outcome_artifact=offered.offered.offer.manifest.bindings.outcome.clone();
        spec.source.repository_id = export.manifest.source.repository_id;
        spec.source.resolved_object.hex = export.manifest.source.base.hex.clone();
        let bytes = spec.canonical_bytes().unwrap();
        let reference = spec.artifact().unwrap();
        offered.offered.offer.binding.execution_spec_digest = reference.digest.clone();
        let mut descriptor = offered.offered.artifacts[0].clone();
        descriptor.kind = ArtifactKind::Composition;
        descriptor.artifact = reference.clone();
        descriptor.bytes = bytes.len() as u64;
        offered.offered.artifacts.push(descriptor.clone());
        let mut observation = offered
            .offered
            .artifacts
            .iter()
            .find(|d| d.artifact == spec.task_context)
            .unwrap()
            .clone();
        observation.artifact = spec.task_observation.clone();
        observation.bytes = observation.artifact.id.as_str().len() as u64;
        offered.offered.artifacts.push(observation);
        while offered.offered.artifacts.len() < count {
            let label = format!("phase-extra-{}", offered.offered.artifacts.len());
            let mut d = offered.offered.artifacts[0].clone();
            d.kind = ArtifactKind::Context;
            d.artifact = ArtifactRef {
                id: aq_session_protocol::Text::new(&label).unwrap(),
                version: aq_session_protocol::Text::new("1").unwrap(),
                digest: Digest::of(label.as_bytes()),
            };
            d.bytes = label.len() as u64;
            offered.offered.artifacts.push(d);
        }
        if phase_mode {
            for d in &mut offered.offered.artifacts {
                d.retain_until_ms = 30000;
            }
        }
        let inline: Vec<_> = offered
            .offered
            .artifacts
            .iter()
            .map(|d| {
                let body = if let Some(body)=result_bodies.get(d.artifact.digest.as_str()) {body.clone()}else if d.artifact == reference {
                    bytes.clone()
                } else if d.artifact == offered.startup.artifact().unwrap() {
                    canonical_bytes(&offered.startup).unwrap()
                } else if d.artifact == offered.startup.payload {
                    b"instructions\n\ntask-context".to_vec()
                } else {
                    d.artifact.id.as_str().as_bytes().to_vec()
                };
                assert_eq!(Digest::of(&body), d.artifact.digest);
                assert_eq!(body.len() as u64, d.bytes);
                (d.clone(), body)
            })
            .collect();
        offered.offered.offer.validate().unwrap();
        offered.validate().unwrap();
        let mut capacity = limits();
        capacity.disk_bytes = 64 * 1024 * 1024 * 1024;
        let mut journal = Journal::create_identity(&root, identity(), capacity).unwrap();
        let id = journal
            .reserve(
                offered.clone(),
                qualification(),
                boot_ns().unwrap()
                    + if phase_mode {
                        1_000_000_000
                    } else {
                        29_000_000_000
                    },
                32 * 1024 * 1024 * 1024,
            )
            .unwrap();
        let accept = if phase_mode {
            None
        } else {
            Some(journal.begin_accept(id, capabilities()).unwrap())
        };
        let cap = journal.state().attempts[&id].reservation.deadline_ns;
        export.manifest.resolved_spec_digest = reference.digest.as_str().into();
        let mut delivery: PreparationDelivery =
            serde_json::from_value(fixture()["replies"][1]["body"].clone()).unwrap();
        delivery.original.clock = DeliveryClock::new(1500, 29000).unwrap();
        delivery.delivery.clock = DeliveryClock::new(1600, 29000).unwrap();
        delivery.original.binding = offered.offered.offer.binding.clone();
        delivery.original.materials[0].binding = offered.offered.offer.binding.clone();
        delivery.original.materials[0].expires_at_ms = 30000;
        if codex {
            delivery.original.materials[0].provider = aq_admission::Label::new("openai").unwrap();
            delivery.original.materials[0].material_bytes = b"synthetic-upstream-key".len() as u64;
            delivery.original.materials[0].material_digest = Digest::of(b"synthetic-upstream-key");
        }
        let material_lease = delivery.original.materials[0].clone();
        let phase_fixture: Value = published_phase_fixture();
        let mut phase_proposal: phase::Proposal =
            serde_json::from_value(phase_fixture["proposal"].clone()).unwrap();
        phase_proposal.original = offered.clone();
        phase_proposal.spec = spec.clone();
        phase_proposal.choice.selection_digest = spec.selection_digest.clone();
        phase_proposal.choice.model = offered.offered.offer.manifest.model.clone();
        phase_proposal.choice.harness = offered.offered.offer.manifest.harness.clone();
        phase_proposal.choice.permission_digest =
            Digest::of(&canonical_bytes(&offered.offered.offer.manifest.permissions).unwrap());
        phase_proposal.choice.credential_profile = material_lease.credential_profile.clone();
        phase_proposal.choice.credential_revision = material_lease.credential_revision;
        phase_proposal.choice.account_reference =
            aq_session_protocol::Text::new(material_lease.account_reference.as_str()).unwrap();
        phase_proposal.choice.endpoint_reference =
            aq_session_protocol::Text::new(material_lease.endpoint_reference.as_str()).unwrap();
        phase_proposal.validate().unwrap();
        let proposed_route = phase_proposal.clone();
        let accept_proposal = phase_proposal.clone();
        let phase_material = material_lease.clone();
        let proposal_route = move || {
            let p = proposed_route.clone();
            async move {
                Json(phase::Document::Proposal(Box::new(
                    p,
                )))
            }
        };
        let phase_window = Arc::new(AtomicU64::new(27300));
        let window = phase_window.clone();
        let phase_accept_route = move |headers: HeaderMap,
                                       Json(document): Json<
            phase::Document,
        >| {
            let p = accept_proposal.clone();
            let m = phase_material.clone();
            let f = phase_fixture.clone();
            let window = window.clone();
            async move {
                use phase::{AcceptedDelivery, AcceptedReceipt, Document};
                let Document::Accept(q) = document else {
                    panic!("phase Accept")
                };
                q.validate_for(&p).unwrap();
                let mut r: AcceptedReceipt = serde_json::from_value(f["accepted"].clone()).unwrap();
                r.operation_id = q.original.operation_id;
                r.request_digest = q.digest().unwrap();
                r.negotiation = q.negotiation.clone();
                r.lease.identity = q.original.identity.clone();
                r.lease.binding = q.original.acceptance.binding.clone();
                r.lease.guardian_id = q.original.guardian_id;
                r.lease.launch_id = q.original.launch_id;
                r.lease.hold = q.hold.clone();
                r.lease.proposal_digest = q.proposal_digest.clone();
                r.lease.policy_digest = p.policy.digest().unwrap();
                r.lease.choice_digest = p.choice.digest().unwrap();
                r.lease.issued_at_ms = 1500;
                r.lease.expires_at_ms = p
                    .original
                    .offered
                    .artifacts
                    .iter()
                    .map(|d| d.retain_until_ms)
                    .min()
                    .unwrap()
                    .min(29000);
                r.lease.materials = vec![m];
                let nonce =
                    Uuid::parse_str(headers["aq-delivery-nonce"].to_str().unwrap()).unwrap();
                let r_expiry = r.lease.expires_at_ms;
                let a = AcceptedDelivery {
                    schema_version: 2,
                    receipt: r,
                    delivery: aq_admission::control::DeliveryStamp {
                        nonce,
                        clock: DeliveryClock::new(
                            r_expiry - window.load(Ordering::SeqCst),
                            r_expiry,
                        )
                        .unwrap(),
                    },
                };
                a.validate_for(&q, &p, nonce).unwrap();
                Json(Document::AcceptedDelivery(Box::new(a)))
            }
        };

        let mode = Arc::new(AtomicU8::new(0));
        let r1 = root.clone();
        let r2 = root.clone();
        let presence_root = root.clone();
        let m = export.manifest.clone();
        let status = m.clone();
        let range_mode = mode.clone();
        let range_entered = Arc::new(AtomicBool::new(false));
        let entered = range_entered.clone();
        let grant_plan = delivery.original.clone();
        let grant_mode = Arc::new(AtomicU8::new(0));
        let grant_window_ms = Arc::new(AtomicU64::new(1_000));
        let grant_case = grant_mode.clone();
        let grant_window = grant_window_ms.clone();
        let grant_id = Uuid::new_v4();
        let prepare_route =
            move |headers: HeaderMap, Json(request): Json<aq_admission::transport::Prepare>| {
                let plan = grant_plan.clone();
                let mode = grant_case.clone();
                let window = grant_window.clone();
                async move {
                    let mut grant: aq_admission::control::InitialGrantDelivery =
                        serde_json::from_value(fixture()["replies"][2]["body"].clone()).unwrap();
                    grant.operation_id = request.operation_id;
                    grant.original.grant.binding = request.binding.clone();
                    grant.original.grant.grant_id = if mode.load(Ordering::SeqCst) == 1 {
                        Uuid::new_v4()
                    } else {
                        grant_id
                    };
                    grant.original.guardian_id = request.guardian_id;
                    grant.original.launch_id = request.launch_id;
                    grant.original.reservation_id = request.reservation.reservation_id;
                    grant.original.materials = plan.materials;
                    grant.original.clock =
                        DeliveryClock::new(2_000, 2_000 + window.load(Ordering::SeqCst)).unwrap();
                    grant.original.grant.expires_at_ms = grant.original.clock.expires_at_ms;
                    grant.execution_deadline_ms = 22_000;
                    grant.delivery.clock = grant.original.clock.clone();
                    grant.delivery.nonce =
                        Uuid::parse_str(headers["aq-delivery-nonce"].to_str().unwrap()).unwrap();
                    Json(ControlReply::InitialGrant(grant))
                }
            };
        let accept_route = move |headers: HeaderMap, Json(request): Json<Accept>| {
            let mut d = delivery.clone();
            async move {
                assert_eq!(headers["authorization"], "Bearer synthetic-node-secret");
                d.delivery.nonce =
                    Uuid::parse_str(headers["aq-delivery-nonce"].to_str().unwrap()).unwrap();
                d.original.operation_id = request.operation_id;
                d.original.reservation = request.reservation;
                d.original.guardian_id = request.guardian_id;
                d.original.launch_id = request.launch_id;
                Json(ControlReply::Preparation(d))
            }
        };
        let artifact_mode = mode.clone();
        let inline_entered = Arc::new(tokio::sync::Semaphore::new(0));
        let inline_release = Arc::new(tokio::sync::Semaphore::new(0));
        let inline_max_active = Arc::new(AtomicUsize::new(0));
        let active = Arc::new(AtomicUsize::new(0));
        let inline_signals = (
            inline_entered.clone(),
            inline_release.clone(),
            inline_max_active.clone(),
        );
        let artifact_route = move |Json(request): Json<ArtifactFetch>| {
            let (descriptor, bytes) = inline
                .iter()
                .find(|(d, _)| d.artifact == request.artifact)
                .unwrap()
                .clone();
            let root = r1.clone();
            let artifact_mode = artifact_mode.clone();
            let active = active.clone();
            let (entered, release, maximum) = inline_signals.clone();
            async move {
                let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                let _active = ActiveInline(active);
                maximum.fetch_max(current, Ordering::SeqCst);
                let snapshot: Snapshot =
                    serde_json::from_slice(&fs::read(root.join("journal.json")).unwrap()).unwrap();
                assert!(
                    snapshot.state.attempts[&request.binding.attempt_id]
                        .downloads
                        .values()
                        .any(|d| d.requests.iter().any(|r| same(r, &request).unwrap()))
                );
                if artifact_mode.load(Ordering::SeqCst) == 4 {
                    entered.add_permits(1);
                    release.acquire().await.unwrap().forget();
                }
                if artifact_mode.load(Ordering::SeqCst) == 3
                    && request.artifact.id.as_str() == "instructions"
                {
                    return (
                        axum::http::StatusCode::SERVICE_UNAVAILABLE,
                        "synthetic partial inline batch",
                    )
                        .into_response();
                }
                let end =
                    (request.offset + u64::from(request.length)).min(bytes.len() as u64) as usize;
                let mut response = bytes[request.offset as usize..end].to_vec().into_response();
                artifact_headers(&mut response, &descriptor, request.offset);
                response
            }
        };
        let status_route = move |HttpPath(id): HttpPath<Uuid>, headers: HeaderMap| {
            let m = status.clone();
            let root = r2.clone();
            async move {
                assert_eq!(headers["authorization"], "Bearer synthetic-node-secret");
                let snapshot: Snapshot =
                    serde_json::from_slice(&fs::read(root.join("journal.json")).unwrap()).unwrap();
                assert!(snapshot.state.attempts[&id].source.is_some());
                let mut r = Json(m).into_response();
                source_headers(&mut r);
                r
            }
        };
        let range_route =
            move |HttpPath((id, member)): HttpPath<(Uuid, String)>,
                  Query(query): Query<std::collections::HashMap<String, u64>>| {
                let m = m.clone();
                let bytes = if member == "manifest" {
                    export.manifest_bytes.clone()
                } else {
                    export.bundle.clone()
                };
                let root = root.clone();
                let mode = range_mode.clone();
                async move {
                    let offset = query["offset"];
                    let limit = query["limit"];
                    let snapshot: Snapshot =
                        serde_json::from_slice(&fs::read(root.join("journal.json")).unwrap())
                            .unwrap();
                    assert_eq!(
                        snapshot.state.attempts[&id]
                            .source
                            .as_ref()
                            .unwrap()
                            .pending
                            .as_ref()
                            .unwrap()
                            .offset,
                        offset
                    );
                    entered.store(true, Ordering::SeqCst);
                    if mode.load(Ordering::SeqCst) == 1 {
                        return (
                            axum::http::StatusCode::SERVICE_UNAVAILABLE,
                            "synthetic lost source read",
                        )
                            .into_response();
                    }
                    if mode.load(Ordering::SeqCst) == 2 {
                        tokio::time::sleep(std::time::Duration::from_millis(800)).await;
                    }
                    let mut r = bytes
                        [offset as usize..(offset + limit).min(bytes.len() as u64) as usize]
                        .to_vec()
                        .into_response();
                    source_headers(&mut r);
                    r.headers_mut()
                        .insert("content-type", "application/octet-stream".parse().unwrap());
                    r.headers_mut().insert(
                        "aq-source-transfer",
                        URL_SAFE_NO_PAD
                            .encode(serde_json::to_vec(&m).unwrap())
                            .parse()
                            .unwrap(),
                    );
                    r.headers_mut()
                        .insert("aq-source-offset", offset.to_string().parse().unwrap());
                    r
                }
            };
        let material_route = move |Json(request): Json<MaterialFetch>| {
            let lease = material_lease.clone();
            async move {
                assert_eq!(request.lease_id, lease.lease_id);
                let delivery = MaterialDelivery {
                    lease,
                    clock: DeliveryClock::new(1700, 30000).unwrap(),
                };
                let bytes = if codex {
                    b"synthetic-upstream-key".as_slice()
                } else {
                    b"synthetic-key".as_slice()
                };
                let mut response = bytes.to_vec().into_response();
                response
                    .headers_mut()
                    .insert("content-type", "application/octet-stream".parse().unwrap());
                response
                    .headers_mut()
                    .insert("cache-control", "no-store".parse().unwrap());
                response.headers_mut().insert(
                    "aq-material-delivery",
                    URL_SAFE_NO_PAD
                        .encode(serde_json::to_vec(&delivery).unwrap())
                        .parse()
                        .unwrap(),
                );
                response
            }
        };
        let scoped_calls = Arc::new(Mutex::new(Vec::new()));
        let scoped_mode = Arc::new(AtomicU8::new(0));
        let scoped_entered = Arc::new(tokio::sync::Semaphore::new(0));
        let scoped_release = Arc::new(tokio::sync::Semaphore::new(0));
        let inventory_saved = Arc::new(Mutex::new(std::collections::BTreeMap::<
            Uuid,
            aq_admission::scoped_presence::InventoryReceipt,
        >::new()));
        let inventory_calls = scoped_calls.clone();
        let inventory_mode = scoped_mode.clone();
        let inventory_route =
            move |Json(request): Json<aq_admission::scoped_presence::InventoryV2>| {
                let saved = inventory_saved.clone();
                let calls = inventory_calls.clone();
                let mode = inventory_mode.clone();
                async move {
                    request.validate().unwrap();
                    assert!(request.provisionable_only.is_empty());
                    calls
                        .lock()
                        .unwrap()
                        .push(("inventory".into(), serde_json::to_value(&request).unwrap()));
                    let (receipt, first) = {
                        let mut saved = saved.lock().unwrap();
                        if let Some(old) = saved.get(&request.operation_id) {
                            old.validate_for(&request).unwrap();
                            (old.clone(), false)
                        } else {
                            let receipt = aq_admission::scoped_presence::InventoryReceipt {
                                schema_version: 1,
                                identity: request.identity.clone(),
                                negotiation: request.negotiation.clone(),
                                inventory: aq_admission::scoped_presence::InventoryIdentity {
                                    operation_id: request.operation_id,
                                    sequence: request.sequence,
                                    revision: request.sequence + 10,
                                    request_digest: request.digest().unwrap(),
                                    snapshot_digest: Digest::of(
                                        b"synthetic server-owned normalized provisionable bindings",
                                    ),
                                },
                                recorded_at_ms: 2100,
                                expires_at_ms: 2100 + request.ttl_ms,
                            };
                            saved.insert(request.operation_id, receipt.clone());
                            (receipt, true)
                        }
                    };
                    if first && mode.load(Ordering::SeqCst) == 1 {
                        return (
                            axum::http::StatusCode::SERVICE_UNAVAILABLE,
                            "synthetic lost inventory acknowledgement",
                        )
                            .into_response();
                    }
                    Json(receipt).into_response()
                }
            };
        let presence_saved = Arc::new(Mutex::new(std::collections::BTreeMap::<
            Uuid,
            aq_admission::scoped_presence::PresenceReceipt,
        >::new()));
        let presence_calls = scoped_calls.clone();
        let presence_mode = scoped_mode.clone();
        let entered = scoped_entered.clone();
        let release = scoped_release.clone();
        let presence_route = move |Json(request): Json<
            aq_admission::scoped_presence::PresenceReport,
        >| {
            let saved = presence_saved.clone();
            let calls = presence_calls.clone();
            let mode = presence_mode.clone();
            let root = presence_root.clone();
            let entered = entered.clone();
            let release = release.clone();
            async move {
                request.validate().unwrap();
                let snapshot: Snapshot =
                    serde_json::from_slice(&fs::read(root.join("journal.json")).unwrap()).unwrap();
                let a = &snapshot.state.attempts[&request.binding.attempt_id];
                let original = &a.initial_grant.as_ref().unwrap().delivery;
                request
                    .validate_against(
                        &original.original.materials[0],
                        &a.prepared.as_ref().unwrap().request.materials[0],
                    )
                    .unwrap();
                calls
                    .lock()
                    .unwrap()
                    .push(("presence".into(), serde_json::to_value(&request).unwrap()));
                let (mut receipt, first) = {
                    let mut saved = saved.lock().unwrap();
                    if let Some(old) = saved.get(&request.operation_id) {
                        old.validate_for(&request).unwrap();
                        (old.clone(), false)
                    } else {
                        let expiry = if request.presence.is_present() {
                            aq_admission::scoped_presence::positive_expiry(
                                original.original.clock.server_time_ms,
                                2200,
                                request.ttl_ms,
                                30000,
                                original.execution_deadline_ms,
                            )
                            .unwrap()
                        } else {
                            2200 + request.ttl_ms
                        };
                        let receipt = aq_admission::scoped_presence::PresenceReceipt {
                            schema_version: 1,
                            operation_id: request.operation_id,
                            identity: request.identity.clone(),
                            negotiation: request.negotiation.clone(),
                            binding: request.binding.clone(),
                            grant_id: request.grant_id,
                            lease_id: request.lease_id,
                            sequence: request.sequence,
                            request_digest: request.digest().unwrap(),
                            inventory_at_record: request.inventory.clone(),
                            recorded_at_ms: 2200,
                            expires_at_ms: expiry,
                            disposition: aq_admission::scoped_presence::Disposition::Recorded,
                        };
                        saved.insert(request.operation_id, receipt.clone());
                        (receipt, true)
                    }
                };
                if first && mode.load(Ordering::SeqCst) == 2 {
                    return (
                        axum::http::StatusCode::SERVICE_UNAVAILABLE,
                        "synthetic lost presence acknowledgement",
                    )
                        .into_response();
                }
                if first && mode.load(Ordering::SeqCst) == 5 {
                    entered.add_permits(1);
                    release.acquire().await.unwrap().forget();
                }
                if mode.load(Ordering::SeqCst) == 3 {
                    receipt.request_digest = Digest::of(b"wrong request digest")
                }
                if mode.load(Ordering::SeqCst) == 4 {
                    return "x".repeat(5000).into_response();
                }
                Json(receipt).into_response()
            }
        };
        let phase_execution_root = dir.path().join("journal");
        let phase_execution_window = grant_window_ms.clone();
        let phase_execution_mode = grant_mode.clone();
        let phase_execution_route =
            move |headers: HeaderMap, Json(doc): Json<phase::Document>| {
                let root = phase_execution_root.clone();
                let window = phase_execution_window.clone();
                let mode = phase_execution_mode.clone();
                async move {
                    let phase::Document::Prepare(q) = doc else {
                        panic!("phase Prepare")
                    };
                    let snapshot: Snapshot =
                        serde_json::from_slice(&fs::read(root.join("journal.json")).unwrap())
                            .unwrap();
                    let a = &snapshot.state.attempts[&q.original.binding.attempt_id];
                    let nonce =
                        Uuid::parse_str(headers["aq-delivery-nonce"].to_str().unwrap()).unwrap();
                    let mut delivery =
                        phase_execution_delivery(a, &q, nonce, window.load(Ordering::SeqCst));
                    if mode.load(Ordering::SeqCst) == 1 {
                        delivery.receipt.grant.grant_id = Uuid::new_v4();
                    }
                    Json(phase::Document::ExecutionDelivery(
                        Box::new(delivery),
                    ))
                }
            };
        let host_report_mode = Arc::new(AtomicU8::new(0));
        let host_report_calls = Arc::new(Mutex::new(Vec::new()));
        let report_mode = host_report_mode.clone();
        let report_calls = host_report_calls.clone();
        let host_started_route = move |Json(request): Json<aq_admission::started::Request>| {
            let mode = report_mode.clone();
            let calls = report_calls.clone();
            async move {
                request.validate().unwrap();
                let mode = mode.load(Ordering::SeqCst);
                if mode == 0 {
                    return (
                        axum::http::StatusCode::NOT_FOUND,
                        "synthetic phase Started unsupported",
                    )
                        .into_response();
                }
                {
                    let mut calls = calls.lock().unwrap();
                    assert!(calls.len() < 8);
                    calls.push(request.clone());
                }
                if mode == 2 {
                    return (
                        axum::http::StatusCode::SERVICE_UNAVAILABLE,
                        "synthetic lost Started ACK",
                    )
                        .into_response();
                }
                let mut receipt = aq_admission::started::Receipt {
                    schema_version: 1,
                    operation_id: request.operation_id,
                    binding: request.binding.clone(),
                    grant_id: request.grant_id,
                    request_digest: request.digest().unwrap(),
                    recorded_at_ms: 2300,
                    authority_revision_at_record: 10,
                };
                if mode == 3 {
                    receipt.request_digest = Digest::of(b"wrong Started ACK");
                }
                Json(receipt).into_response()
            }
        };
        let phase_terminal_calls = Arc::new(Mutex::new(Vec::new()));
        let phase_calls = phase_terminal_calls.clone();
        let phase_mode = host_report_mode.clone();
        let phase_terminal_route =
            move |HttpPath(path): HttpPath<String>,
                  headers: HeaderMap,
                  Json(request): Json<Value>| {
                let calls = phase_calls.clone();
                let mode = phase_mode.clone();
                async move {
                    assert_eq!(headers["aq-phase-terminal-api-version"], "1");
                    let mode = mode.load(Ordering::SeqCst);
                    if mode == 0 {
                        return axum::http::StatusCode::NOT_FOUND.into_response();
                    }
                    {
                        let mut calls = calls.lock().unwrap();
                        assert!(calls.len() < 16);
                        calls.push((path.clone(), request.clone()));
                    }
                    if mode == 2 {
                        return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
                    }
                    if mode == 4 && path == "started" {
                        // Delayed reply after receiving the immutable request:
                        // the worker must preserve an unknown Started outcome.
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    }
                    let mut receipt = match path.as_str() {
                        "started" => {
                            let q: terminal::StartedRequest =
                                serde_json::from_value(request).unwrap();
                            q.validate().unwrap();
                            serde_json::to_value(terminal::StartedReceipt {
                                schema_version: 1,
                                negotiation: q.negotiation.clone(),
                                origin: q.origin.clone(),
                                request_digest: q.digest().unwrap(),
                                original: aq_admission::started::Receipt {
                                    schema_version: 1,
                                    operation_id: q.original.operation_id,
                                    binding: q.original.binding.clone(),
                                    grant_id: q.original.grant_id,
                                    request_digest: q.original.digest().unwrap(),
                                    recorded_at_ms: 2300,
                                    authority_revision_at_record: 10,
                                },
                            })
                            .unwrap()
                        }
                        "stopped" => {
                            let q: terminal::StoppedRequest =
                                serde_json::from_value(request).unwrap();
                            q.validate().unwrap();
                            serde_json::to_value(terminal::StoppedReceipt {
                                schema_version: 1,
                                negotiation: q.negotiation.clone(),
                                origin: q.origin.clone(),
                                request_digest: q.digest().unwrap(),
                                original: aq_admission::stopped::Receipt {
                                    schema_version: 1,
                                    operation_id: q.original.evidence.stop_operation_id,
                                    binding: q.original.evidence.binding.clone(),
                                    grant_id: q.original.grant_id().unwrap(),
                                    request_digest: q.original.digest().unwrap(),
                                    recorded_at_ms: 2400,
                                    authority_revision_at_record: 11,
                                },
                            })
                            .unwrap()
                        }
                        _ => return axum::http::StatusCode::NOT_FOUND.into_response(),
                    };
                    if mode == 3 {
                        receipt["request_digest"] = json!(Digest::of(b"wrong phase ACK"));
                    }
                    ([("aq-phase-terminal-api-version", "1")], Json(receipt)).into_response()
                }
            };
        let router = Router::new()
            .route("/v1/worker/profiled/started", post(host_started_route))
            .route(
                "/v1/worker/profiled-v2/{terminal_path}",
                post(phase_terminal_route),
            )
            .route(
                "/v1/worker/profiled-v2/prepare",
                post(phase_execution_route),
            )
            .route("/v1/worker/profiled-v2/proposal", post(proposal_route))
            .route("/v1/worker/profiled-v2/accept", post(phase_accept_route))
            .route("/v1/worker/profiled/inventory-v2", post(inventory_route))
            .route(
                "/v1/worker/profiled/material-presence",
                post(presence_route),
            )
            .route("/v1/worker/profiled/accept", post(accept_route))
            .route("/v1/worker/profiled/prepare", post(prepare_route))
            .route("/v1/worker/profiled/artifact", post(artifact_route))
            .route("/v1/worker/profiled/material", post(material_route))
            .route("/v1/worker/profiled/source/{id}", get(status_route))
            .route("/v1/worker/profiled/source/{id}/{member}", get(range_route));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let client = NativeClient::from_config(&crate::config::WorkerConfig {
            server_url: format!("http://{address}"),
            node_id: identity().node_id,
            credential: "synthetic-node-secret".into(),
            cf_client_id: None,
            cf_client_secret: None,
        })
        .unwrap();
        let mut proposed = None;
        let (preparation, phase) = if let Some(accept) = accept {
            let p = Arc::new(client.accept(&accept, cap, None).await.unwrap());
            journal.record_preparation(id, &p).unwrap();
            (Some(p), None)
        } else if defer_phase_accept {
            (None,None)
        } else {
            let p = client
                .$propose(&identity(), &offered, &phase_proposal.negotiation, cap)
                .await
                .unwrap();
            let source_profile: ArtifactRef = serde_json::from_value(
                published_phase_fixture()["accept"]["hold"]["source_profile"]
                    .clone(),
            )
            .unwrap();
            let q = journal
                .$begin(id, &p, capabilities(), source_profile, 30000)
                .unwrap();
            let a = Arc::new(client.$accept(&p, &q, None).await.unwrap());
            journal.$record(id, &a).unwrap();
            proposed = Some(p);
            (None, Some(a))
        };
        let store = MaterialStore::open(&materials, identity(), 32).unwrap();
        let pipeline = Arc::new(InputPipeline::new(journal, store, client.clone()));
        Self {
            pipeline: Some(pipeline),
            result_pins,
            preparation,
            phase,
            phase_window,
            proposed,
            client,
            reference,
            id,
            root: dir.path().join("journal"),
            _dir: dir,
            task,
            mode,
            range_entered,
            inline_entered,
            inline_release,
            inline_max_active,
            grant_mode,
            grant_window_ms,
            scoped_calls,
            scoped_mode,
            host_report_mode,
            host_report_calls,
            phase_terminal_calls,
            scoped_entered,
            scoped_release,
        }
    }
    async fn all_inputs(&self) -> Vec<crate::native::VerifiedArtifact> {
        let inputs = self
            .pipeline()
            .artifacts(self.id, self.preparation().clone())
            .await
            .unwrap();
        for lease in &self.preparation().plan().materials {
            self.pipeline()
                .material(self.id, self.preparation().clone(), lease.lease_id)
                .await
                .unwrap();
        }
        inputs
    }
    async fn spec(&self) -> crate::native::VerifiedArtifact {
        self.pipeline()
            .artifact(self.id, self.preparation().clone(), self.reference.clone())
            .await
            .unwrap()
    }
}
fn source_headers(r: &mut Response) {
    r.headers_mut()
        .insert("aq-source-api-version", "1".parse().unwrap());
    r.headers_mut()
        .insert("cache-control", "no-store".parse().unwrap());
}
fn artifact_headers(r: &mut Response, d: &aq_admission::RetainedArtifact, offset: u64) {
    r.headers_mut()
        .insert("content-type", "application/octet-stream".parse().unwrap());
    r.headers_mut()
        .insert("aq-admission-api-version", "0.1.7".parse().unwrap());
    r.headers_mut()
        .insert("cache-control", "no-store".parse().unwrap());
    r.headers_mut().insert(
        "aq-artifact-retention",
        URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(d).unwrap())
            .parse()
            .unwrap(),
    );
    r.headers_mut()
        .insert("aq-artifact-offset", offset.to_string().parse().unwrap());
}
struct GitPeer {
    client: GitClient,
    mode: Arc<AtomicU8>,
    calls: Arc<Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    _dir: tempfile::TempDir,
}
impl Drop for GitPeer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            t.join().unwrap()
        }
    }
}
impl GitPeer {
    fn new(journal: PathBuf, m: TransferManifest) -> Self {
        Self::profile(journal, m, true)
    }
    fn profile(journal: PathBuf, m: TransferManifest, native_inspection: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let socket = dir.path().join("git.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).unwrap();
        listener.set_nonblocking(true).unwrap();
        fs::create_dir(dir.path().join("artifacts")).unwrap();
        fs::set_permissions(
            dir.path().join("artifacts"),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        let wid = Uuid::new_v4();
        let work = dir.path().join("workspaces").join(wid.to_string());
        fs::create_dir_all(&work).unwrap();
        let mode = Arc::new(AtomicU8::new(0));
        let calls = Arc::new(Mutex::new(vec![]));
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let faults = mode.clone();
        let recorded = calls.clone();
        let thread = std::thread::spawn(move || {
            let mut created = Value::Null;
            let mut imported = Value::Null;
            let mut lost = false;
            while !flag.load(Ordering::SeqCst) {
                let (mut conn, _) = match listener.accept() {
                    Ok(c) => c,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(2));
                        continue;
                    }
                    Err(e) => panic!("{e}"),
                };
                let mut line = String::new();
                BufReader::new(conn.try_clone().unwrap())
                    .read_line(&mut line)
                    .unwrap();
                let req: Value = serde_json::from_str(&line).unwrap();
                let operation = req["operation"].as_str().unwrap();
                if operation != "capabilities" {
                    let snapshot: Snapshot =
                        serde_json::from_slice(&fs::read(journal.join("journal.json")).unwrap())
                            .unwrap();
                    assert!(
                        snapshot.state.attempts[&m.attempt.attempt_id]
                            .source
                            .as_ref()
                            .unwrap()
                            .operations
                            .values()
                            .any(|p| p.request == req)
                    );
                }
                recorded.lock().unwrap().push(req.clone());
                let result = match operation {
                    "capabilities" => {
                        let mut capabilities = json!({"protocol_version":1,"artifact_format_version":1,"limits":{"artifact":{"max_expanded_bytes":536870912u64,"max_bundle_bytes":67108864u64}},"git":{"checkout_profile":"no_attributes_v1"},"operations":["workspace.inspect","retention.inspect","repo.show","artifact.import","workspace.create","workspace.checkpoint","workspace.retire","artifact.export","retention.release","repo.cache-evict"]});
                        if !native_inspection {
                            capabilities["operations"]
                                .as_array_mut()
                                .unwrap()
                                .retain(|v| v != "workspace.inspect" && v != "retention.inspect");
                        }
                        capabilities
                    }
                    "repo.show" => {
                        json!({"repository_id":m.source.repository_id,"binding_generation":1,"identity":m.source.logical_identity,"mode":"worker"})
                    }
                    "artifact.import" => {
                        if imported.is_null() {
                            let native: GitManifest = serde_json::from_slice(
                                &fs::read(
                                    work.parent()
                                        .unwrap()
                                        .parent()
                                        .unwrap()
                                        .join("artifacts/incoming")
                                        .join(m.artifact_id.to_string())
                                        .join("manifest.json"),
                                )
                                .unwrap(),
                            )
                            .unwrap();
                            imported = json!({"receiver_installation_id":Uuid::new_v4(),"repository_id":m.source.repository_id,"binding_generation":1,"transfer_id":m.artifact_id,"source":m.source.base,"retention_handle":m.artifact_id,"manifest_digest":native.canonical_digest().unwrap(),"retention_state":"active","historical":false});
                        }
                        if faults.load(Ordering::SeqCst) == 1 && !lost {
                            lost = true;
                            continue;
                        }
                        imported.clone()
                    }
                    "workspace.create" => {
                        if created.is_null() {
                            created = json!({"workspace_id":wid,"repository_id":m.source.repository_id,"binding_generation":1,"generation":1,"base":m.source.base.hex,"path":work,"lifecycle":"active","allocating_operation":req["operation_id"],"last_checkpoint":null,"retiring_operation":null,"config_digest":"a".repeat(64)});
                        }
                        created.clone()
                    }
                    "retention.inspect" => {
                        json!({"id":m.artifact_id,"repository_id":m.source.repository_id,"binding_generation":1,"generation":1,"state":if faults.load(Ordering::SeqCst)==2{"released"}else{"active"},"refs":[{"name":format!("refs/aq-git/imports/{}",m.artifact_id),"oid":m.source.base.hex}]})
                    }
                    "workspace.inspect" => {
                        json!({"workspace":created,"head":if faults.load(Ordering::SeqCst)==3{json!({"algorithm":"sha1","hex":"b".repeat(40)})}else{serde_json::to_value(&m.source.base).unwrap()},"clean":true})
                    }
                    other => panic!("unexpected native Git action {other}"),
                };
                let mut reply = json!({"protocol_version":1,"request_id":req["request_id"],"ok":true,"result":result});
                if let Some(id) = req.get("operation_id") {
                    reply["operation_id"] = id.clone();
                    reply["operation_state"] = json!("complete");
                }
                let mut bytes = serde_json::to_vec(&reply).unwrap();
                bytes.push(b'\n');
                conn.write_all(&bytes).unwrap();
            }
        });
        let client = GitClient::new(&socket, dir.path()).unwrap();
        Self {
            client,
            mode,
            calls,
            stop,
            thread: Some(thread),
            _dir: dir,
        }
    }
}

async fn collect_result_fixture(h:&Inputs,git:&GitPeer)->crate::native::preparation::DurablePhaseExecution {
    let state=h.pipeline().snapshot().await.unwrap();let a=&state.attempts[&h.id];
    let (_config_root,mut config)=crate::native::runner::tests::config();let published=published_phase_fixture();
    config.negotiation=serde_json::from_value(published["proposal"]["negotiation"].clone()).unwrap();config.capabilities=capabilities();
    config.source_profile=serde_json::from_value(published["accept"]["hold"]["source_profile"].clone()).unwrap();config.maximum_preparation_ms=30000;config.renderer=a.reservation.offer.startup.renderer_qualification.clone();
    crate::native::runner::test_collect(h.pipeline(),&h.client,&config,h.id,&a.reservation.offer,a.reservation.deadline_ns,git.client.clone()).await.unwrap()
}
#[tokio::test]
async fn native_result_actual_retained_owner_refuses_json_launch_and_dies_with_pipeline(){
    let _clock=crate::native::http::test_clock::TestClock::new();
    let export=synthetic();let manifest=export.manifest.clone();let mut h=Inputs::new_result(export).await;
    fs::set_permissions(h._dir.path(),fs::Permissions::from_mode(0o700)).unwrap();
    let git=GitPeer::new(h.root.clone(),manifest);let grant=collect_result_fixture(&h,&git).await;
    let channel=h.pipeline().create_result_channel_in_namespace(&grant,h._dir.path().join("results"),h.result_pins.clone().unwrap()).await.unwrap();
    let state=h.pipeline().snapshot().await.unwrap();let history=state.attempts[&h.id].result.as_ref().unwrap();
    channel.verify_for(&channel.binding().launch,grant.history().record_digest(),&channel.binding().expected_outcome,&history.scope.workspace.path,&history.scope.installation).unwrap();
    let claim=aq_agent_result_local::Submission::new(channel.binding().clone(),aq_agent_result_local::SubmissionInput{schema_version:1,conclusion:aq_agent_result_local::Conclusion::new("Only explicit bounded test claim").unwrap()}).unwrap();
    let path=channel.submissions().join("submission.json");fs::write(&path,aq_agent_result_local::encode(&claim).unwrap()).unwrap();fs::set_permissions(&path,fs::Permissions::from_mode(0o600)).unwrap();
    assert!(h.pipeline().poll_result_claim(h.id).await.is_err(),"a file and real grant still cannot impersonate an actual Running host");
    assert!(h.pipeline().snapshot().await.unwrap().attempts[&h.id].result.as_ref().unwrap().claim.is_none());
    let lock=fs::File::open(channel.root().join("owner.lock")).unwrap();assert!(fs2::FileExt::try_lock_shared(&lock).is_err());
    drop(h.pipeline.take());
    fs2::FileExt::try_lock_shared(&lock).unwrap();
    assert!(channel.verify_for(&channel.binding().launch,grant.history().record_digest(),&channel.binding().expected_outcome,&history.scope.workspace.path,&history.scope.installation).is_err(),"historical owners never reconstruct live intake");
}
#[tokio::test]
async fn native_result_fsync_cut_never_publishes_intake_or_loses_pending_evidence(){
    let export=synthetic();let manifest=export.manifest.clone();let h=Inputs::new_result(export).await;
    fs::set_permissions(h._dir.path(),fs::Permissions::from_mode(0o700)).unwrap();
    let git=GitPeer::new(h.root.clone(),manifest);let grant=collect_result_fixture(&h,&git).await;
    h.pipeline().test_journal_fault(WriteFault::AfterFileSync).await.unwrap();
    let path=h._dir.path().join("result");assert!(h.pipeline().create_result_channel(&grant,path.clone(),h.result_pins.clone().unwrap()).await.is_err());
    assert!(!path.exists());let pending=fs::read(h.root.join("journal.pending")).unwrap();let snapshot:Snapshot=serde_json::from_slice(&pending).unwrap();
    assert_eq!(snapshot.digest,canonical_digest(&snapshot.state).unwrap());
    assert!(matches!(snapshot.state.attempts[&h.id].result.as_ref().unwrap().scope.storage,crate::native::result::history::StorageStage::Creating));
    assert!(h.pipeline().available().await.is_err(),"ambiguous fsync cannot free execution capacity");
}

}; }
pub(super) use physical_fixture;
