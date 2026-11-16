use aq_admission::{ArtifactKind, RetainedArtifact, startup::NativeOffer};
use aq_session_protocol::{Digest, canonical_bytes};
use aq_worker::native::*;

fn fixture() -> (NativeOffer, Vec<u8>) {
    let value: serde_json::Value = serde_json::from_str(include_str!(
        "../../../vendor/aq-admission-0.1.6/artifacts/startup.fixture.json"
    ))
    .unwrap();
    (
        aq_admission::startup::decode_native_offer(
            &serde_json::to_vec(&value["native_offer"]).unwrap(),
        )
        .unwrap(),
        value["payload_utf8"].as_str().unwrap().as_bytes().to_vec(),
    )
}
fn descriptors(offer: &NativeOffer) -> (RetainedArtifact, RetainedArtifact) {
    let find = |reference| {
        offer
            .offered
            .artifacts
            .iter()
            .find(|a| a.artifact == reference)
            .unwrap()
            .clone()
    };
    (
        find(offer.startup.artifact().unwrap()),
        find(offer.startup.payload.clone()),
    )
}
fn verified(descriptor: RetainedArtifact, bytes: &[u8]) -> VerifiedArtifact {
    let mut assembly = ArtifactAssembly::new(descriptor).unwrap();
    for chunk in bytes.chunks(7) {
        assembly.append(assembly.next_offset(), chunk).unwrap();
    }
    assembly.finish().unwrap()
}
#[test]
fn published_native_fixture_downloads_produce_one_host_payload() {
    let (offer, bytes) = fixture();
    let (descriptor, payload) = descriptors(&offer);
    let downloaded = verified(payload, &bytes);
    assert_eq!(downloaded.bytes(), bytes);
    let receipt = downloaded.receipt();
    assert_eq!(receipt.artifact, offer.startup.payload);
    assert_eq!(receipt.digest, Digest::of(&bytes));
    assert_eq!(receipt.bytes, bytes.len() as u64);
    prepare_startup(
        &offer,
        &offer.startup.renderer_qualification,
        verified(descriptor, &canonical_bytes(&offer.startup).unwrap()),
        downloaded,
    )
    .unwrap();
}
#[test]
fn exact_prefix_retry_is_idempotent_but_changed_partial_and_gapped_replies_refuse() {
    let (offer, bytes) = fixture();
    let (_, descriptor) = descriptors(&offer);
    let mut assembly = ArtifactAssembly::new(descriptor).unwrap();
    assembly.append(0, &bytes[..5]).unwrap();
    assembly.append(0, &bytes[..5]).unwrap();
    assert_eq!(assembly.next_offset(), 5);
    assert!(assembly.append(0, b"xxxxx").is_err());
    assert!(assembly.append(3, &bytes[3..7]).is_err());
    assert!(assembly.append(6, &bytes[6..]).is_err());
    assert!(assembly.append(u64::MAX, b"x").is_err());
    assert_eq!(assembly.next_offset(), 5);
    assembly.append(5, &bytes[5..]).unwrap();
    assert_eq!(assembly.finish().unwrap().bytes(), bytes);
}
#[test]
fn incomplete_corrupt_empty_oversized_and_source_bundle_downloads_refuse() {
    let (offer, bytes) = fixture();
    let (_, descriptor) = descriptors(&offer);
    assert!(
        ArtifactAssembly::new(descriptor.clone())
            .unwrap()
            .finish()
            .is_err()
    );
    let mut assembly = ArtifactAssembly::new(descriptor.clone()).unwrap();
    assert!(assembly.append(0, &[]).is_err());
    assert!(assembly.append(0, &vec![0; MAX_CHUNK_BYTES + 1]).is_err());
    let mut corrupt = bytes;
    corrupt[0] ^= 1;
    assembly.append(0, &corrupt).unwrap();
    assert!(assembly.finish().is_err());
    let mut bundle = descriptor;
    bundle.kind = ArtifactKind::Source;
    bundle.bytes = MAX_INLINE_BYTES + 1;
    bundle.validate().unwrap();
    assert!(ArtifactAssembly::new(bundle).is_err());
}
#[test]
fn self_consistent_replacement_cannot_replace_original_payload() {
    let (offer, mut bytes) = fixture();
    let (descriptor, mut payload) = descriptors(&offer);
    bytes[0] ^= 1;
    payload.artifact.digest = Digest::of(&bytes);
    assert!(
        prepare_startup(
            &offer,
            &offer.startup.renderer_qualification,
            verified(descriptor, &canonical_bytes(&offer.startup).unwrap()),
            verified(payload, &bytes),
        )
        .is_err()
    );
}
#[test]
fn offer_cannot_qualify_its_own_renderer_or_replace_retention_receipt() {
    let (offer, bytes) = fixture();
    let (descriptor, payload) = descriptors(&offer);
    let descriptor_bytes = canonical_bytes(&offer.startup).unwrap();
    let mut unsupported = offer.startup.renderer_qualification.clone();
    unsupported.digest = Digest::of(b"another installed renderer");
    let differing = prepare_startup(
        &offer,
        &unsupported,
        verified(descriptor.clone(), &descriptor_bytes),
        verified(payload.clone(), &bytes),
    );
    // any-model: the offer's renderer qualification is not compared with the
    // installed renderer; every other check below still refuses.
    assert_eq!(differing.is_ok(), cfg!(feature = "any-model"));
    let mut changed = payload;
    changed.retention_receipt.digest = Digest::of(b"different owner receipt");
    assert!(
        prepare_startup(
            &offer,
            &offer.startup.renderer_qualification,
            verified(descriptor, &descriptor_bytes),
            verified(changed, &bytes),
        )
        .is_err()
    );
}
#[test]
fn alternate_descriptor_encoding_is_not_the_content_addressed_object() {
    let (offer, bytes) = fixture();
    let (mut descriptor, payload) = descriptors(&offer);
    let pretty = serde_json::to_vec_pretty(&offer.startup).unwrap();
    descriptor.bytes = pretty.len() as u64;
    descriptor.artifact.digest = Digest::of(&pretty);
    assert!(
        prepare_startup(
            &offer,
            &offer.startup.renderer_qualification,
            verified(descriptor, &pretty),
            verified(payload, &bytes),
        )
        .is_err()
    );
}

