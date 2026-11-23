#[tokio::test]
async fn create_is_idempotent_for_identical_request() {
    let mut h = harness().await;
    let request = request(1, 2, 3);
    let first = bounded(h.backend.create(&request)).await.unwrap();
    h.docker.clear_requests();
    let second = bounded(h.backend.create(&request)).await.unwrap();
    assert_eq!(first, second);
    assert!(posts(&h.docker).is_empty(), "{:?}", h.docker.requests());
    assert_eq!(h.docker.container_names(), vec![request.name.clone()]);
}

#[tokio::test]
async fn create_conflicts_on_foreign_same_name() {
    let mut h = harness().await;
    let original = request(1, 2, 3);
    bounded(h.backend.create(&original)).await.unwrap();
    let before = h.docker.container(&original.name).unwrap().body;
    h.docker.clear_requests();

    let mut different = original.clone();
    different.hard_deadline_ms += 1;
    assert_eq!(
        bounded(h.backend.create(&different)).await,
        Err(BackendError::Conflict)
    );
    h.docker.edit_container_labels(&original.name, |labels| {
        labels["aq.managed-by"] = json!("someone-else");
    });
    assert_eq!(
        bounded(h.backend.create(&original)).await,
        Err(BackendError::Conflict)
    );
    assert!(
        posts(&h.docker).is_empty(),
        "never adopt or delete a foreign object"
    );
    assert_eq!(
        h.docker.container(&original.name).unwrap().body["Image"],
        before["Image"]
    );

    // A foreign volume holding the work-volume name blocks create too.
    let other = request(1, 2, 4);
    let volume = format!("{}-work", other.name);
    let seeded = h
        .docker
        .send(DockerRequest {
            method: DockerMethod::Post,
            path: "/v1.44/volumes/create".into(),
            content_type: Some("application/json"),
            body: serde_json::to_vec(&json!({"Name": volume, "Labels": {"owner": "operator"}}))
                .unwrap()
                .into(),
            max_response_bytes: 1 << 20,
        })
        .await
        .unwrap();
    assert_eq!(seeded.status, 201);
    h.docker.clear_requests();
    assert_eq!(
        bounded(h.backend.create(&other)).await,
        Err(BackendError::Conflict)
    );
    assert!(h.docker.container(&other.name).is_none());
    assert_eq!(
        h.docker.volume(&volume).unwrap().labels,
        json!({"owner": "operator"})
    );
    assert!(deletes(&h.docker).is_empty());
}

#[tokio::test]
async fn lost_create_reply_is_discoverable() {
    let mut h = harness().await;
    let request = request(1, 2, 3);
    h.docker.script(Rule {
        method: DockerMethod::Post,
        path_prefix: "/v1.44/containers/create".into(),
        effect: Effect::DropReplyAfterApply,
    });
    assert!(matches!(
        bounded(h.backend.create(&request)).await,
        Err(BackendError::Uncertain(_))
    ));
    let owned = bounded(h.backend.list_owned(request.owner.node_id))
        .await
        .unwrap();
    assert_eq!(owned.len(), 1);
    let entry = &owned[0];
    assert_eq!(entry.name, request.name);
    assert_eq!(entry.template_digest, request.template_digest);
    assert_eq!(entry.hard_deadline_ms, request.hard_deadline_ms);
    assert_eq!(entry.state, SandboxState::Created);
    assert!(entry.disk_present);
    assert_eq!(
        bounded(h.backend.create(&request)).await.unwrap(),
        entry.sandbox
    );
}

#[tokio::test]
async fn list_owned_scopes_node_and_includes_old_boots() {
    let mut h = harness().await;
    let old_boot = request(1, 1, 1);
    let current = request(1, 2, 2);
    let other_node = request(9, 2, 3);
    let volume_only = request(1, 2, 4);
    for request in [&old_boot, &current, &other_node, &volume_only] {
        bounded(h.backend.create(request)).await.unwrap();
    }
    let sandbox = bounded(h.backend.inspect(&SandboxIdentity {
        kind: BackendKind::DockerLocal,
        backend_id: volume_only.name.clone(),
        owner: volume_only.owner.clone(),
    }))
    .await
    .unwrap()
    .sandbox;
    // Stopping a never-started sandbox removes only its container.
    assert!(bounded(h.backend.stop(&sandbox)).await.is_confirmed());
    assert!(h.docker.container(&volume_only.name).is_none());
    h.docker.clear_requests();

    let owned = bounded(h.backend.list_owned(Uuid::from_u128(1)))
        .await
        .unwrap();
    let mut names: Vec<&str> = owned.iter().map(|e| e.name.as_str()).collect();
    names.sort();
    let mut expected = vec![
        old_boot.name.as_str(),
        current.name.as_str(),
        volume_only.name.as_str(),
    ];
    expected.sort();
    assert_eq!(names, expected);
    let entry = owned.iter().find(|e| e.name == volume_only.name).unwrap();
    assert_eq!(entry.state, SandboxState::Stopped);
    assert_eq!(entry.running_processes, Some(0));
    assert!(entry.disk_present);
    assert_eq!(entry.sandbox, sandbox);
    let old = owned.iter().find(|e| e.name == old_boot.name).unwrap();
    assert_eq!(old.sandbox.owner, old_boot.owner);
    let node = Uuid::from_u128(1).to_string();
    for (_, path) in h.docker.requests() {
        assert!(path.contains(&format!("aq.node%3D{node}")), "{path}");
    }
}

#[tokio::test]
async fn inventory_failure_is_err() {
    let mut h = harness_with(ScriptedDocker::new(), |config| config.api_timeout_ms = 100).await;
    let request = request(1, 2, 3);
    let sandbox = bounded(h.backend.create(&request)).await.unwrap();
    let node = request.owner.node_id;
    for prefix in ["/v1.44/containers/json", "/v1.44/volumes?"] {
        h.docker.script(Rule {
            method: DockerMethod::Get,
            path_prefix: prefix.into(),
            effect: Effect::FailBefore(500),
        });
        assert_eq!(
            bounded(h.backend.list_owned(node)).await,
            Err(BackendError::Uncertain(UnconfirmedReason::BackendFailure)),
            "{prefix}"
        );
    }
    h.docker.pause();
    assert_eq!(
        bounded(h.backend.list_owned(node)).await,
        Err(BackendError::Uncertain(UnconfirmedReason::Timeout))
    );
    h.docker.resume();
    h.docker.edit_container_labels(&request.name, |labels| {
        labels.as_object_mut().unwrap().remove("aq.attempt");
    });
    let unknown = BackendError::Uncertain(UnconfirmedReason::UnknownState);
    assert_eq!(
        bounded(h.backend.list_owned(node)).await,
        Err(unknown.clone())
    );
    assert_eq!(bounded(h.backend.inspect(&sandbox)).await, Err(unknown));
}
