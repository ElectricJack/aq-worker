use super::*;
mod enrollment;
mod guardian;
mod lost_attempt;
mod observation;
use self::enrollment::{count, fake_server, fake_server_with, profile};
use observation::observation_peer;

/// Starts, then stops, an observation node from inside a runtime worker thread, the way the
/// service does: its blocking HTTP client is never built or dropped on the runtime.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn observation_nodes_stop_inside_a_runtime_without_panicking() {
    let dir = tempfile::tempdir().unwrap();
    let ms = std::time::Duration::from_millis;
    let id = Uuid::from_u128(7);
    let start = |directory: PathBuf| {
        let peer = HttpOwnerPeer::for_node("http://127.0.0.1:9", id, "aqn_test", &[], ms(100))?;
        let limits = NodeLimits {
            store_file_bytes: 1 << 20,
            store_data_bytes: 1 << 20,
            store_cache_bytes: 1 << 16,
            tracked_identities: 16,
            max_sources: 1,
            resume_grace: ms(10),
            discovery_entries: 16,
            poll: ms(10),
            retry_start: ms(10),
            retry_max: ms(20),
            finish_drain: ms(10),
        };
        ObservationNode::start(&directory, id, Uuid::from_u128(8), limits, peer, |_| {})
    };
    let node = off_runtime(|| start(dir.path().join("obs")).unwrap()).unwrap();
    stop_observations(vec![node]);
    // A node that is only dropped is stopped too.
    assert_eq!(
        off_runtime(|| start(dir.path().join("obs")).is_ok()),
        Some(true)
    );
}
fn slot_ctx(dir: &Path) -> SlotContext {
    SlotContext {
        slot: 1,
        node_id: Uuid::nil(),
        enrollment: dir.join("worker.toml"),
        state_dir: dir.join("current"),
        host_root: dir.join("run/1/host"),
        git_socket: dir.join("run/1/git.sock"),
        hold: std::time::Duration::ZERO,
    }
}
#[test]
fn restart_reason_follows_the_handoff_marker() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = slot_ctx(dir.path());
    let attempt = Uuid::from_u128(9);
    let driver = ctx.host_root.join("driver-1");
    std::fs::create_dir_all(&driver).unwrap();
    std::fs::write(
        driver.join("driver.json"),
        serde_json::json!({"owner": {"attempt_id": attempt}}).to_string(),
    )
    .unwrap();
    assert_eq!(restart_reason(&ctx, attempt), HandoffReason::Restart);
    for (written, expected) in [
        ("service_stop", HandoffReason::ServiceStop),
        ("owner_stop", HandoffReason::OwnerStop),
    ] {
        let _ = std::fs::remove_file(driver.join("handoff"));
        std::fs::write(
            driver.join("handoff"),
            format!(r#"{{"schema_version":1,"reason":"{written}"}}"#),
        )
        .unwrap();
        assert_eq!(restart_reason(&ctx, attempt), expected);
    }
    assert_eq!(
        restart_reason(&ctx, Uuid::from_u128(10)),
        HandoffReason::Restart
    );
}

/// Review #5 and #6: every input is checked before a secret is read or the server is
/// called, the plaintext rule of the node link included.
#[tokio::test]
async fn enroll_validates_everything_before_a_secret_or_a_request() {
    let server = fake_server(0).await;
    let p = profile();
    let repo = |format: &str| {
        serde_json::json!({"repository_id": Uuid::nil(), "identity": "r",
            "binding_generation": 1, "object_format": format})
    };
    let write_repos = |repos: Vec<serde_json::Value>| {
        std::fs::write(
            p.dir.path().join("repositories.json"),
            serde_json::to_vec(&repos).unwrap(),
        )
        .unwrap()
    };
    write_repos(vec![repo("sha1"); 17]);
    let e = p.enroll(&p.args(&server.origin), None).await.unwrap_err();
    assert!(
        format!("{e:#}").contains("at most 16 repositories"),
        "{e:#}"
    );
    write_repos(vec![repo("sha256")]);
    let e = p.enroll(&p.args(&server.origin), None).await.unwrap_err();
    assert!(format!("{e:#}").contains("object_format"), "{e:#}");
    write_repos(vec![repo("sha1")]);
    for origin in ["http://10.0.0.5:8081", "http://aq.example.com"] {
        let e = p.enroll(&p.args(origin), None).await.unwrap_err();
        assert!(format!("{e:#}").contains("must be https"), "{e:#}");
    }
    assert_eq!((count(&server.enrolls), count(&server.claims)), (0, 0));
    assert!(!p.layout.config_dir.exists(), "nothing written");
}