/// Codex exact-settings checks through the public mapping API. Moved here from
/// the lib's unit tests (build budget); the helpers mirror
/// `src/native/adapter/codex/tests.rs`.
mod codex_settings {
    use aq_admission::startup::NativeOffer;
    use aq_session_protocol::{ArtifactRef, Digest, Text};
    use aq_worker::native::adapter::{
        AdapterManifest, ExpectedAdapter, FilePin, HandoffKind, InstalledAdapter,
        codex::{CODEX_CLI_VERSION, CodexExecProfile},
    };
    use serde::Serialize;
    use std::{os::unix::fs::PermissionsExt, path::Path};

    const PEER: &[u8] = b"#!/bin/sh\nexit 0\n";
    fn canonical_digest(value: &impl Serialize) -> anyhow::Result<Digest> {
        let mut value = serde_json::to_value(value)?;
        value.sort_all_objects();
        Ok(Digest::of(&serde_json::to_vec(&value)?))
    }
    fn fixture() -> (NativeOffer, Vec<u8>) {
        let value: serde_json::Value = serde_json::from_slice(include_bytes!(
            "../../../vendor/aq-admission-0.1.9/artifacts/startup.fixture.json"
        ))
        .unwrap();
        let mut offer: NativeOffer = serde_json::from_value(value["native_offer"].clone()).unwrap();
        let m = &mut offer.offered.offer.manifest;
        m.harness.harness = Text::new("codex").unwrap();
        m.harness.cli_version = Text::new(CODEX_CLI_VERSION).unwrap();
        m.model.provider = Text::new("openai").unwrap();
        m.model.reasoning = Text::new("medium").unwrap();
        m.model.spending_budget = None;
        m.terminal.input_enabled = false;
        offer.offered.offer.binding.launch_manifest_digest = canonical_digest(m).unwrap();
        offer.validate().unwrap();
        (
            offer,
            value["payload_utf8"].as_str().unwrap().as_bytes().to_vec(),
        )
    }
    /// An installation whose renderer is `renderer`.
    fn installed(dir: &Path, offer: &NativeOffer, renderer: &ArtifactRef) -> InstalledAdapter {
        let path = dir.join("executable");
        std::fs::write(&path, PEER).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o500)).unwrap();
        let reference = ArtifactRef {
            id: Text::new("fixture").unwrap(),
            version: Text::new("1").unwrap(),
            digest: Digest::of(PEER),
        };
        let pin = FilePin {
            path,
            artifact: reference.clone(),
        };
        let m = AdapterManifest {
            schema_version: 1,
            adapter: offer.offered.offer.manifest.harness.adapter.clone(),
            runtime: reference.clone(),
            qualification: reference.clone(),
            renderer: renderer.clone(),
            handoff: HandoffKind::OpaquePrivateFileV1,
            executable: pin.clone(),
            helper: pin.clone(),
            tmux: pin,
        };
        let digest = m.digest().unwrap();
        InstalledAdapter::open(
            m,
            ExpectedAdapter {
                manifest_digest: &digest,
                adapter: &offer.offered.offer.manifest.harness.adapter,
                runtime: &reference,
                qualification: &reference,
                renderer,
            },
        )
        .unwrap()
    }
    fn profile(o: &NativeOffer) -> CodexExecProfile {
        let m = &o.offered.offer.manifest;
        CodexExecProfile {
            harness: m.harness.clone(),
            model: m.model.clone(),
            permissions_digest: canonical_digest(&m.permissions).unwrap(),
            renderer: o.startup.renderer_qualification.clone(),
            confinement: None,
        }
    }
    /// A private home, a workspace and the root that owns both.
    fn dirs() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        std::fs::create_dir(&home).unwrap();
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
        let work = root.path().join("work");
        std::fs::create_dir(&work).unwrap();
        (root, home, work)
    }

    #[test]
    fn changed_payload_settings_private_mode_and_hard_budget_are_refused() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        std::fs::create_dir(&home).unwrap();
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
        let (offer, payload) = fixture();
        let digest = canonical_digest(&offer.offered.offer.manifest).unwrap();
        let work = root.path().join("work");
        std::fs::create_dir(&work).unwrap();
        let selected = profile(&offer);
        let mut adapter = installed(root.path(), &offer, &offer.startup.renderer_qualification);
        assert!(
            selected
                .prepare(
                    &mut adapter,
                    &offer,
                    &digest,
                    b"changed".to_vec(),
                    &work,
                    &home
                )
                .is_err()
        );
        // The installed model must equal the offered one; with any-model the offered
        // model is used instead (see the next test).
        let mut changed = selected;
        changed.model.reasoning = Text::new("high").unwrap();
        assert_eq!(
            changed
                .prepare(&mut adapter, &offer, &digest, payload.clone(), &work, &home)
                .is_err(),
            !cfg!(feature = "any-model")
        );
        let mut budgeted = offer.clone();
        budgeted.offered.offer.manifest.model.spending_budget =
            Some(aq_session_protocol::SpendingBudget {
                currency: Text::new("USD").unwrap(),
                basis: aq_session_protocol::BudgetBasis::TaskCumulativeEstimatedCost,
                limit_microunits: 1000,
            });
        let budget_digest = canonical_digest(&budgeted.offered.offer.manifest).unwrap();
        budgeted.offered.offer.binding.launch_manifest_digest = budget_digest.clone();
        assert!(
            profile(&budgeted)
                .prepare(
                    &mut adapter,
                    &budgeted,
                    &budget_digest,
                    payload.clone(),
                    &work,
                    &home
                )
                .is_err()
        );
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            profile(&offer)
                .prepare(&mut adapter, &offer, &digest, payload, &work, &home)
                .is_err()
        );
    }
    /// any-model: the installation and its profile carry another model and
    /// another renderer than the offer names. The offer is accepted and Codex is
    /// started with the offered model and reasoning. Without the feature the
    /// same offer is refused, and the installed harness stays exact either way.
    #[test]
    fn any_model_starts_codex_with_the_offered_model_and_any_renderer_qualification() {
        let (root, home, work) = dirs();
        let (offer, payload) = fixture();
        let manifest = &offer.offered.offer.manifest;
        let digest = canonical_digest(manifest).unwrap();
        let mut renderer = offer.startup.renderer_qualification.clone();
        renderer.digest = Digest::of(b"the installed renderer");
        let mut adapter = installed(root.path(), &offer, &renderer);
        let mut selected = profile(&offer);
        selected.renderer = renderer;
        let other_renderer =
            selected.prepare(&mut adapter, &offer, &digest, payload.clone(), &work, &home);
        assert_eq!(other_renderer.is_ok(), cfg!(feature = "any-model"));
        selected.model.model = Text::new("installed-model").unwrap();
        selected.model.reasoning = Text::new("high").unwrap();
        assert_ne!(manifest.model.reasoning.as_str(), "high");
        let prepared =
            selected.prepare(&mut adapter, &offer, &digest, payload.clone(), &work, &home);
        assert_eq!(prepared.is_ok(), cfg!(feature = "any-model"));
        if let Ok(prepared) = prepared {
            let args = &prepared.command().arguments;
            let offered = manifest.model.model.as_str();
            assert!(
                args.windows(2)
                    .any(|w| w[0] == "--model" && w[1] == offered)
            );
            assert!(!args.iter().any(|a| a.contains("installed-model")));
            let effort = format!(
                "model_reasoning_effort=\"{}\"",
                manifest.model.reasoning.as_str()
            );
            assert!(args.contains(&effort), "{args:?}");
        }
        selected.harness.cli_version = Text::new("0.0.0").unwrap();
        assert!(
            selected
                .prepare(&mut adapter, &offer, &digest, payload, &work, &home)
                .is_err()
        );
    }

    /// The Claude docker-local recipe accepts a manifest only with a reasoning
    /// value it can map (`default`, `off` or an effort).
    #[test]
    fn claude_docker_local_manifest_refuses_unknown_reasoning() {
        use aq_worker::native::adapter::claude::{
            CLAUDE_CLI_VERSION, validate_docker_local_manifest,
        };
        let (offer, _) = fixture();
        let mut m = offer.offered.offer.manifest;
        m.harness.harness = Text::new("claude").unwrap();
        m.harness.cli_version = Text::new(CLAUDE_CLI_VERSION).unwrap();
        m.model.provider = Text::new("anthropic").unwrap();
        m.harness.required_capabilities = vec![aq_session_protocol::Capability {
            id: Text::new(aq_session_protocol::CONTAINER_CONFINEMENT_CAPABILITY).unwrap(),
            version: 1,
        }];
        for known in ["default", "off", "low", "medium", "high", "xhigh", "max"] {
            m.model.reasoning = Text::new(known).unwrap();
            validate_docker_local_manifest(&m).unwrap();
        }
        for unknown in ["none", "High", "ultra", "--dangerously-skip-permissions"] {
            m.model.reasoning = Text::new(unknown).unwrap();
            let error = validate_docker_local_manifest(&m).unwrap_err();
            assert!(
                error.to_string().contains("unsupported reasoning"),
                "{error}"
            );
        }
    }
}
