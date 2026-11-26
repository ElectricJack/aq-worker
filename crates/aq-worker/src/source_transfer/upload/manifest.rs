use super::*;

fn read_manifest(root: &Path, artifact: Uuid) -> Result<Vec<u8>> {
    let mut file = artifact_file(root, artifact, "manifest.json", ArtifactAccess::Retained)?;
    ensure!(
        file.metadata()?.len() <= 1024 * 1024,
        "artifact manifest bound"
    );
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 1024 * 1024, "artifact manifest bound");
    Ok(bytes)
}

pub(super) fn retained_manifest(
    record: &crate::journal::AttemptRecord,
    source: &crate::source::SourceState,
    git: &GitClient,
) -> Result<TransferManifest> {
    let exported = source.exported.as_ref().context("export required")?;
    let artifact: Uuid = serde_json::from_value(exported["artifact_handle"].clone())?;
    let bytes = read_manifest(git.root(), artifact)?;
    let m: GitManifest = serde_json::from_slice(&bytes)?;
    ensure!(
        serde_json::to_value(&m)? == exported["manifest"]
            && m.format_version == 1
            && m.transfer_id == artifact
            && matches_source_binding(&m, &source.binding)
            && m.source
                == source
                    .checkpoint
                    .as_ref()
                    .context("checkpoint required")?
                    .source
            && m.object_format == "sha1"
            && m.reference_only == (m.bundle_size == 0),
        "retained artifact manifest changed"
    );
    let manifest = TransferManifest {
        transfer_id: Uuid::new_v4(),
        role: SourceRole::Checkpoint,
        checkpoint: m.source,
        attempt: record.offer.attempt.clone(),
        resolved_spec_digest: record.offer.spec.resolved_spec_digest.clone(),
        source: source.binding.clone(),
        artifact_id: artifact,
        manifest_digest: format!("{:x}", Sha256::digest(&bytes)),
        bundle_digest: m.bundle_sha256,
        manifest_bytes: bytes.len() as u64,
        bundle_bytes: m.bundle_size,
    };
    manifest.validate()?;
    ensure!(
        manifest
            .manifest_bytes
            .checked_add(manifest.bundle_bytes)
            .is_some_and(|n| n <= record.pending_bytes),
        "upload exceeds reserved artifact bytes"
    );
    Ok(manifest)
}
