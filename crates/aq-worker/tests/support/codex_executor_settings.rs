use super::{CodexSettings, NativeConfig};
use aq_session_protocol::{Digest, HarnessSettings, Text};
use std::os::unix::fs::PermissionsExt;
pub(super) fn settings(config: &NativeConfig) -> CodexSettings {
    let fixture: serde_json::Value = serde_json::from_slice(include_bytes!(
        "../../../../vendor/aq-admission-0.1.7/artifacts/control.fixture.json"
    ))
    .unwrap();
    let manifest = &fixture["native_offer"]["offered"]["offer"]["manifest"];
    let mut harness: HarnessSettings = serde_json::from_value(manifest["harness"].clone()).unwrap();
    harness.harness = Text::new("codex").unwrap();
    harness.cli_version = Text::new("0.166.0").unwrap();
    harness.adapter = config.adapter.clone();
    harness.required_capabilities.clear();
    let root = config.state_dir.join("host");
    std::fs::create_dir(&root).unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    CodexSettings {
        host_root: root,
        harness,
        permissions_digest: Some(Digest::of(b"permissions")),
        columns: 80,
        rows: 24,
    }
}
