//! Native consumer building blocks. Verified bytes are not execution permission.
//! Runtime journal, current authenticated transport and actual grants separately
//! own admission. Typed inline Source observations may use this buffer; Git
//! bundles use the separate streaming source owner.
pub mod adapter;
pub mod http;
pub mod journal;
pub mod materials;
mod phase_profile_shared;
pub mod preparation;
pub mod runner;
pub use preparation::launch;

use anyhow::{Result, ensure};
use aq_admission::{
    ArtifactKind, RetainedArtifact, startup::NativeOffer, transport::ArtifactReceipt,
};
use aq_session_host::RenderedStartup;
use aq_session_protocol::{ArtifactRef, Digest, canonical_bytes};

pub const MAX_INLINE_BYTES: u64 = 2_097_152;
pub const MAX_INLINE_SET_BYTES: u64 = 8 * 1024 * 1024;
pub const MAX_CHUNK_BYTES: usize = 262_144;

/// Bounded, ordered download assembly with exact prefix replay. A response may
/// neither create a gap nor replace already received bytes. Caller owns request
/// identity, authentication, timeout and the enclosing preparation deadline.
pub struct ArtifactAssembly {
    descriptor: RetainedArtifact,
    bytes: Vec<u8>,
}
pub struct VerifiedArtifact {
    descriptor: RetainedArtifact,
    bytes: Vec<u8>,
}
impl ArtifactAssembly {
    pub fn new(descriptor: RetainedArtifact) -> Result<Self> {
        descriptor.validate()?;
        ensure!(descriptor.bytes <= MAX_INLINE_BYTES, "inline artifact size");
        Ok(Self {
            descriptor,
            bytes: Vec::new(),
        })
    }
    pub fn next_offset(&self) -> u64 {
        self.bytes.len() as u64
    }
    pub fn append(&mut self, offset: u64, bytes: &[u8]) -> Result<()> {
        ensure!(
            !bytes.is_empty() && bytes.len() <= MAX_CHUNK_BYTES,
            "artifact chunk bound"
        );
        let end = offset.checked_add(bytes.len() as u64);
        ensure!(
            end.is_some_and(|end| end <= self.descriptor.bytes),
            "artifact chunk range"
        );
        let end = end.unwrap() as usize;
        if offset < self.next_offset() {
            ensure!(
                end <= self.bytes.len() && self.bytes[offset as usize..end] == *bytes,
                "artifact replay differs or overlaps new bytes"
            );
            return Ok(());
        }
        ensure!(offset == self.next_offset(), "artifact chunk gap");
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }
    pub fn finish(self) -> Result<VerifiedArtifact> {
        ensure!(
            self.bytes.len() as u64 == self.descriptor.bytes
                && Digest::of(&self.bytes) == self.descriptor.artifact.digest,
            "artifact incomplete or digest mismatch"
        );
        Ok(VerifiedArtifact {
            descriptor: self.descriptor,
            bytes: self.bytes,
        })
    }
}
impl VerifiedArtifact {
    pub fn receipt(&self) -> ArtifactReceipt {
        ArtifactReceipt {
            artifact: self.descriptor.artifact.clone(),
            bytes: self.bytes.len() as u64,
            digest: Digest::of(&self.bytes),
        }
    }
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// Build the host's single final payload only from independently checked actual
/// downloads. The installed renderer qualification comes from trusted runtime
/// configuration, not from accepting the offer's own claim of compatibility.
/// Historical retained timestamps are checked by the public contract; freshness
/// at launch still belongs to preparation/grant authority and local deadlines.
pub fn prepare_startup(
    offer: &NativeOffer,
    installed_renderer: &ArtifactRef,
    descriptor: VerifiedArtifact,
    payload: VerifiedArtifact,
) -> Result<RenderedStartup> {
    offer.validate()?;
    ensure!(
        offer.startup.renderer_qualification == *installed_renderer,
        "unsupported startup renderer qualification"
    );
    ensure!(
        descriptor.descriptor.artifact == offer.startup.artifact()?
            && descriptor.descriptor.kind == ArtifactKind::Definition
            && descriptor.bytes == canonical_bytes(&offer.startup)?
            && payload.descriptor.artifact == offer.startup.payload
            && payload.descriptor.kind == ArtifactKind::Context,
        "downloaded startup descriptor/payload mismatch"
    );
    for item in [&descriptor, &payload] {
        let expected = &item.descriptor;
        ensure!(
            offer.offered.artifacts.iter().any(|retained| {
                retained.artifact == expected.artifact
                    && retained.kind == expected.kind
                    && retained.bytes == expected.bytes
                    && retained.retention_receipt == expected.retention_receipt
                    && retained.retained_at_ms == expected.retained_at_ms
                    && retained.retain_until_ms == expected.retain_until_ms
            }),
            "download descriptor differs from original offer"
        );
    }
    offer.startup.verify_payload(&payload.bytes)?;
    RenderedStartup::new(
        offer.offered.offer.manifest.clone(),
        offer.startup.artifact()?,
        offer.startup.payload.clone(),
        payload.bytes,
    )
}