/// Review #5: an enrolled profile is refused (no code is read) unless --reconfigure.
#[tokio::test]
async fn enroll_refuses_an_enrolled_profile_without_reconfigure() {
    let server = fake_server(0).await;
    let p = profile();
    let mut args = p.args(&server.origin);
    p.enroll(&args, Some("code-1")).await.unwrap();
    assert!(p.slot1().contains("aqn_cred1") && p.layout.service_toml().exists());
    let e = p.enroll(&args, None).await.unwrap_err();
    assert!(format!("{e:#}").contains("--reconfigure"), "{e:#}");
    assert_eq!(count(&server.enrolls), 1);
    args.reconfigure = true;
    p.enroll(&args, Some("code-2")).await.unwrap();
    assert!(p.slot1().contains("aqn_cred2"));
    let slot2 = std::fs::read_to_string(p.layout.slot_enrollment(2)).unwrap();
    assert!(slot2.contains("aqn_slot2"));
}

/// Review #5: a failed key claim leaves slot 1 as it was and keeps the redeemed credential;
/// the rerun resumes at the claim without a new code.
#[tokio::test]
async fn failed_claim_keeps_slot_one_and_resumes_without_a_code() {
    let server = fake_server(1).await;
    let p = profile();
    let old = EnrollmentFile {
        server_url: server.origin.clone(),
        node_id: Uuid::from_u128(7),
        credential: "aqn_old".into(),
        cf_client_id: None,
        cf_client_secret: None,
    };
    old.save(&p.layout.slot_enrollment(1)).unwrap();
    let mut args = p.args(&server.origin);
    args.reconfigure = true;
    let e = p.enroll(&args, Some("code-1")).await.unwrap_err();
    assert!(format!("{e:#}").contains("is in use"), "{e:#}");
    assert!(p.slot1().contains("aqn_old"), "slot 1 is untouched");
    assert!(pending_enrollment(&p.layout).exists() && !p.layout.service_toml().exists());
    args.reconfigure = false;
    p.enroll(&args, None).await.unwrap();
    assert_eq!((count(&server.enrolls), count(&server.claims)), (1, 2));
    assert!(p.slot1().contains("aqn_cred1"));
    assert!(!pending_enrollment(&p.layout).exists() && p.layout.service_toml().exists());
    let other = p.args("https://other.example.com");
    assert!(
        p.enroll(&other, None)
            .await
            .unwrap_err()
            .to_string()
            .contains("--reconfigure")
    );
}

