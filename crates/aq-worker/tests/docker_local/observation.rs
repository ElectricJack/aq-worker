/// The server as a node's observation store sees it: the real transport contract,
/// every batch acknowledged.
#[derive(Clone)]
struct ObservationServer {
    boot: Uuid,
    owner: std::sync::Arc<std::sync::Mutex<Option<aq_observation_admission::Owner>>>,
    receipts: std::sync::Arc<
        std::sync::Mutex<std::collections::BTreeMap<Uuid, aq_observation_admission::Receipt>>,
    >,
}
impl aq_observation_store::AuthenticatedOwnerPeer for ObservationServer {
    fn mutate(
        &mut self,
        request: &aq_observation_admission::Request,
    ) -> anyhow::Result<aq_observation_admission::Reply> {
        use aq_observation_admission as a;
        let mut owner = self.owner.lock().unwrap();
        let access = a::Access {
            node_id: Uuid::from_u128(1),
            boot_id: self.boot,
            accepted_through: 0,
            max_pending_distance: a::MAX_PENDING_DISTANCE,
            new_boot_previously_owned: false,
        };
        let mut receipts = self.receipts.lock().unwrap();
        let known = receipts.get(&request.operation_id);
        let reply = a::apply(
            owner.as_ref(),
            known,
            request,
            &access,
            Some(Uuid::from_u128(400)),
        )?
        .reply;
        receipts.insert(request.operation_id, reply.receipt.clone());
        *owner = Some(reply.current_owner.clone());
        Ok(reply)
    }
    fn status(&mut self, node_id: Uuid) -> anyhow::Result<aq_observation_admission::Status> {
        Ok(aq_observation_admission::Status {
            version: 1,
            node_id,
            admitted_node_boot_id: self.boot,
            owner: self.owner.lock().unwrap().clone(),
            accepted_through: 0,
        })
    }
}
impl aq_observation_store::node::NodePeer for ObservationServer {
    fn batch(
        &mut self,
        packet: &aq_lifecycle_protocol::Packet,
    ) -> anyhow::Result<aq_observation_admission::BatchReply> {
        use aq_lifecycle_protocol::*;
        let Message::Batch(batch) = &packet.body.message else {
            anyhow::bail!("packet class")
        };
        let receipt = Receipt {
            schema_version: SCHEMA_VERSION,
            message_id: packet.body.message_id,
            body_digest: packet.body_digest.clone(),
            disposition: ReceiptDisposition::ObservationRecorded,
            through_sequence: Some(batch.last_sequence),
        };
        Ok(aq_observation_admission::BatchReply::new(receipt, vec![]))
    }
}

