use std::{path::Path, process::Command};

fn python_case(name: &str) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let status = Command::new("python3")
        .arg(root.join("scripts/tests/test_template_tool.py"))
        .arg(format!("TemplateTests.{name}"))
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .status()
        .expect("Python 3 required for template packaging tests");
    assert!(status.success(), "template acceptance case: {name}");
}

#[test]
fn finalize_refuses_template_missing_tmux_or_users() {
    python_case("test_finalize_refuses_template_missing_tmux_or_users");
}

#[test]
fn credential_scan_fails_build_with_auth_json() {
    python_case("test_credential_scan_fails_build_with_auth_json");
}

#[test]
fn template_cli_build_and_scan_use_embedded_tools() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let status = Command::new("python3")
        .arg(root.join("scripts/tests/test_template_tool.py"))
        .arg("BuildTests.test_cli_build_uses_embedded_tools_without_source_tree")
        .env("AQ_TEMPLATE_BINARY", env!("CARGO_BIN_EXE_aq-worker"))
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .status()
        .unwrap();
    assert!(status.success());
}
