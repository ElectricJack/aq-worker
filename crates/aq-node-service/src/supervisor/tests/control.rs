use super::*;

/// A qualification pin in the control reply (current, switching, not yet effective) or
/// none at all changes nothing: the slots poll at once and are never paused by it.
#[tokio::test(start_paused = true)]
async fn a_qualification_pin_in_the_control_reply_is_ignored() {
    let now = wall();
    let soon = now + 20_000;
    for replies in [
        vec![reply(vec![], None)],
        vec![reply(vec![], Some(pin("1", now - 1000, now + 3_600_000)))],
        vec![
            reply(vec![], Some(pin("1", now - 1000, now + 86_400_000))),
            reply(vec![], Some(pin("2", soon, soon + 86_400_000))),
        ],
        vec![reply(vec![], Some(pin("2", soon, soon + 86_400_000)))],
    ] {
        let rig = rig(1);
        let backend = Arc::new(FakeBackend::default());
        script(&backend, 1, vec![Step::Observe]);
        let ctl = control(replies);
        let (handle, tx) = start(&rig, &backend, &ctl);
        sleep(Duration::from_secs(1)).await;
        assert_eq!(*backend.calls.lock().unwrap(), ["run:1"], "polls at once");
        let paused = backend.observed.lock().unwrap().clone().unwrap();
        for _ in 0..90 {
            sleep(Duration::from_secs(1)).await;
            assert!(!*paused.borrow(), "never paused");
        }
        sleep(Duration::from_secs(3600)).await;
        assert_eq!(*backend.calls.lock().unwrap(), ["run:1"]);
        stop(handle, tx).await;
    }
}

/// No slot polls before the first successful control reply; a failed control poll does
/// not count.
#[tokio::test(start_paused = true)]
async fn slots_wait_for_the_first_control_reply() {
    let rig = rig(1);
    let backend = Arc::new(FakeBackend::default());
    script(&backend, 1, vec![Step::Hang]);
    let ctl = control(vec![Err((500, false)), reply(vec![], None)]);
    let (handle, tx) = start(&rig, &backend, &ctl);
    sleep(Duration::from_secs(4)).await;
    assert!(backend.calls.lock().unwrap().is_empty(), "no control reply");
    sleep(Duration::from_secs(2)).await;
    assert_eq!(*backend.calls.lock().unwrap(), ["run:1"]);
    stop(handle, tx).await;
}

/// D4: each slot's stop notice is posted for its own slot.
#[tokio::test(start_paused = true)]
async fn each_stop_notice_is_posted_for_its_own_slot() {
    let rig = rig(2);
    let backend = Arc::new(FakeBackend::default());
    for slot in 1..=2 {
        script(&backend, slot, vec![Step::Hang]);
    }
    let ctl = control(vec![reply(vec![], None)]);
    let (handle, tx) = start(&rig, &backend, &ctl);
    sleep(Duration::from_secs(1)).await;
    let (a1, a2) = (Uuid::from_u128(1), Uuid::from_u128(2));
    driver(&rig, 1, a1);
    driver(&rig, 2, a2);
    tx.send_replace(true);
    sleep(Duration::from_secs(1)).await;
    let mut posted: Vec<_> = (ctl.handoffs.lock().unwrap().iter())
        .map(|(slot, n)| (*slot, n.attempt_id))
        .collect();
    posted.sort();
    assert_eq!(posted, [(1, a1), (2, a2)]);
    handle.await.unwrap().unwrap();
}

/// D4: `MachineLink` posts a slot's notice with that slot's node credential, and the
/// control poll with slot 1's.
#[tokio::test]
async fn machine_link_posts_each_notice_with_its_slots_credential() {
    use axum::{Json, Router, http::HeaderMap, routing::post};
    let seen = Arc::new(Mutex::new(Vec::<(String, String)>::new()));
    let record = |route: &'static str, seen: Arc<Mutex<Vec<(String, String)>>>| {
        post(
            move |h: HeaderMap, Json(body): Json<serde_json::Value>| async move {
                let bearer = h["authorization"].to_str().unwrap().to_owned();
                seen.lock().unwrap().push((route.to_owned(), bearer));
                Json(match route {
                    // The node-slots contract: the reply carries the request's schema
                    // version and, at version 2, the node's slot count.
                    "control" => serde_json::json!({"schema_version": body["schema_version"],
                    "machine_key": "desk", "desired": "running", "generation": 1,
                    "handoff": [], "slots": 2}),
                    _ => body,
                })
            },
        )
    };
    let app = Router::new()
        .route("/v1/worker/handoffs", record("handoffs", seen.clone()))
        .route("/v1/worker/control", record("control", seen.clone()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let rig = rig(2);
    for slot in 1..=2 {
        EnrollmentFile {
            server_url: origin.clone(),
            node_id: Uuid::from_u128(slot as u128),
            credential: format!("aqn_slot{slot}"),
            cf_client_id: None,
            cf_client_secret: None,
        }
        .save(&rig.layout.slot_enrollment(slot))
        .unwrap();
    }
    let first = EnrollmentFile::load(&rig.layout.slot_enrollment(1)).unwrap();
    let primary = Arc::new(ServerLink::from_enrollment(&first).unwrap());
    let link = MachineLink::new(primary, &rig.layout, 2);
    let notice = |id| HandoffNotice {
        schema_version: 1,
        attempt_id: Uuid::from_u128(id),
        reason: HandoffReason::ServiceStop,
    };
    link.handoff(2, &notice(2)).await.unwrap();
    link.handoff(1, &notice(1)).await.unwrap();
    let reply = link.control().await.unwrap();
    assert_eq!(
        (reply.schema_version, reply.node_slots()),
        (aq_protocol::NODE_SLOTS_SCHEMA_VERSION, 2)
    );
    assert!(link.handoff(3, &notice(3)).await.is_err(), "no slot 3");
    server.abort();
    let seen = seen.lock().unwrap().clone();
    let expected = [
        ("handoffs", "Bearer aqn_slot2"),
        ("handoffs", "Bearer aqn_slot1"),
        ("control", "Bearer aqn_slot1"),
    ];
    assert_eq!(seen, expected.map(|(r, b)| (r.to_owned(), b.to_owned())));
}
