struct FakeNode {
    agent_checkpoint: Option<RetentionReceipt>,
    stop: WriterStop,
    gate: InputGate,
    fences: Vec<InputFence>,
    pending: Vec<Vec<Uuid>>,
    closes: usize,
    stops: usize,
    reconciles: usize,
    computes: usize,
    disks: usize,
    log: RefCell<Vec<&'static str>>,
    _gate_dir: tempfile::TempDir,
}
impl FakeNode {
    fn new(retention: &mut FakeRetention) -> Self {
        Self::with_summary(retention, SUMMARY)
    }
    /// The agent retains `text` as its checkpoint summary before the directive.
    fn with_summary(retention: &mut FakeRetention, text: &str) -> Self {
        let request = CheckpointRequest::for_intent(&intent()).unwrap();
        let bytes = serde_json::to_vec(&summary(text)).unwrap();
        let receipt = retention.agent_retain(&request, ArtifactRole::Checkpoint, &bytes);
        let dir = tempfile::tempdir().unwrap();
        let gate = InputGate::new(dir.path()).unwrap();
        Self {
            agent_checkpoint: Some(receipt),
            stop: WriterStop::Stopped {
                proof: StopProof {
                    guardian_instance_id: GUARDIAN,
                    writer_set_digest: writer_set(),
                    stopped_at_boot_ms: 1200,
                },
                writer_count: 2,
                observed_at_ms: 1200,
            },
            gate,
            fences: Vec::new(),
            pending: Vec::new(),
            closes: 0,
            stops: 0,
            reconciles: 0,
            computes: 0,
            disks: 0,
            log: RefCell::new(Vec::new()),
            _gate_dir: dir,
        }
    }
}
impl SuspensionPorts for FakeNode {
    fn request_checkpoint(
        &mut self,
        request: &CheckpointRequest,
    ) -> Result<Option<RetentionReceipt>> {
        assert_eq!(request.max_summary_bytes, 16384);
        assert_eq!(request.role, ArtifactRole::Checkpoint);
        self.log.borrow_mut().push("checkpoint");
        Ok(self.agent_checkpoint.clone())
    }
    fn close_input(&mut self) -> Result<InputFence> {
        self.closes += 1;
        self.log.borrow_mut().push("close_input");
        self.gate.close()
    }
    fn stop_writers(&mut self) -> Result<WriterStop> {
        self.stops += 1;
        self.log.borrow_mut().push("stop");
        Ok(self.stop.clone())
    }
    fn reconcile_effects(
        &mut self,
        fence: &InputFence,
        pending: &[Uuid],
    ) -> Result<EffectsObservation> {
        self.reconciles += 1;
        self.fences.push(fence.clone());
        self.pending.push(pending.to_vec());
        Ok(EffectsObservation::Reconciled {
            receipt: artifact("effects-reconciliation"),
        })
    }
    fn release_compute(&mut self, stop: &StopObservation) -> Result<()> {
        assert!(matches!(stop, StopObservation::ReportedStopped { .. }));
        self.computes += 1;
        self.log.borrow_mut().push("release_compute");
        Ok(())
    }
    fn release_disk(&mut self, _directive: &ArtifactRef) -> Result<()> {
        self.disks += 1;
        Ok(())
    }
}

/// Fresh tmux host stand-in: issues new credentials into the new sandbox.
#[derive(Default)]
struct FakeLauncher {
    launches: Vec<(Binding, PathBuf)>,
}
impl SessionLauncher for FakeLauncher {
    fn launch(
        &mut self,
        successor: &Binding,
        layout: &CredentialLayout,
        bootstrap: &Path,
    ) -> Result<()> {
        assert!(bootstrap.exists(), "bootstrap instructions precede launch");
        for path in layout.paths() {
            std::fs::create_dir_all(path.parent().unwrap())?;
            std::fs::write(
                &path,
                format!("fresh-{}-{}", successor.session_id(), Uuid::new_v4()),
            )?;
        }
        self.launches
            .push((successor.clone(), bootstrap.to_owned()));
        Ok(())
    }
}

// ---------- sandbox helpers ----------

struct Node {
    _tmp: tempfile::TempDir,
    journal: PathBuf,
    layout: CredentialLayout,
}
fn node() -> Node {
    let tmp = tempfile::tempdir().unwrap();
    let layout = CredentialLayout::new(tmp.path().join("runner"), tmp.path().join("work"));
    std::fs::create_dir_all(&layout.runner_root).unwrap();
    std::fs::create_dir_all(&layout.work_root).unwrap();
    Node {
        journal: tmp.path().join("journal"),
        layout,
        _tmp: tmp,
    }
}

fn git(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(["-c", "user.name=h5", "-c", "user.email=h5@example.invalid"])
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// A workspace with a commit, a dirty tracked file and an untracked file.
fn dirty_workspace(layout: &CredentialLayout) {
    let repo = layout.workspace();
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("tracked.txt"), "committed\n").unwrap();
    git(&repo, &["add", "tracked.txt"]);
    git(&repo, &["commit", "-q", "-m", "base"]);
    std::fs::write(repo.join("tracked.txt"), "committed\ndirty edit\n").unwrap();
    std::fs::write(repo.join("untracked.txt"), "new work\n").unwrap();
}

/// Plant every credential kind; returns the planted secret strings.
fn plant_credentials(layout: &CredentialLayout) -> Vec<String> {
    layout
        .paths()
        .iter()
        .enumerate()
        .map(|(i, path)| {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let secret = format!("planted-secret-{i}-{}", Uuid::new_v4());
            std::fs::write(path, &secret).unwrap();
            secret
        })
        .collect()
}

fn contains(hay: &[u8], needle: &str) -> bool {
    hay.windows(needle.len()).any(|w| w == needle.as_bytes())
}

fn all_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(all_files(&p));
            } else {
                out.push(p);
            }
        }
    }
    out
}

fn hibernate(a: &Node) -> (Suspended, FakeRetention, FakeAq, FakeNode) {
    let mut retention = FakeRetention::default();
    let mut ports = FakeNode::new(&mut retention);
    let mut aq = FakeAq::new(predecessor());
    let parked = Suspender::new(&a.journal, predecessor(), a.layout.clone())
        .with_chunk_bytes(4096)
        .suspend(&suspend_directive(), &mut ports, &mut aq, &mut retention)
        .unwrap();
    (parked, retention, aq, ports)
}

/// AQ issues the continuation to the successor grant from the parked receipts.
fn continuation_packet(parked: &Suspended) -> Packet {
    let template = packet(4);
    let Message::Continuation(mut attachment) = template.body.message.clone() else {
        unreachable!()
    };
    attachment.retained = parked.report.central_receipts.clone();
    Packet::seal(template.body.message_id, Message::Continuation(attachment)).unwrap()
}

fn attachment_of(p: &Packet) -> ContinuationAttachment {
    match &p.body.message {
        Message::Continuation(a) => a.as_ref().clone(),
        _ => unreachable!(),
    }
}

fn ack_for(woken: &Woken, attachment: &ContinuationAttachment) -> BootstrapAck {
    BootstrapAck {
        admission_id: attachment.admission_id,
        continuation: attachment.continuation.clone(),
        successor: attachment.successor.clone(),
        attachment_digest: woken.attachment_digest.clone(),
        outcome: attachment.outcome.clone(),
        acknowledgment_receipt: artifact("authenticated-aq-bootstrap-ack"),
    }
}
