use super::*;
impl Peer {
    pub(super) async fn base_download(&self, path: &str, query: &str) -> Response {
        let offer = self.coordinator.lock().unwrap().offer.clone();
        let native: aq_worker::source::GitManifest =
            serde_json::from_value(self.fixture.exported["manifest"].clone()).unwrap();
        let dir = self
            .fixture
            .central
            .join("artifacts")
            .join(native.transfer_id.to_string());
        if path.ends_with("/manifest") || path.ends_with("/bundle") {
            if path.ends_with("/bundle") && !self.held.swap(true, Ordering::SeqCst) {
                self.release.notified().await;
            }
            return read_base_chunk(&dir, path, query);
        }
        use sha2::{Digest, Sha256};
        let manifest = std::fs::read(dir.join("manifest.json")).unwrap();
        json_response(TransferManifest {
            transfer_id: Uuid::new_v4(),
            role: SourceRole::Base,
            checkpoint: self.fixture.binding.base.clone(),
            attempt: offer.attempt,
            resolved_spec_digest: offer.spec.resolved_spec_digest,
            source: self.fixture.binding.clone(),
            artifact_id: native.transfer_id,
            manifest_digest: format!("{:x}", Sha256::digest(&manifest)),
            bundle_digest: native.bundle_sha256,
            manifest_bytes: manifest.len() as u64,
            bundle_bytes: native.bundle_size,
        })
    }
}
fn read_base_chunk(dir: &Path, path: &str, query: &str) -> Response {
    let filename = if path.ends_with("/manifest") {
        "manifest.json"
    } else {
        "objects.bundle"
    };
    let bytes = std::fs::read(dir.join(filename)).unwrap();
    let url = reqwest::Url::parse(&format!("http://fixture/?{query}")).unwrap();
    let fields: BTreeMap<_, _> = url.query_pairs().collect();
    let offset: usize = fields["offset"].parse().unwrap();
    let length: usize = fields["limit"].parse().unwrap();
    bytes[offset..offset + length].to_vec().into_response()
}
