#[test]
fn suspension_recovers_at_every_boundary() {
    for boundary in SUSPENSION_BOUNDARIES {
        let a = node();
        dirty_workspace(&a.layout);
        plant_credentials(&a.layout);
        let mut retention = FakeRetention::default();
        let mut ports = FakeNode::new(&mut retention);
        let mut aq = FakeAq::new(predecessor());
        let err = Suspender::new(&a.journal, predecessor(), a.layout.clone())
            .with_chunk_bytes(4096)
            .with_boundaries(Boundaries::crash_after(boundary))
            .suspend(&suspend_directive(), &mut ports, &mut aq, &mut retention)
            .unwrap_err();
        assert!(
            err.downcast_ref::<Crashed>().is_some(),
            "{boundary}: {err:#}"
        );

        let mut recovered =
            Suspender::new(&a.journal, predecessor(), a.layout.clone()).with_chunk_bytes(4096);
        let parked = recovered
            .suspend(&suspend_directive(), &mut ports, &mut aq, &mut retention)
            .unwrap_or_else(|e| panic!("{boundary}: recovery failed: {e:#}"));
        assert_eq!(
            parked.report.disposition,
            SuspensionDisposition::RetainedSuspended,
            "{boundary}"
        );
        parked.report.validate().unwrap();
        parked.receipt.validate().unwrap();
        assert_single_effects(&ports, boundary);
        assert_eq!(retention.commits, 3, "{boundary}: one receipt per artifact");
        assert_eq!(parked.report.central_receipts.len(), 3);
        // A repeated directive returns the same transition, not a new one.
        let again = Suspender::new(&a.journal, predecessor(), a.layout.clone())
            .suspend(&suspend_directive(), &mut ports, &mut aq, &mut retention)
            .unwrap();
        assert_eq!(again, parked, "{boundary}: repeat recovers the same report");
        let distinct: BTreeSet<_> = aq.sent.iter().collect();
        assert_eq!(
            distinct.len(),
            2,
            "{boundary}: one checkpoint + one parking message id"
        );
    }
}

#[test]
fn suspension_refuses_conflicting_or_stale_directive() {
    let a = node();
    dirty_workspace(&a.layout);
    let (_parked, mut retention, mut aq, mut ports) = hibernate(&a);
    let mut other = match suspend_directive().body.message {
        Message::Suspend(i) => i,
        _ => unreachable!(),
    };
    other.deadline_ms += 1;
    let conflicting = Packet::seal(Uuid::new_v4(), Message::Suspend(other.clone())).unwrap();
    assert!(
        Suspender::new(&a.journal, predecessor(), a.layout.clone())
            .suspend(&conflicting, &mut ports, &mut aq, &mut retention)
            .is_err()
    );
    other.binding = successor();
    let stale = Packet::seal(Uuid::new_v4(), Message::Suspend(other)).unwrap();
    let b = node();
    assert!(
        Suspender::new(&b.journal, predecessor(), b.layout.clone())
            .suspend(&stale, &mut ports, &mut aq, &mut retention)
            .is_err()
    );
}

#[test]
fn unknown_stop_holds_without_snapshot_or_release() {
    let a = node();
    dirty_workspace(&a.layout);
    plant_credentials(&a.layout);
    let mut retention = FakeRetention::default();
    let mut ports = FakeNode::new(&mut retention);
    ports.stop = WriterStop::Unknown {
        reason: "guardian lost writer ownership".into(),
    };
    let mut aq = FakeAq::new(predecessor());
    let held = Suspender::new(&a.journal, predecessor(), a.layout.clone())
        .suspend(&suspend_directive(), &mut ports, &mut aq, &mut retention)
        .unwrap();
    assert_held_without_snapshot(&held, &ports, &retention);
    assert!(
        Suspender::new(&a.journal, predecessor(), a.layout.clone())
            .release_disk(&artifact("disk-release"), &mut ports, &mut aq)
            .is_err(),
        "held suspension cannot release protected disk"
    );
    assert_eq!(ports.disks, 0);
}

#[test]
fn protected_disk_released_only_by_explicit_directive() {
    let a = node();
    dirty_workspace(&a.layout);
    let (parked, _retention, mut aq, mut ports) = hibernate(&a);
    assert_disk_preserved(&parked, &ports);
    let directive = artifact("disk-release-directive");
    let released = Suspender::new(&a.journal, predecessor(), a.layout.clone())
        .release_disk(&directive, &mut ports, &mut aq)
        .unwrap();
    assert_disk_released(&released, &directive, &ports);
    // Idempotent under the same directive.
    Suspender::new(&a.journal, predecessor(), a.layout.clone())
        .release_disk(&directive, &mut ports, &mut aq)
        .unwrap();
    assert_eq!(ports.disks, 1);
}
fn assert_single_effects(ports: &FakeNode, boundary: &str) {
    for (effect, count) in [
        ("stop", ports.stops),
        ("close input", ports.closes),
        ("release compute", ports.computes),
        ("reconcile", ports.reconciles),
    ] {
        assert_eq!(count, 1, "{boundary}: {effect} applied once");
    }
    assert_eq!(
        ports.fences,
        [InputFence {
            last_accepted_operation_id: None
        }],
        "{boundary}: the journaled fence reaches effects"
    );
    assert_eq!(
        ports
            .log
            .borrow()
            .iter()
            .filter(|s| **s == "checkpoint")
            .count(),
        1,
        "{boundary}: checkpoint requested once"
    );
    // Effects are ordered: checkpoint, close, stop, then compute release.
    assert_eq!(
        *ports.log.borrow(),
        ["checkpoint", "close_input", "stop", "release_compute"],
        "{boundary}"
    );
}

fn assert_held_without_snapshot(held: &Suspended, ports: &FakeNode, retention: &FakeRetention) {
    assert!(matches!(
        held.report.disposition,
        SuspensionDisposition::Held { .. }
    ));
    assert_eq!(
        held.receipt.disposition,
        ReceiptDisposition::ParkingRecorded
    );
    assert!(!held.report.compute_released && ports.computes == 0);
    let retained_counts = (
        retention.chunks,
        retention.committed.len(),
        held.report.central_receipts.len(),
    );
    assert_eq!(
        retained_counts,
        (0, 0, 0),
        "held stop produced no uploads, commits, or receipts"
    );
}

fn assert_disk_preserved(parked: &Suspended, ports: &FakeNode) {
    let report = &parked.report;
    assert_eq!(
        (report.compute_released, report.protected_disk_released),
        (true, false)
    );
    assert_eq!(report.disk_release_directive, None);
    assert!(
        parked.report.local_retained_bytes > 0,
        "snapshot stays on protected disk"
    );
    assert_eq!(ports.disks, 0, "compute release does not release disk");
}

fn assert_disk_released(released: &Suspended, directive: &ArtifactRef, ports: &FakeNode) {
    let report = &released.report;
    assert_eq!(
        (report.protected_disk_released, report.local_retained_bytes),
        (true, 0)
    );
    assert_eq!(report.disk_release_directive.as_ref(), Some(directive));
    assert_eq!(ports.disks, 1);
}
