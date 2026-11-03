//! Standalone template packaging tools, embedded so installed workers need no checkout.
use std::{ffi::OsString, fs, process::Command};

pub fn run(args: &[OsString]) -> anyhow::Result<()> {
    let directory = tempfile::Builder::new().prefix("aq-template-").tempdir()?;
    let tool = directory.path().join("template_tool.py");
    fs::write(
        &tool,
        include_bytes!("../../../packaging/sandbox/template_tool.py"),
    )?;
    fs::write(
        directory.path().join("credential_scan.py"),
        include_bytes!("../../../packaging/sandbox/credential_scan.py"),
    )?;
    let status = Command::new("python3")
        .arg("-I")
        .arg(tool)
        .args(args)
        .status()?;
    anyhow::ensure!(status.success(), "template operation refused");
    Ok(())
}
