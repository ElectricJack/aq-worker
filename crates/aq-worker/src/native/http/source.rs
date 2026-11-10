//! Native source API1 over the independently frozen worker0.2 manifest.
//! Journal the verified spec's exact source manifest and verify whole file digests on disk.
use super::*;
use anyhow::Context as _;
use aq_admission::{ObjectFormat, SourceAdmission};
use aq_session_protocol::Validate as _;
use aq_worker_protocol::{SourceRole, TransferFile, TransferManifest, Validate as _};

pub struct SourceChunk {
    pub manifest: TransferManifest,
    pub offset: u64,
    pub bytes: Vec<u8>,
}
pub(crate) fn version(response: &Response) -> Result<()> {
    ensure!(
        response
            .headers()
            .get_all("aq-source-api-version")
            .iter()
            .count()
            == 1
            && response
                .headers()
                .get("aq-source-api-version")
                .is_some_and(|v| v == "1")
            && response
                .headers()
                .get(header::CACHE_CONTROL)
                .is_some_and(|v| v == "no-store"),
        "native source response version/cache policy"
    );
    Ok(())
}
pub(crate) fn transfer(binding: &LaunchBinding, manifest: &TransferManifest) -> Result<()> {
    binding.validate()?;
    manifest.validate()?;
    let SessionIdentity::Remote { session_id, .. } = binding.session else {
        anyhow::bail!("native source worker binding");
    };
    ensure!(
        manifest.role == SourceRole::Base
            && manifest.bundle_bytes > 0
            && manifest.transfer_id == manifest.artifact_id
            && manifest.attempt.attempt_id == binding.attempt_id
            && manifest.attempt.session_id == session_id
            && manifest.attempt.generation == binding.assignment_generation
            && manifest.resolved_spec_digest == binding.execution_spec_digest.as_str()
            && manifest.source.base.hex.bytes().any(|b| b != b'0'),
        "native source assignment/profile mismatch"
    );
    Ok(())
}
impl NativeClient {
    /// A source GET, retried while it fails transiently (`fetch_retried`).
    async fn source_get(&self, path: &str) -> Result<Response> {
        let url = format!("{}/v1/worker/profiled/source/{path}", self.base);
        fetch_retried(self.fetch_deadline_ns, || self.source_get_once(&url)).await
    }
    async fn source_get_once(&self, url: &str) -> Result<Response> {
        let response = (self.bulk.get(url))
            .header(header::AUTHORIZATION, self.credential.clone())
            .header("aq-source-api-version", "1")
            .send()
            .await
            .map_err(|_| TransportFailed("native source transport failed"))?;
        if !response.status().is_success() {
            return Err(refused(response).await);
        }
        version(&response)?;
        Ok(response)
    }
    /// First discovery is read-only. It must be journaled before file transfer;
    /// retries supply the original manifest and may not replace its metadata.
    pub async fn source_status(
        &self,
        binding: &LaunchBinding,
        source: &SourceAdmission,
        previous: Option<&TransferManifest>,
    ) -> Result<TransferManifest> {
        Ok(self
            .source_status_with_range(binding, source, previous)
            .await?
            .0)
    }
    pub(crate) async fn source_status_with_range(
        &self,
        binding: &LaunchBinding,
        source: &SourceAdmission,
        previous: Option<&TransferManifest>,
    ) -> Result<(TransferManifest, u32)> {
        self.scope(binding)?;
        binding.validate()?;
        source.validate()?;
        ensure!(
            source.resolved_object.format == ObjectFormat::Sha1,
            "unsupported native source format"
        );
        if let Some(old) = previous {
            transfer(binding, old)?;
        }
        let _slot = self.bulk_slots.try_acquire()?;
        let response = self.source_get(&binding.attempt_id.to_string()).await?;
        ensure!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .is_some_and(|v| v == "application/json"),
            "native source status body type"
        );
        let maximum_range = response
            .headers()
            .get("aq-source-max-range")
            .map(|v| v.to_str().ok().and_then(|v| v.parse::<u32>().ok()))
            .unwrap_or(Some(262144))
            .context("invalid native source range capability")?;
        ensure!(
            matches!(maximum_range, 262144 | 1048576),
            "unsupported native source range capability"
        );
        let manifest: TransferManifest = serde_json::from_slice(&body(response, 8192).await?)
            .map_err(|_| anyhow::anyhow!("invalid native source status"))?;
        transfer(binding, &manifest)?;
        ensure!(
            manifest.source.repository_id == source.repository_id
                && manifest.source.binding_generation == source.repository_binding_generation
                && manifest.source.base.hex == source.resolved_object.hex
                && previous.is_none_or(|old| old == &manifest),
            "native source pins changed"
        );
        Ok((manifest, maximum_range))
    }
    pub async fn source_chunk(
        &self,
        binding: &LaunchBinding,
        expected: &TransferManifest,
        member: TransferFile,
        offset: u64,
        limit: u32,
    ) -> Result<SourceChunk> {
        self.scope(binding)?;
        transfer(binding, expected)?;
        let (name, size) = match member {
            TransferFile::Manifest => ("manifest", expected.manifest_bytes),
            TransferFile::Bundle => ("bundle", expected.bundle_bytes),
        };
        ensure!(
            (1..=1_048_576).contains(&limit) && offset < size,
            "native source range"
        );
        let count = (size - offset).min(u64::from(limit)) as usize;
        let _slot = self.bulk_slots.try_acquire()?;
        let response = self
            .source_get(&format!(
                "{}/{name}?offset={offset}&limit={limit}",
                binding.attempt_id
            ))
            .await?;
        body_type(&response)?;
        let manifest: TransferManifest = metadata(&response, "aq-source-transfer")?;
        transfer(binding, &manifest)?;
        ensure!(&manifest == expected, "native source transfer changed");
        let offsets: Vec<_> = response
            .headers()
            .get_all("aq-source-offset")
            .iter()
            .collect();
        ensure!(
            offsets.len() == 1
                && offsets[0].to_str().ok().and_then(|v| v.parse::<u64>().ok()) == Some(offset),
            "native source offset changed"
        );
        let bytes = body(response, count).await?;
        ensure!(
            bytes.len() == count,
            "native source short or oversized chunk"
        );
        Ok(SourceChunk {
            manifest,
            offset,
            bytes: bytes.to_vec(),
        })
    }
}
