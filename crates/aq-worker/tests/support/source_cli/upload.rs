use super::*;
impl Peer {
    pub(super) fn begin(&self, value: Value) -> Response {
        let m: TransferManifest = serde_json::from_value(value["manifest"].clone()).unwrap();
        m.validate().unwrap();
        let mut upload = self.upload.lock().unwrap();
        if let Some((old, status)) = upload.as_ref() {
            assert_eq!(old, &m);
            return json_response(status);
        }
        let status = TransferStatus {
            transfer_id: m.transfer_id,
            manifest_offset: 0,
            bundle_offset: 0,
            receipt: None,
        };
        std::fs::create_dir_all(
            self.fixture
                .central
                .join("artifacts/incoming")
                .join(m.artifact_id.to_string()),
        )
        .unwrap();
        *upload = Some((m, status.clone()));
        json_response(status)
    }

    pub(super) fn finalize(&self, value: Value) -> Response {
        let r: TransferStatusRequest = serde_json::from_value(value).unwrap();
        let mut upload = self.upload.lock().unwrap();
        let (m, status) = upload.as_mut().unwrap();
        assert_eq!(r.transfer_id, m.transfer_id);
        if status.receipt.is_none() {
            assert_eq!(status.manifest_offset, m.manifest_bytes);
            assert_eq!(status.bundle_offset, m.bundle_bytes);
            status.receipt = Some(self.import_checkpoint(m));
        }
        json_response(status)
    }

    fn import_checkpoint(&self, m: &TransferManifest) -> CentralRetentionReceipt {
        let native: aq_worker::source::GitManifest = serde_json::from_slice(
            &std::fs::read(
                self.fixture
                    .central
                    .join("artifacts/incoming")
                    .join(m.artifact_id.to_string())
                    .join("manifest.json"),
            )
            .unwrap(),
        )
        .unwrap();
        let result = direct(
            &self.fixture.exe,
            &self.fixture.central,
            self.fixture.rid,
            "artifact.import",
            json!({"artifact_handle":m.artifact_id}),
            1,
        );
        assert_eq!(result["source"], json!(m.checkpoint));
        CentralRetentionReceipt {
            receipt_id: Uuid::new_v4(),
            transfer_id: m.transfer_id,
            repository_id: self.fixture.rid,
            binding_generation: 1,
            source: m.checkpoint.clone(),
            retention_handle: m.artifact_id,
            manifest_digest: native.canonical_digest().unwrap(),
        }
    }

    pub(super) fn chunk(&self, path: &str, query: &str, body: &[u8]) -> Response {
        let u = reqwest::Url::parse(&format!("http://fixture/?{query}")).unwrap();
        let fields: BTreeMap<_, _> = u.query_pairs().collect();
        let op = Uuid::parse_str(&fields["operation_id"]).unwrap();
        let mut operations = self.operations.lock().unwrap();
        let request = json!({"query":query,"bytes":body.to_vec()});
        if let Some((old, response)) = operations.get(&op) {
            assert_eq!(old, &request);
            return json_response(response);
        }
        let mut upload = self.upload.lock().unwrap();
        let (m, status) = upload.as_mut().unwrap();
        let manifest = path.ends_with("/manifest");
        let offset = if manifest {
            &mut status.manifest_offset
        } else {
            &mut status.bundle_offset
        };
        assert_eq!(*offset, fields["offset"].parse::<u64>().unwrap());
        use sha2::{Digest, Sha256};
        assert_eq!(fields["digest"], format!("{:x}", Sha256::digest(body)));
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(
                self.fixture
                    .central
                    .join("artifacts/incoming")
                    .join(m.artifact_id.to_string())
                    .join(if manifest {
                        "manifest.json"
                    } else {
                        "objects.bundle"
                    }),
            )
            .unwrap();
        f.write_all(body).unwrap();
        f.sync_all().unwrap();
        *offset += body.len() as u64;
        let response = json!(status);
        operations.insert(op, (request, response.clone()));
        json_response(response)
    }
}
