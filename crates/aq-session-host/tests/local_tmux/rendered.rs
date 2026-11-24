fn rendered_material(c: &mut Case, bytes: &[u8]) -> RenderedStartup {
    use aq_session_protocol::{ArtifactRef, Digest, Text};
    let digest = Digest::of(b"runtime-validated synthetic descriptor fixture");
    let descriptor = ArtifactRef {
        id: Text::new(format!("aq.startup-delivery/{}", digest.as_str())).unwrap(),
        version: Text::new("1").unwrap(),
        digest,
    };
    c.manifest.modules.push(descriptor.clone());
    c.local.context_material.clear();
    let payload = ArtifactRef {
        id: Text::new("final-startup").unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(bytes),
    };
    RenderedStartup::new(c.manifest.clone(), descriptor, payload, bytes.to_vec()).unwrap()
}
#[test]
#[ignore = "actual local tmux host qualification"]
fn rendered_startup_reaches_owned_harness_as_one_exact_payload() {
    let mut c = Case::new("rendered", false);
    let bytes = "framed\nfixture\n雪\0literal; $(no shell)\n".as_bytes();
    let startup = rendered_material(&mut c, bytes);
    let now = ownership::raw_boot_ms();
    let permit = ExecutionPermit {
        session: c.manifest.bindings.session.clone(),
        manifest_digest: canonical_digest(&c.manifest).unwrap(),
        sequence: 1,
        issued_boot_ms: now,
        expires_boot_ms: now + 1500,
    };
    let mut host = SessionHost::launch_rendered(
        c.temp.path(),
        c.id,
        &startup,
        permit,
        &c.local,
        &c.adapter,
        &mut c.authority,
    )
    .unwrap();
    let received = wait(|| std::fs::read(host.directory().join("rendered-received.bin")).ok());
    assert_eq!(received, bytes);
    assert!(!host.directory().join("context-1.bin").exists());
    let configuration: serde_json::Value =
        serde_json::from_slice(&std::fs::read(host.directory().join("launch.json")).unwrap())
            .unwrap();
    assert!(configuration.get("rendered_startup").is_some());
    host.stop().unwrap();
    let stopped = host.wait_stopped(Duration::from_secs(3)).unwrap();
    assert_eq!(stopped.phase, HostPhase::Stopped);
    assert!(stopped.kernel_wait_echild);
}
#[test]
#[ignore = "actual local tmux host qualification"]
fn rendered_payload_tampered_after_intent_is_refused_before_native_exec() {
    struct Tamper {
        inner: Authority,
        path: PathBuf,
    }
    impl LaunchAuthority for Tamper {
        fn record_launch_intent(&mut self, i: &LaunchIntent) -> Result<()> {
            self.inner.record_launch_intent(i)
        }
        fn record_guardian(&mut self, i: &GuardianIdentity) -> Result<()> {
            self.inner.record_guardian(i)
        }
        fn record_start_intent(&mut self, i: &LaunchIntent, g: &GuardianIdentity) -> Result<()> {
            self.inner.record_start_intent(i, g)
        }
        fn record_started(&mut self, o: &HostObservation) -> Result<()> {
            self.inner.record_started(o)
        }
        fn checkpoint(&mut self, stage: &str) -> Result<()> {
            if stage == "start-intent-durable" {
                std::fs::write(&self.path, b"corrupt")?;
            }
            Ok(())
        }
    }
    let mut c = Case::new("rendered", false);
    let startup = rendered_material(&mut c, b"fixture");
    let now = ownership::raw_boot_ms();
    let permit = ExecutionPermit {
        session: c.manifest.bindings.session.clone(),
        manifest_digest: canonical_digest(&c.manifest).unwrap(),
        sequence: 1,
        issued_boot_ms: now,
        expires_boot_ms: now + 1500,
    };
    let path = c.temp.path().join(c.id.to_string());
    let mut authority = Tamper {
        inner: c.authority,
        path: path.join("context-0.bin"),
    };
    assert!(
        SessionHost::launch_rendered(
            c.temp.path(),
            c.id,
            &startup,
            permit,
            &c.local,
            &c.adapter,
            &mut authority
        )
        .is_err()
    );
    assert!(!path.join("pane-exec.claim").exists());
    assert!(!path.join("fixture.claim").exists());
    let intent: LaunchIntent =
        serde_json::from_slice(&std::fs::read(c.temp.path().join("caller-intent.json")).unwrap())
            .unwrap();
    let mut host = SessionHost::recover(c.temp.path(), &intent).unwrap();
    let stopped = host.wait_stopped(Duration::from_secs(3)).unwrap();
    assert_eq!(stopped.phase, HostPhase::Stopped);
    assert!(stopped.kernel_wait_echild);
}
