//! Deterministic structural profile vectors, never owning runtime evidence.
#[path = "../tests/refresh_common.rs"]
mod common;
use aq_phase_terminal_protocol::refresh_v1 as profile;
fn main() {
    let out = std::path::PathBuf::from(std::env::args().nth(1).expect("output directory"));
    let check = std::env::args().any(|arg| arg == "--check");
    let values = [
        ("schema.json", profile::schema()),
        ("manifest.json", profile::contract_manifest().unwrap()),
        (
            "artifact.json",
            serde_json::to_value(profile::contract_artifact().unwrap()).unwrap(),
        ),
        ("fixtures.json", common::fixtures()),
        ("invalid.json", common::invalid_fixtures()),
    ];
    if !check {
        std::fs::create_dir_all(&out).unwrap();
    }
    for (name, value) in values {
        let bytes = format!("{}\n", serde_json::to_string_pretty(&value).unwrap());
        if check {
            assert_eq!(
                std::fs::read(out.join(name)).unwrap(),
                bytes.as_bytes(),
                "refresh terminal vector drift: {name}"
            );
        } else {
            std::fs::write(out.join(name), bytes).unwrap();
        }
    }
}
