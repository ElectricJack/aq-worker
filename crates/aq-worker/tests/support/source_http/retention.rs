use super::*;
impl PeerState {
    pub(super) fn begin(&mut self, c: &mut TcpStream, body: &[u8], fixture: &Fixture) {
        let request: Value = serde_json::from_slice(body).unwrap();
        let m: TransferManifest = serde_json::from_value(request["manifest"].clone()).unwrap();
        m.validate().unwrap();
        assert_eq!(m.source.repository_id, fixture.rid);
        if let Some(old) = &self.manifest {
            assert_eq!(old, &m);
        } else {
            self.status = Some(TransferStatus {
                transfer_id: m.transfer_id,
                manifest_offset: 0,
                bundle_offset: 0,
                receipt: None,
            });
            self.manifest = Some(m.clone());
            std::fs::create_dir_all(
                fixture
                    .central
                    .join("artifacts/incoming")
                    .join(m.artifact_id.to_string()),
            )
            .unwrap();
        }
        respond(c, &json!(self.status), false);
    }

    pub(super) fn chunk(&mut self, c: &mut TcpStream, request: &server::Request, central: &Path) {
        let body = request.body.as_slice();
        let (route, query) = request.path.split_once('?').unwrap();
        let fields: BTreeMap<_, _> = query
            .split('&')
            .map(|v| v.split_once('=').unwrap())
            .collect();
        let op = Uuid::parse_str(fields["operation_id"]).unwrap();
        if let Some((old, response)) = self.operations.get(&op) {
            assert_eq!(old, body);
            respond(c, response, false);
            return;
        }
        let m = self.manifest.as_ref().unwrap().clone();
        let offset: u64 = fields["offset"].parse().unwrap();
        use sha2::{Digest, Sha256};
        assert_eq!(fields["digest"], format!("{:x}", Sha256::digest(body)));
        let file = route.rsplit('/').next().unwrap();
        let (filename, current) = self.chunk_offset(file);
        assert_eq!(offset, *current);
        let path = central
            .join("artifacts/incoming")
            .join(m.artifact_id.to_string())
            .join(filename);
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        f.write_all(body).unwrap();
        f.sync_all().unwrap();
        *current += body.len() as u64;
        let response = json!(self.status);
        self.operations
            .insert(op, (body.to_vec(), response.clone()));
        respond(c, &response, false);
    }

    fn chunk_offset(&mut self, file: &str) -> (&'static str, &mut u64) {
        let status = self.status.as_mut().unwrap();
        match file {
            "manifest" => ("manifest.json", &mut status.manifest_offset),
            "bundle" => ("objects.bundle", &mut status.bundle_offset),
            _ => panic!("unexpected chunk route"),
        }
    }
    pub(super) fn finalize(&mut self, c: &mut TcpStream, body: &[u8], fixture: &Fixture) {
        let request: TransferStatusRequest = serde_json::from_slice(body).unwrap();
        request.validate().unwrap();
        if let Some((old, response)) = self.operations.get(&request.operation_id) {
            assert_eq!(old, body);
            respond(c, response, false);
            return;
        }
        let m = self.manifest.as_ref().unwrap().clone();
        assert_eq!(request.transfer_id, m.transfer_id);
        assert_eq!(
            self.status.as_ref().unwrap().manifest_offset,
            m.manifest_bytes
        );
        assert_eq!(self.status.as_ref().unwrap().bundle_offset, m.bundle_bytes);
        self.status.as_mut().unwrap().receipt = Some(import_receipt(fixture, m));
        let response = json!(self.status);
        self.operations
            .insert(request.operation_id, (body.to_vec(), response.clone()));
        self.finalize_lost = true;
        respond(c, &response, true);
    }
}
fn import_receipt(fixture: &Fixture, m: TransferManifest) -> CentralRetentionReceipt {
    let native: aq_worker::source::GitManifest = serde_json::from_slice(
        &std::fs::read(
            fixture
                .central
                .join("artifacts/incoming")
                .join(m.artifact_id.to_string())
                .join("manifest.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let imported = direct(
        &fixture.exe,
        &fixture.central,
        fixture.rid,
        "artifact.import",
        json!({"artifact_handle":m.artifact_id}),
        1,
    );
    assert_eq!(imported["source"], json!(m.checkpoint));
    CentralRetentionReceipt {
        receipt_id: Uuid::new_v4(),
        transfer_id: m.transfer_id,
        repository_id: fixture.rid,
        binding_generation: 1,
        source: m.checkpoint,
        retention_handle: m.artifact_id,
        manifest_digest: native.canonical_digest().unwrap(),
    }
}