/// Round 2, D: the claim succeeded but the run died before the slot files were written. The
/// server returns slot 2's credential only once, so the rerun names the precise remedy; and
/// a run that died after the files were installed (pending not yet removed) completes.
#[tokio::test]
async fn resume_after_a_successful_claim_is_done_or_names_the_remedy() {
    let server = fake_server(0).await;
    let p = profile();
    let first = p.pending(&server.origin);
    let request = SlotsRequest {
        schema_version: 1,
        machine_key: "desk".into(),
        slots: 2,
    };
    let lost = ServerLink::from_enrollment(&first)
        .unwrap()
        .slots(&request)
        .await
        .unwrap();
    assert!(
        lost.slots[0].credential.is_some(),
        "minted, then the run died"
    );
    let e = format!(
        "{:#}",
        p.enroll(&p.args(&server.origin), None).await.unwrap_err()
    );
    assert!(
        e.contains("Revoke the machine") && e.contains("--reconfigure"),
        "{e}"
    );
    assert!(!e.contains("rerun"), "{e}");
    assert_eq!(p.slot1(), "", "nothing installed");
    // The earlier run had staged (or installed) every slot file before it died.
    for (slot, node, credential) in [(1, 1, "aqn_cred1"), (2, 99, "aqn_slot2")] {
        let file = EnrollmentFile {
            node_id: Uuid::from_u128(node),
            credential: credential.into(),
            ..first.clone()
        };
        file.save(&staged(&p.layout.slot_enrollment(slot))).unwrap();
    }
    p.enroll(&p.args(&server.origin), None).await.unwrap();
    let slot2 = std::fs::read_to_string(p.layout.slot_enrollment(2)).unwrap();
    assert!(slot2.contains("aqn_slot2"));
    assert!(!staged(&p.layout.slot_enrollment(2)).exists());
    assert!(!pending_enrollment(&p.layout).exists() && p.layout.service_toml().exists());
    assert_eq!(count(&server.enrolls), 0, "no code was redeemed");
}

