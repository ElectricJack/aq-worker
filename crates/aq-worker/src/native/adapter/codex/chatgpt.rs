//! Native subscription login supplied through the existing exact material lease.
//! No shared Codex home, token logging, provider call, or credential persistence.
use anyhow::{Result, ensure};
use aq_session_protocol::{LaunchManifest, Validate};
use zeroize::Zeroizing;

use crate::native::adapter::MaterialHandoff;

pub(crate) fn validated_chatgpt_auth_json(
    material: &MaterialHandoff<'_>,
    manifest: &LaunchManifest,
) -> Result<Zeroizing<Vec<u8>>> {
    manifest.validate()?;
    let lease = &material.installed.metadata().delivery.lease;
    ensure!(
        lease.binding.launch_manifest_digest == crate::native::journal::canonical_digest(manifest)?
            && lease.provider.as_str() == "openai"
            && manifest.credentials.iter().any(|required| {
                required.credential_profile == lease.credential_profile
                    && required.purpose.as_str() == lease.purpose.as_str()
            }),
        "Codex login exact material provider/profile binding"
    );
    let auth = material.read_private()?;
    aq_session_host::PrivateLaunchInput::validate_chatgpt_auth_json(&auth)?;
    Ok(auth)
}
