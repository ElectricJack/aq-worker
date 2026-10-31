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
    assert!(
        prepare_startup(
            &offer,
            &unsupported,
            verified(descriptor.clone(), &descriptor_bytes),
            verified(payload.clone(), &bytes),
        )
        .is_err()
    );
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