/// A node this service does not run (an enrolled node above `nodes`) has its retained
/// observation intent left in place and gets no request; a node it runs is observed.
#[test]
fn retained_intents_of_nodes_not_run_send_no_requests() {
    let (_runtime, seen, origin) = observation_peer();
    let dir = tempfile::tempdir().unwrap();
    let config = ServiceConfig::parse(&format!(
        "schema_version = 1\nprofile = \"local\"\nmachine_key = \"desk\"\nnodes = 1\n\
         native_template = \"/n.json\"\nstate_root = \"{}\"\n\
         [aq_git]\nbinary = \"/g\"\ngit_executable = \"/usr/bin/git\"\n",
        dir.path().display()
    ))
    .unwrap();
    let worker = |n: u128, credential: &str| WorkerConfig {
        server_url: origin.clone(),
        node_id: Uuid::from_u128(n),
        credential: credential.into(),
        cf_client_id: None,
        cf_client_secret: None,
    };
    let (live, retired) = (worker(1, "aqn_live"), worker(2, "aqn_retired"));
    let intent = (dir.path().join("observations")).join(retired.node_id.to_string());
    std::fs::create_dir_all(&intent).unwrap();
    std::fs::write(intent.join("intent.json"), b"{}").unwrap();
    let retired_session = Arc::new(NodeSession::pending());
    retired_session.retire();
    let (started, registrars) = start_node_observations(
        &config,
        vec![
            (live.clone(), Arc::new(NodeSession::pending())),
            (retired.clone(), retired_session),
        ],
    );
    assert_eq!(started.len(), 1);
    assert_eq!(
        registrars.keys().copied().collect::<Vec<_>>(),
        [live.node_id]
    );
    // The live node's own requests reach the server, so silence for the retired one means
    // nothing was sent for it.
    for _ in 0..250 {
        if seen.sent("aqn_live") > 0 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(seen.sent("aqn_live") > 0, "the live node is observed");
    for node in started {
        node.shutdown();
    }
    assert_eq!(seen.sent("aqn_retired"), 0);
    assert_eq!(std::fs::read(intent.join("intent.json")).unwrap(), b"{}");
}

/// Round 2, E: an Access refusal at a resumed claim says so, and --reconfigure on the
/// resume replaces the stored Access token without a new code.
#[tokio::test]
async fn resume_refused_by_access_takes_new_access_credentials() {
    let server = fake_server_with(0, Some("new-id")).await;
    let p = profile();
    p.pending(&server.origin);
    let mut args = p.args(&server.origin);
    let e = format!("{:#}", p.enroll(&args, None).await.unwrap_err());
    assert!(
        e.contains("Cloudflare Access refused the stored token"),
        "{e}"
    );
    assert!(e.contains("--reconfigure"), "{e}");
    (args.reconfigure, args.cf_service_token) = (true, true);
    p.enroll_access(&args, "new-id").await.unwrap();
    assert!(p.slot1().contains("new-id") && p.slot1().contains("aqn_cred1"));
    assert_eq!(count(&server.enrolls), 0);
}

/// D8: the machine label is the machine key unless --name gives one.
#[tokio::test]
async fn machine_label_defaults_to_the_machine_key() {
    let server = fake_server(0).await;
    let p = profile();
    let mut args = p.args(&server.origin);
    args.name = None;
    p.enroll(&args, Some("code-1")).await.unwrap();
    let machine: serde_json::Value =
        serde_json::from_slice(&std::fs::read(p.layout.machine_json()).unwrap()).unwrap();
    assert_eq!(
        (machine["key"].as_str(), machine["label"].as_str()),
        (Some("desk"), Some("desk"))
    );
    args.reconfigure = true;
    args.name = Some("Desk PC".into());
    p.enroll(&args, Some("code-2")).await.unwrap();
    let machine: serde_json::Value =
        serde_json::from_slice(&std::fs::read(p.layout.machine_json()).unwrap()).unwrap();
    assert_eq!(machine["label"], "Desk PC");
}

/// D9: the CLI uses service.toml's state and runtime roots, so purge removes a custom
/// state root.
#[tokio::test]
async fn purge_uses_the_configured_state_root() {
    let server = fake_server(0).await;
    let p = profile();
    p.enroll(&p.args(&server.origin), Some("code-1"))
        .await
        .unwrap();
    let custom = p.dir.path().join("aq-state");
    let mut config = ServiceConfig::load(&p.layout.service_toml()).unwrap();
    config.state_root = custom.clone();
    config.runtime_root = Some(p.dir.path().join("run/local"));
    config.save(&p.layout.service_toml()).unwrap();
    std::fs::create_dir_all(custom.join("slots/1/current")).unwrap();
    let defaults = Layout {
        node: None,
        config_dir: p.layout.config_dir.clone(),
        state_root: p.dir.path().join("default-state"),
        runtime_root: p.dir.path().join("default-run"),
    };
    let (layout, _) = loaded_in(defaults, "local").unwrap();
    assert_eq!(layout.state_root, custom);
    assert_eq!(layout.runtime_root, p.dir.path().join("run/local"));
    layout.prepare_runtime().unwrap();
    purge_dirs(&layout).unwrap();
    assert!(!custom.exists() && !layout.runtime_root.exists());
    assert!(!p.layout.config_dir.exists());
}

/// Spec 0014: an installed template still carries `executor.settings.permissions_digest`
/// and cannot be edited on a remote machine. The key is read and ignored, and a template
/// without it loads the same; an unknown key is still refused.
#[test]
fn a_template_with_or_without_the_old_permissions_digest_loads() {
    let artifact = |id: &str| serde_json::json!({"id": id, "version": "1", "digest": aq_session_protocol::Digest::of(id.as_bytes())});
    let pinned = |id: &str, path: &str| serde_json::json!({"artifact": artifact(id), "path": path});
    let template = "sha256:057ae952da10fff6c0812a4b8e41b09f0f34c2ba310763976357a786536b3950";
    let old = serde_json::json!({
        "kind": "claude-2.1.318-docker-local-result-local-v1",
        "composite_definition": "/var/lib/aq/composite.json",
        "settings": {
            "environment": artifact("environment"),
            "permissions_digest": aq_session_protocol::Digest::of(b"installed plan"),
            "host": {
                "claude_executable": "/opt/aq/bin/claude", "columns": 120, "rows": 40,
                "docker_local": {"shared_root_fs": true, "socket": "/run/user/1000/aq-sandbox/podman.sock",
                    "templates": [{"digest": template, "image": format!("registry.invalid/aq-template@{template}")}]},
                "helper": pinned("aq.host/aq-session-host", "/var/lib/aq/aq-session-host"),
                "tmux": pinned("aq.host/tmux", "/usr/bin/tmux"),
                "max_workspace_bytes": 268435456, "renderer": artifact("renderer"),
                "root": "/var/lib/aq/host", "template": template
            }
        }
    });
    let load = |wire: &serde_json::Value| serde_json::from_value::<ExecutorConfig>(wire.clone());
    let Ok(ExecutorConfig::ClaudeDockerLocalResultLocal { .. }) = load(&old) else {
        panic!(
            "the installed template shape is refused: {:?}",
            load(&old).err()
        );
    };
    // Its host root (never created by the installer) is not what a node registers with:
    // the preflight opens the service's runtime root, the parent of the slot runtimes.
    let dir = tempfile::tempdir().unwrap();
    let mut executor = load(&old).ok();
    registration_host_root(&mut executor, &slot_ctx(dir.path())).unwrap();
    let Some(ExecutorConfig::ClaudeDockerLocalResultLocal { settings, .. }) = executor else {
        unreachable!()
    };
    assert_eq!(settings.host.root, dir.path().join("run"));
    let mut new = old.clone();
    new["settings"]
        .as_object_mut()
        .unwrap()
        .remove("permissions_digest");
    assert!(load(&new).is_ok(), "{:?}", load(&new).err());
    new["settings"]["permission_plans"] = serde_json::json!([]);
    assert!(load(&new).is_err(), "unknown keys stay refused");
}
/// A granted execution with no launch intent, host or result is released (archived, the
/// slot freed), also behind added context. Every other hold stays held: a Prepare whose
/// grant is unknown, an inconsistent journal (a host or result without its history), and
/// an ordinary error is neither.
#[test]
fn only_a_granted_execution_that_never_launched_is_released() {
    use aq_worker::native::journal::RecoveryHold;
    let hold = |reason, why: &'static str| -> Result<SalvageDisposition> {
        Err(RecoveryHold(reason, anyhow::anyhow!(why)).into())
    };
    let never = hold(
        "launch_closure_pending",
        "execution granted, no launch recorded",
    );
    assert!(never_launched(&never));
    assert!(never_launched(&never.context("recover slot 1")));
    for (reason, why) in [
        ("prepare_disposition_unknown", "Prepare journaled, no grant"),
        ("journal_inconsistent", "runner native host history absent"),
        ("journal_unreadable: format", "trailing bytes"),
    ] {
        let held = hold(reason, why);
        assert!(!never_launched(&held), "{reason}");
        assert!(matches!(
            held_if_permanent(held.map(|_| RecoverOutcome::Clean)),
            Ok(RecoverOutcome::Held(_))
        ));
    }
    assert!(!never_launched(&Ok(SalvageDisposition::NotStarted)));
    assert!(!never_launched::<()>(&Err(anyhow::anyhow!(
        "server unreachable"
    ))));
}
/// R1: an inconsistent journal (missing or mixed history) holds the slot with its reason
/// instead of failing every retry; any other recovery error is still retried.
#[test]
fn an_inconsistent_journal_holds_the_slot_and_status_names_the_reason() {
    use aq_worker::native::journal::RecoveryHold;
    let mixed = anyhow::anyhow!("host without actual phase execution");
    let held = held_if_permanent(Err(RecoveryHold("journal_inconsistent", mixed).into()));
    let why = "journal_inconsistent: host without actual phase execution";
    assert_eq!(held.unwrap(), RecoverOutcome::Held(why.into()));
    let cause = anyhow::anyhow!("Prepare journaled, no grant");
    let wrapped = anyhow::Error::from(RecoveryHold("prepare_disposition_unknown", cause));
    let wrapped = held_if_permanent(Err(wrapped.context("recover slot 1"))).unwrap();
    let unknown = "prepare_disposition_unknown: Prepare journaled, no grant";
    assert_eq!(wrapped, RecoverOutcome::Held(unknown.into()));
    let transient = held_if_permanent(Err(anyhow::anyhow!("server unreachable")));
    assert!(
        transient.is_err(),
        "a transient failure is retried, not held"
    );
    let fine = held_if_permanent(Ok(RecoverOutcome::Reported)).unwrap();
    assert_eq!(fine, RecoverOutcome::Reported);

    let dir = tempfile::tempdir().unwrap();
    let layout = Layout {
        node: None,
        config_dir: dir.path().join("config"),
        state_root: dir.path().join("state"),
        runtime_root: dir.path().join("run"),
    };
    assert_eq!(slot_line(&layout, 1), "slot 1: not held");
    write_private(&layout.slot_held(1), why.as_bytes()).unwrap();
    let line = slot_line(&layout, 1);
    assert_eq!(
        line,
        format!("slot 1: held until the owner releases it: {why}")
    );
    assert_eq!(slot_line(&layout, 2), "slot 2: not held");
}
/// #51 review M4: `release-sandbox` refuses a sandbox whose attempt the slot's recovery still
/// holds for salvage (or cannot tell: the service owns the journal), unless it went through
/// the lost path or the slot has no journal.
#[test]
fn release_sandbox_refuses_an_attempt_held_for_salvage() {
    let dir = tempfile::tempdir().unwrap();
    let current = dir.path();
    let attempt = Uuid::from_u128(85);
    assert!(
        !held_for_salvage(current, attempt, || Ok(vec![attempt])),
        "no journal"
    );
    std::fs::write(current.join("journal"), b"").unwrap();
    assert!(held_for_salvage(current, attempt, || Ok(vec![attempt])));
    assert!(!held_for_salvage(current, attempt, || Ok(vec![])));
    assert!(held_for_salvage(current, attempt, || anyhow::bail!(
        "owned"
    )));
    std::fs::write(preserved_sandbox_record(current, attempt), b"{}").unwrap();
    assert!(
        !held_for_salvage(current, attempt, || Ok(vec![attempt])),
        "lost path"
    );
}
/// The node's heartbeat survives a 500 and an edge 403 (logged, retried with back-off) and
/// stops the node only when AQ itself revokes the credential.
#[tokio::test]
async fn heartbeat_errors_are_retried_and_only_a_revocation_stops_the_node() {
    use axum::{Json, Router, extract::State, http::StatusCode, response::IntoResponse};
    use std::sync::atomic::{AtomicU32, Ordering::SeqCst};
    let beats = Arc::new(AtomicU32::new(0));
    let probe = |Json(q): Json<serde_json::Value>| async move {
        Json(serde_json::json!({"nonce": q["nonce"], "server_time": "2026-11-17T00:00:00Z"}))
    };
    let beat = |State(beats): State<Arc<AtomicU32>>| async move {
        match beats.fetch_add(1, SeqCst) {
            0 => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
            1 => StatusCode::FORBIDDEN.into_response(),
            2 => Json(serde_json::json!({"server_version": "t", "heartbeat_interval_secs": 1}))
                .into_response(),
            _ => (
                StatusCode::UNAUTHORIZED,
                Json(
                    serde_json::json!({"error": aq_protocol::CREDENTIAL_REVOKED, "message": "no"}),
                ),
            )
                .into_response(),
        }
    };
    let app = Router::new()
        .route("/v1/worker/probe", axum::routing::post(probe))
        .route("/v1/worker/heartbeat", axum::routing::post(beat))
        .with_state(beats.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let worker = WorkerConfig {
        server_url: format!("http://{}", listener.local_addr().unwrap()),
        node_id: Uuid::from_u128(7),
        credential: "c".into(),
        cf_client_id: None,
        cf_client_secret: None,
    };
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let (node_stop, stop) = watch::channel(false);
    let mut stopped = stop.clone();
    let heartbeat = tokio::spawn(node_heartbeat(worker, node_stop, stop));
    // The 500 and the edge 403 are behind it: the third heartbeat was accepted, node still up.
    while beats.load(SeqCst) < 3 {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        !*stopped.borrow(),
        "a 500 and an edge 403 did not stop the node"
    );
    assert!(!heartbeat.is_finished());
    stopped.wait_for(|v| *v).await.unwrap();
    let error = heartbeat.await.unwrap().unwrap_err();
    assert!(
        format!("{error:#}").contains("credential rejected"),
        "{error:#}"
    );
    assert_eq!(beats.load(SeqCst), 4);
}
