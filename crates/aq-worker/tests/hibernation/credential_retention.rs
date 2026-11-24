#[test]
fn hibernation_leaves_no_credentials_in_retained_state() {
    let a = node();
    dirty_workspace(&a.layout);
    let secrets = plant_credentials(&a.layout);
    let (parked, retention, _aq, ports) = hibernate(&a);
    assert_suspended_resources(&parked, &ports);
    for path in a.layout.paths() {
        assert!(
            !path.exists(),
            "credential {} survived hibernation",
            path.display()
        );
    }
    assert_retained_credentials_removed(&retention, &secrets);
    assert_journal_credentials_removed(&a.journal, &secrets);
    assert_workspace_credential_holds();
    assert_summary_credential_holds();
}
fn assert_suspended_resources(parked: &Suspended, ports: &FakeNode) {
    assert_eq!(
        parked.report.disposition,
        SuspensionDisposition::RetainedSuspended
    );
    assert!(parked.report.compute_released && !parked.report.protected_disk_released);
    assert_eq!(
        parked.receipt.disposition,
        ReceiptDisposition::ParkingRecorded
    );
    assert_eq!(ports.stops, 1);
    assert_eq!(ports.pending, [vec![Uuid::from_u128(0x32)]]);
}

fn assert_retained_credentials_removed(retention: &FakeRetention, secrets: &[String]) {
    let retained = retention.all_bytes();
    assert_eq!(
        retained.len(),
        3,
        "source, checkpoint and stop proof retained centrally"
    );
    for (role, bytes) in &retained {
        for secret in secrets {
            assert!(
                !contains(bytes, secret),
                "{role:?} artifact holds a credential"
            );
        }
        if *role == ArtifactRole::Source {
            assert_archive_is_clean(bytes, secrets);
        }
    }
}

fn assert_archive_is_clean(bytes: &[u8], secrets: &[String]) {
    let archive = SourceArchive::from_bytes(bytes).unwrap();
    for (path, data) in archive.decoded().unwrap() {
        for known in [
            "permit.json",
            "provider.key",
            "access.json",
            ".credentials.json",
        ] {
            assert!(!path.ends_with(known), "retained credential path {path}");
        }
        assert!(!path.contains("agent-home"), "retained login path {path}");
        for secret in secrets {
            assert!(!contains(&data, secret));
        }
    }
}

fn assert_journal_credentials_removed(journal: &Path, secrets: &[String]) {
    // The local journal and protected snapshot never hold secret bytes either.
    for file in all_files(journal) {
        let bytes = std::fs::read(&file).unwrap();
        for secret in secrets {
            assert!(
                !contains(&bytes, secret),
                "{} holds a credential",
                file.display()
            );
        }
    }
}

fn assert_workspace_credential_holds() {
    // A credential copied into the workspace is an explicit hold: no retention.
    let b = node();
    dirty_workspace(&b.layout);
    let secrets = plant_credentials(&b.layout);
    std::fs::write(
        b.layout.workspace().join("notes.txt"),
        format!("token: {}\n", secrets[0]),
    )
    .unwrap();
    let (held, retention, _aq, ports) = hibernate(&b);
    assert!(matches!(
        held.report.disposition,
        SuspensionDisposition::Held { .. }
    ));
    assert_only_stop_proof_retained(&held, &retention);
    assert!(!held.report.compute_released && ports.computes == 0);
    for path in b.layout.paths() {
        assert!(
            !path.exists(),
            "credentials are scrubbed after stop even when held"
        );
    }
}

fn assert_summary_credential_holds() {
    // A credential leaked into the agent summary also holds.
    let c = node();
    dirty_workspace(&c.layout);
    let secrets = plant_credentials(&c.layout);
    let mut retention = FakeRetention::default();
    let mut ports = FakeNode::with_summary(&mut retention, &format!("my key is {}", secrets[1]));
    let mut aq = FakeAq::new(predecessor());
    let held = Suspender::new(&c.journal, predecessor(), c.layout.clone())
        .suspend(&suspend_directive(), &mut ports, &mut aq, &mut retention)
        .unwrap();
    assert!(matches!(
        held.report.disposition,
        SuspensionDisposition::Held { .. }
    ));
    assert_only_stop_proof_retained(&held, &retention);
}

/// A held suspension after a proven stop retains only the stop proof.
fn assert_only_stop_proof_retained(held: &Suspended, retention: &FakeRetention) {
    let roles: Vec<_> = retention.all_bytes().into_iter().map(|(r, _)| r).collect();
    assert_eq!(roles, [ArtifactRole::Result], "nothing but the stop proof");
    assert_eq!(held.report.central_receipts.len(), 1);
    assert!(stop_proof_receipt(&held.report.stop, &held.report.central_receipts).is_some());
}
