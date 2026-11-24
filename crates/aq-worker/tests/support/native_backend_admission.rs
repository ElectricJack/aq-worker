use super::*;

#[tokio::test]
async fn a_lost_sandbox_daemon_during_hydration_prevents_the_grant_request() {
    let _clock = crate::native::http::test_clock::TestClock::new();
    let export = synthetic();
    let manifest = export.manifest.clone();
    let h = Inputs::new_collect(export).await;
    let git = GitPeer::new(h.root.clone(), manifest);
    let state = snapshot_retrying_overload(h.pipeline()).await;
    let a = &state.attempts[&h.id];
    let (config_root, mut config) = crate::native::runner::tests::config();
    let published = published_phase_fixture();
    config.negotiation =
        serde_json::from_value(published["proposal"]["negotiation"].clone()).unwrap();
    config.capabilities = capabilities();
    config.source_profile =
        serde_json::from_value(published["accept"]["hold"]["source_profile"].clone()).unwrap();
    config.maximum_preparation_ms = 30000;
    config.renderer = a.reservation.offer.startup.renderer_qualification.clone();
    let root = config_root.path();
    std::fs::create_dir(root.join("journal")).unwrap();
    let pin = serde_json::json!({"artifact": config.adapter, "path": root.join("pin")});
    config.executor = Some(serde_json::from_value(serde_json::json!({
        "kind": "claude-2.1.318-docker-local-result-local-v1",
        "composite_definition": root.join("definition"),
        "settings": {
            "environment": config.renderer,
            "host": {
                "root": root, "helper": pin, "tmux": pin, "renderer": config.renderer,
                "columns": 120, "rows": 40,
                "docker_local": {"socket": root.join("offline.sock")},
                "template": format!("sha256:{}", "5".repeat(64)),
                "claude_executable": "/opt/aq/bin/claude",
                "max_workspace_bytes": 268435456
            }
        }
    })).unwrap());

    let result = crate::native::runner::TestPreparation {
        pipeline: h.pipeline(),
        inventory: &h.inventory().await,
        config: &config,
    }
    .collect(&a.reservation.offer, a.reservation.deadline_ns, git.client.clone())
    .await;
    let error = result.err().expect("the native grant must be withheld");
    assert_eq!(error.to_string(), "native grant withheld");
    assert_eq!(error.root_cause().to_string(), "sandbox_daemon_unavailable");
    let state = snapshot_retrying_overload(h.pipeline()).await;
    let attempt = &state.attempts[&h.id];
    assert!(attempt.phase_acceptance.is_some());
    assert!(attempt.phase_prepared.is_none());
    assert!(attempt.phase_execution.is_none());
}
