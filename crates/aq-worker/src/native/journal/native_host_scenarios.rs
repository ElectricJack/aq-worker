//! Explicit local tmux3.4/pinned synthetic ELF. No real issuer/provider claim.
use super::*;
use crate::native::{
    launch::{
        Checkpoint, InstalledFilePin, NativeHostLaunchResult, NativeStopOutcome,
        SyntheticHostConfig, SyntheticHostProfile, SyntheticMode,
    },
    preparation::{DurablePhaseExecution, PhaseExecutionCommit},
};
fn pin(binary: &str, hash: &str, id: &str) -> InstalledFilePin {
    let path =
        PathBuf::from(std::env::var_os(binary).expect("explicit native host fixture binary"));
    let digest: Digest = serde_json::from_value(json!(
        std::env::var(hash).expect("explicit native host fixture SHA256")
    ))
    .unwrap();
    InstalledFilePin {
        path: fs::canonicalize(path).unwrap(),
        artifact: ArtifactRef {
            id: aq_session_protocol::Text::new(id).unwrap(),
            version: aq_session_protocol::Text::new("1").unwrap(),
            digest,
        },
    }
}
fn profile(h: &Inputs, mode: SyntheticMode) -> SyntheticHostProfile {
    let root = h._dir.path().join("host");
    if !root.exists() {
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let snapshot: Snapshot =
        serde_json::from_slice(&fs::read(h.root.join("journal.json")).unwrap()).unwrap();
    let a = &snapshot.state.attempts[&h.id];
    SyntheticHostProfile::new(SyntheticHostConfig {
        root,
        helper: pin(
            "AQ_NATIVE_HOST_BINARY",
            "AQ_NATIVE_HOST_SHA256",
            "aq.synthetic-host-helper",
        ),
        tmux: pin(
            "AQ_NATIVE_TMUX_BINARY",
            "AQ_NATIVE_TMUX_SHA256",
            "aq.synthetic-system-tmux34",
        ),
        program: pin(
            "AQ_NATIVE_SYNTHETIC_ELF",
            "AQ_NATIVE_SYNTHETIC_SHA256",
            "aq.synthetic-native-harness",
        ),
        harness: a.reservation.offer.offered.offer.manifest.harness.clone(),
        capabilities: capabilities(),
        renderer: ArtifactRef {
            id: aq_session_protocol::Text::new("aq.synthetic-baseline-ascii").unwrap(),
            version: aq_session_protocol::Text::new("1").unwrap(),
            digest: Digest::of(b"synthetic baseline only; no Snapshot qualification"),
        },
        columns: 80,
        rows: 24,
        mode,
    })
    .unwrap()
}
// The ordinary lifecycle scenarios need time for hashing, fsync and real tmux startup.
const HOST_OBSERVATION_WINDOW_MS: u64 = 15_000;
async fn ready(window: u64) -> (Inputs, GitPeer, DurablePhaseExecution) {
    let export = synthetic();
    let manifest = export.manifest.clone();
    let h = {
        // Only the synthetic one-second proposal uses the fixture clock. Drop it
        // before preparing the execution grant or starting the physical host.
        let _clock = crate::native::http::test_clock::TestClock::new();
        Inputs::new_phase(export).await
    };
    let git = GitPeer::new(h.root.clone(), manifest);
    h.grant_window_ms.store(window, Ordering::SeqCst);
    let inputs = phase_join(&h, git.client.clone()).await;
    let snapshot = h.pipeline().snapshot().await.unwrap();
    let a = &snapshot.attempts[&h.id];
    let intent = a.phase_acceptance.as_ref().unwrap();
    let observed = h
        .client
        .prepare_execution(
            h.proposed.as_ref().unwrap(),
            &intent.request,
            h.phase.as_ref().unwrap(),
            inputs.request(),
            None,
        )
        .await
        .unwrap();
    let PhaseExecutionCommit::Usable(owner) = h
        .pipeline()
        .record_phase_execution(inputs, observed)
        .await
        .unwrap()
    else {
        panic!("synthetic execution owner expired before native host admission")
    };
    (h, git, owner)
}
fn launch_wait(
    h: &Inputs,
    execution: &DurablePhaseExecution,
) -> impl std::future::Future<Output = anyhow::Result<NativeHostLaunchResult>> + Send + 'static {
    h.pipeline()
        .launch_phase_synthetic(execution, profile(h, SyntheticMode::Wait))
}
fn current_host(h: &Inputs) -> crate::native::journal::launch::NativeHostHistory {
    h.pipeline().phase_host_history(h.id).unwrap().unwrap()
}
async fn running_host(
    h: &Inputs,
    grant: &DurablePhaseExecution,
) -> crate::native::launch::LocalPhaseHost {
    let outcome = launch_wait(h, grant).await.unwrap();
    let NativeHostLaunchResult::Running(host) = outcome else {
        panic!("actual Running absent")
    };
    host
}
fn host_limits() -> Limits {
    let mut cap = limits();
    cap.disk_bytes = 64 * 1024 * 1024 * 1024;
    cap
}
fn host_directory(history: &crate::native::journal::launch::NativeHostHistory) -> PathBuf {
    history
        .intent
        .host_root
        .join(history.intent.launch_id.to_string())
}
async fn stop_and_persist(h: &Inputs) {
    assert!(matches!(
        h.pipeline().stop_phase_host(h.id).await.unwrap(),
        NativeStopOutcome::StoppedPendingJournal(_)
    ));
    h.pipeline().persist_phase_host_stop(h.id).await.unwrap();
}
async fn terminal_history(
    h: &Inputs,
) -> crate::native::journal::phase_terminal::PhaseTerminalHistory {
    h.pipeline()
        .phase_terminal_history(h.id)
        .await
        .unwrap()
        .unwrap()
}
async fn wait_for_host(
    h: &Inputs,
    ready: impl Fn(&crate::native::journal::launch::NativeHostHistory) -> bool,
) -> crate::native::journal::launch::NativeHostHistory {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if let Ok(Some(h)) = h.pipeline().phase_host_history(h.id)
                && ready(&h)
            {
                break h;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await
        }
    })
    .await
    .unwrap()
}
async fn barrier(pause: &WritePause) {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !pause.entered() {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
}

include!("native_host_scenarios/host.rs");

include!("native_host_scenarios/recovery.rs");

include!("native_host_scenarios/reports.rs");

include!("native_host_scenarios/terminal.rs");

include!("native_host_scenarios/acknowledgment.rs");

#[cfg(feature = "test-support")]
#[path = "output_relay_scenarios.rs"]
mod output_relay_scenarios;
