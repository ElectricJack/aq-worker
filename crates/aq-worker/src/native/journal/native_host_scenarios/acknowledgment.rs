#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires pinned native synthetic host and actual tmux3.4; fsync cut copies are history-only"]
async fn native_phase_terminal_ack_write_cuts_reopen_old_or_whole_ack_without_freeing_holds() {
    let (h, _git, execution) = ready(5000).await;
    let _host = running_host(&h, &execution).await;
    stop_and_persist(&h).await;
    let initial = h.pipeline().snapshot().await.unwrap();
    let t = initial.attempts[&h.id]
        .native_host
        .as_ref()
        .unwrap()
        .terminal
        .as_ref()
        .unwrap();
    h.host_report_mode.store(1, Ordering::SeqCst);
    let started = h
        .client
        .phase_terminal_started(&t.started.request)
        .await
        .unwrap();
    let stopped = h
        .client
        .phase_terminal_stopped(&t.stopped.as_ref().unwrap().request)
        .await
        .unwrap();
    for receipt in [TerminalAck::Started(started), TerminalAck::Stopped(stopped)] {
        for fault in [WriteFault::BeforeWrite, WriteFault::AfterRename] {
            let (dir, mut journal) = journal_copy(&h);
            journal.inject_fault(fault);
            assert!(receipt.apply(&mut journal, h.id, 0).is_err());
            assert!(journal.recovery_required());
            drop(journal);
            let mut reopened = Journal::open(
                &dir.path().join("journal"),
                identity().node_id,
                host_limits(),
            )
            .unwrap();
            check_ack_cut(&mut reopened, h.id, t, (&receipt, fault));
        }
    }
    cancel_ack_waiter(&h).await;
}
fn journal_copy(h: &Inputs) -> (tempfile::TempDir, Journal) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("journal");
    fs::create_dir(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    fs::copy(h.root.join("journal.json"), root.join("journal.json")).unwrap();
    fs::set_permissions(root.join("journal.json"), fs::Permissions::from_mode(0o600)).unwrap();
    let cap = host_limits();
    let journal = Journal::open(&root, identity().node_id, cap).unwrap();
    (dir, journal)
}
enum TerminalAck {
    Started(aq_phase_terminal_protocol::StartedReceipt),
    Stopped(aq_phase_terminal_protocol::StoppedReceipt),
}
impl TerminalAck {
    fn apply(
        &self,
        journal: &mut Journal,
        id: Uuid,
        recorded_time_offset: u64,
    ) -> anyhow::Result<()> {
        match self {
            Self::Stopped(ack) => {
                let mut receipt = ack.clone();
                receipt.original.recorded_at_ms += recorded_time_offset;
                journal.phase_stopped_receipt(id, receipt)
            }
            Self::Started(ack) => {
                let mut receipt = ack.clone();
                receipt.original.recorded_at_ms += recorded_time_offset;
                journal.phase_started_receipt(id, receipt)
            }
        }
    }
}
fn check_ack_cut(
    reopened: &mut Journal,
    id: Uuid,
    t: &crate::native::journal::phase_terminal::PhaseTerminalHistory,
    (receipt, fault): (&TerminalAck, WriteFault),
) {
    assert!(reopened.available().is_err());
    assert_eq!(reopened.state().attempts.len(), 1);
    let saved = reopened.state().attempts[&id]
        .native_host
        .as_ref()
        .unwrap()
        .terminal
        .as_ref()
        .unwrap();
    assert_eq!(
        saved.started.request.digest().unwrap(),
        t.started.request.digest().unwrap()
    );
    assert_eq!(
        saved.stopped.as_ref().unwrap().request.digest().unwrap(),
        t.stopped.as_ref().unwrap().request.digest().unwrap()
    );
    let present = match receipt {
        TerminalAck::Stopped(_) => saved.stopped.as_ref().unwrap().receipt.is_some(),
        TerminalAck::Started(_) => saved.started.receipt.is_some(),
    };
    assert_eq!(present, matches!(fault, WriteFault::AfterRename));
    if present {
        let revision = reopened.state().revision;
        assert!(receipt.apply(reopened, id, 1).is_err());
        assert_eq!(reopened.state().revision, revision);
    }
}
async fn cancel_ack_waiter(h: &Inputs) {
    // Cancel only the waiter after the ACK writer owns the fsync job. The
    // pipeline retains that job, immutable outbox and resources through commit.
    let pause = WritePause::new();
    h.pipeline().test_pause_grant_commit(&pause).await.unwrap();
    let pipeline = h.pipeline().clone();
    let id = h.id;
    let pending = tokio::spawn(async move { pipeline.report_phase_terminal_stopped(id).await });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !pause.entered() {
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
    pending.abort();
    pause.release();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if let Ok(Some(history)) = h.pipeline().phase_terminal_history(h.id).await
                && history
                    .stopped
                    .as_ref()
                    .is_some_and(|s| s.receipt.is_some())
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(h.pipeline().retained_input_stats().unwrap().attempts, 1);
    assert_eq!(h.pipeline().snapshot().await.unwrap().attempts.len(), 1);
}
