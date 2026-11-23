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
            verified(descriptor, &canonical_bytes(&offer.startup).unwrap()),
            verified(payload, &bytes),
        )
        .is_err()
    );
}
#[test]
fn offer_cannot_replace_retention_receipt() {
    let (offer, bytes) = fixture();
    let (descriptor, payload) = descriptors(&offer);
    let descriptor_bytes = canonical_bytes(&offer.startup).unwrap();
    let mut changed = payload;
    changed.retention_receipt.digest = Digest::of(b"different owner receipt");
    assert!(
        prepare_startup(
            &offer,
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

    include!("native_startup/commands.rs");
    include!("native_startup/sealed_image.rs");
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
            permissions_digest: None,
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
    /// Spec 0014: the installation pins no permission plan (the digest an old template
    /// still carries is ignored). Another role's operations and another project's scope
    /// start; a manifest for another adapter is still refused.
    #[test]
    fn codex_starts_with_any_offered_permission_plan() {
        let (root, home, work) = dirs();
        let (offer, payload) = fixture();
        let mut adapter = installed(root.path(), &offer, &offer.startup.renderer_qualification);
        let mut selected = profile(&offer);
        selected.permissions_digest = Some(Digest::of(b"the plan an old template pinned"));
        let offered = |change: &dyn Fn(&mut aq_session_protocol::LaunchManifest)| {
            let mut offer = offer.clone();
            let m = &mut offer.offered.offer.manifest;
            change(m);
            let digest = canonical_digest(m).unwrap();
            offer.offered.offer.binding.launch_manifest_digest = digest.clone();
            (offer, digest)
        };
        for (operations, scope) in [
            (
                &["task.read", "knowledge.read", "source.write"][..],
                "project:1/repository:1",
            ),
            (
                &["task.read", "knowledge.read"][..],
                "project:1/repository:1",
            ),
            (
                &["task.read", "knowledge.read", "source.write"][..],
                "project:2/repository:7",
            ),
        ] {
            let (offer, digest) = offered(&|m| {
                let scopes: Vec<_> = operations
                    .iter()
                    .map(|operation| aq_session_protocol::Scope {
                        operation: Text::new(*operation).unwrap(),
                        resource: Text::new(scope).unwrap(),
                    })
                    .collect();
                m.permissions.requested = scopes.clone();
                m.permissions.effective = scopes.clone();
                m.permissions.required = scopes;
            });
            offer.validate().unwrap();
            selected
                .prepare(&mut adapter, &offer, &digest, payload.clone(), &work, &home)
                .unwrap();
        }
        let (other, digest) = offered(&|m| m.harness.adapter.digest = Digest::of(b"other adapter"));
        let refused =
            selected.prepare(&mut adapter, &other, &digest, payload.clone(), &work, &home);
        assert!(refused.is_err(), "another adapter is not this installation");
        let (other, digest) = offered(&|m| m.harness.environment.digest = Digest::of(b"other"));
        let refused = selected.prepare(&mut adapter, &other, &digest, payload, &work, &home);
        assert!(
            refused.is_err(),
            "another environment is not this installation"
        );
    }
    /// The installation names no model, and its renderer differs from the one the
    /// offer names. Every offer is accepted and Codex is started with the offered
    /// model and reasoning (`default` passes no effort override); an unknown
    /// reasoning is refused, and the installed harness stays exact.
    #[test]
    fn codex_starts_with_the_offered_model_and_any_renderer_qualification() {
        let (root, home, work) = dirs();
        let (offer, payload) = fixture();
        let mut renderer = offer.startup.renderer_qualification.clone();
        renderer.digest = Digest::of(b"the installed renderer");
        assert_ne!(renderer, offer.startup.renderer_qualification);
        let mut adapter = installed(root.path(), &offer, &renderer);
        let mut selected = profile(&offer);
        selected.renderer = renderer;
        let offered = |model: &str, reasoning: &str| {
            let mut offer = offer.clone();
            let m = &mut offer.offered.offer.manifest;
            m.model.model = Text::new(model).unwrap();
            m.model.reasoning = Text::new(reasoning).unwrap();
            let digest = canonical_digest(m).unwrap();
            offer.offered.offer.binding.launch_manifest_digest = digest.clone();
            offer.validate().unwrap();
            (offer, digest)
        };
        for (model, reasoning) in [
            ("gpt-5.1-codex-mini", "medium"),
            ("a-model-no-installation-names", "high"),
            ("another-offered-model", "low"),
            ("yet-another-offered-model", "default"),
        ] {
            let (offer, digest) = offered(model, reasoning);
            let prepared = selected
                .prepare(&mut adapter, &offer, &digest, payload.clone(), &work, &home)
                .unwrap();
            let args = &prepared.command().arguments;
            assert!(
                args.windows(2).any(|w| w[0] == "--model" && w[1] == model),
                "{args:?}"
            );
            assert_eq!(args.iter().filter(|a| *a == "--model").count(), 1);
            let efforts: Vec<_> = (args.iter())
                .filter(|a| a.starts_with("model_reasoning_effort"))
                .collect();
            if reasoning == "default" {
                assert!(efforts.is_empty(), "{args:?}");
            } else {
                assert_eq!(
                    efforts,
                    [&format!("model_reasoning_effort=\"{reasoning}\"")]
                );
            }
        }
        let (unknown, unknown_digest) = offered("gpt-5.1-codex-mini", "xhigh");
        let Err(refused) = selected.prepare(
            &mut adapter,
            &unknown,
            &unknown_digest,
            payload.clone(),
            &work,
            &home,
        ) else {
            panic!("unknown reasoning accepted")
        };
        let expected = "expected default, low, medium or high";
        assert!(format!("{refused:#}").contains(expected), "{refused:#}");
        let digest = canonical_digest(&offer.offered.offer.manifest).unwrap();
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

#[path = "native_startup/events.rs"]
mod events;
#[path = "native_startup/installed_adapter.rs"]
mod installed_adapter;
#[path = "native_startup/native_image.rs"]
mod native_image;
