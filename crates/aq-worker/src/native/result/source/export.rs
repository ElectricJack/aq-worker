use super::*;
use crate::source_transfer::{ArtifactAccess, artifact_file};

pub(super) fn verify_export(
    root: &Path,
    value: &serde_json::Value,
    h: &ResultHistory,
) -> Result<ExportRecord> {
    let artifact: Uuid = serde_json::from_value(value["artifact_handle"].clone())?;
    let manifest: GitManifest = serde_json::from_value(value["manifest"].clone())?;
    ensure!(
        !artifact.is_nil()
            && manifest.transfer_id == artifact
            && manifest.format_version == 1
            && manifest.object_format == "sha1"
            && crate::source_transfer::matches_source_binding(&manifest, &h.scope.source)
            && manifest.source
                == h.source
                    .checkpoint
                    .as_ref()
                    .context("result checkpoint absent")?
                    .source
            && valid_export_profile(&manifest),
        "result actual Git export manifest differs"
    );
    let (bytes, size) = exact_manifest(root, artifact, &manifest)?;
    let total = size
        .checked_add(manifest.bundle_size)
        .context("result export bytes overflow")?;
    ensure!(
        total <= h.scope.artifact_limit_bytes,
        "result export exceeds original disk reservation"
    );
    verify_bundle(root, artifact, &manifest)?;
    Ok(ExportRecord {
        artifact,
        manifest,
        manifest_digest: format!("{:x}", Sha256::digest(&bytes)),
        manifest_bytes: size,
        total_bytes: total,
    })
}

fn valid_export_profile(manifest: &GitManifest) -> bool {
    !manifest.producer_installation_id.is_nil()
        && manifest.reference_only == (manifest.bundle_size == 0)
        && manifest.prerequisites.iter().all(|p| p == &manifest.base)
}
fn exact_manifest(root: &Path, artifact: Uuid, manifest: &GitManifest) -> Result<(Vec<u8>, u64)> {
    let mut file = artifact_file(root, artifact, "manifest.json", ArtifactAccess::Retained)?;
    let size = file.metadata()?.len();
    ensure!(
        (1..=1_048_576).contains(&size),
        "result export manifest bound"
    );
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(1_048_577)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 == size
            && serde_json::to_value(serde_json::from_slice::<GitManifest>(&bytes)?)?
                == serde_json::to_value(manifest)?,
        "result exact exported manifest bytes differ"
    );
    Ok((bytes, size))
}
fn verify_bundle(root: &Path, artifact: Uuid, manifest: &GitManifest) -> Result<()> {
    if manifest.bundle_size > 0 {
        let mut bundle = artifact_file(root, artifact, "objects.bundle", ArtifactAccess::Retained)?;
        ensure!(
            bundle.metadata()?.len() == manifest.bundle_size,
            "result bundle length changed"
        );
        let mut hash = Sha256::new();
        let mut buffer = [0u8; 65536];
        let mut read = 0u64;
        loop {
            let n = bundle.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            read = read
                .checked_add(n as u64)
                .context("result bundle overflow")?;
            ensure!(read <= manifest.bundle_size, "result bundle grew");
            hash.update(&buffer[..n]);
        }
        ensure!(
            read == manifest.bundle_size
                && manifest.bundle_sha256.as_deref()
                    == Some(format!("{:x}", hash.finalize()).as_str()),
            "result actual bundle hash differs"
        );
    } else {
        ensure!(
            manifest.bundle_sha256.is_none(),
            "reference-only bundle has digest"
        );
    }
    Ok(())
}