/// The launch's registration (`launch::claude_docker::observe`) against a running node
/// store: without a catalog entry or a store nothing is registered and nothing fails;
/// with them the source carries the server-derived ID, Claude's transcript is the
/// attempt's `<attempt>.jsonl`, and Codex's is the earliest rollout under `sessions/`.
#[test]
fn a_launch_registers_its_transcript_with_the_node_store() {
    use aq_admission::observation_source;
    use aq_observation_store::node::SourceStatus;
    use aq_worker::native::launch::claude_docker::observe;
    let temp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap();
    let entry = reference_entry();

    let mount = |name: &str| {
        let directory = root.join(name);
        std::fs::create_dir(&directory).unwrap();
        directory
    };
    let node = transcript_node(&root);
    let store = node.observations();
    let account = |_: &str| Some("owner-account".to_owned());

    let unobserved = observation_offer(&entry, 7, "claude", false);
    assert!(observe(Some(&store), &unobserved, account, mount("none")).is_none());
    assert!(
        observe(
            None,
            &observation_offer(&entry, 8, "claude", true),
            account,
            mount("no-store")
        )
        .is_none()
    );
    assert!(
        observe(
            Some(&store),
            &observation_offer(&entry, 9, "claude", true),
            |_| None,
            mount("no-lease")
        )
        .is_none()
    );

    let (claude, codex) = (mount("claude"), mount("codex"));
    write_transcript_fixtures(&claude, &codex);
    for (attempt, harness, directory) in [(4, "claude", claude), (5, "codex", codex)] {
        let handle = observe(
            Some(&store),
            &observation_offer(&entry, attempt, harness, true),
            account,
            directory,
        )
        .unwrap_or_else(|| panic!("{harness} launch not registered"));
        let derived = observation_source::source_id(Uuid::from_u128(attempt), 1, &entry).unwrap();
        assert_eq!(handle.source_id(), derived);
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while handle.status() != Some(SourceStatus::Collecting) {
            assert!(
                std::time::Instant::now() < deadline,
                "{harness}: {:?}",
                handle.status()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    node.shutdown();
}
fn observation_offer(
    entry: &aq_session_protocol::ArtifactRef,
    attempt: u128,
    harness: &str,
    observed: bool,
) -> aq_session_protocol::ProfiledOffer {
    use aq_admission::observation_source;
    use aq_session_protocol::{ProfiledOffer, SessionIdentity, canonical_digest};
    let mut offer: ProfiledOffer = serde_json::from_str(include_str!(
        "../../../aq-agent-client/tests/fixtures/profiled-offer.json"
    ))
    .unwrap();
    let attempt_id = Uuid::from_u128(attempt);
    let SessionIdentity::Remote {
        node_id,
        boot_id,
        session_id,
        assignment_generation,
        ..
    } = offer.binding.session
    else {
        panic!("remote fixture")
    };
    let session = SessionIdentity::Remote {
        node_id,
        boot_id,
        attempt_id,
        assignment_generation,
        session_id,
    };
    offer.binding.attempt_id = attempt_id;
    offer.binding.session = session.clone();
    let m = &mut offer.manifest;
    m.bindings.attempt_id = attempt_id;
    m.bindings.session = session;
    m.harness.harness = harness.try_into().unwrap();
    if observed {
        m.modules.push(observation_source::module(entry).unwrap().0);
    }
    offer.binding.launch_manifest_digest = canonical_digest(m).unwrap();
    offer
}
fn reference_entry() -> aq_session_protocol::ArtifactRef {
    serde_json::from_value(
        json!({"id":"aq.observation-catalog/fixture","version":"1","digest":"ab".repeat(32)}),
    )
    .unwrap()
}

fn transcript_node(root: &std::path::Path) -> aq_observation_store::node::Node {
    use aq_observation_store::node::{Node, NodeLimits};
    let boot = Uuid::from_u128(1);
    let server = ObservationServer {
        boot,
        owner: Default::default(),
        receipts: Default::default(),
    };
    let limits = NodeLimits {
        poll: Duration::from_millis(5),
        ..NodeLimits::default()
    };
    Node::start(
        &root.join("store"),
        Uuid::from_u128(1),
        boot,
        limits,
        server,
        |_| {},
    )
    .unwrap()
}

fn write_transcript_fixtures(claude: &std::path::Path, codex: &std::path::Path) {
    let session = Uuid::from_u128(4);
    let line = json!({"type":"assistant","uuid":"u1","sessionId":session,
        "message":{"id":"m1","model":"model-a","content":[{"type":"text","text":"step"}]}});
    std::fs::write(claude.join(format!("{session}.jsonl")), format!("{line}\n")).unwrap();
    // The later rollout is not this session's: choosing it would never collect.
    let day = codex.join("2026/11/18");
    std::fs::create_dir_all(&day).unwrap();
    let rollout = "0199a213-81c0-7800-8aa1-bbab2a035a53";
    for (time, name, id) in [
        ("10-00-00", rollout, rollout),
        ("11-00-00", "0199a213-81c0-7800-8aa1-bbab2a035a54", "other"),
    ] {
        let meta = json!({"type":"session_meta","payload":{"id":id}});
        std::fs::write(
            day.join(format!("rollout-2026-11-18T{time}-{name}.jsonl")),
            format!("{meta}\n"),
        )
        .unwrap();
    }
}
