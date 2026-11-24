//! Standalone template packaging tools, embedded so installed workers need no checkout.
use std::{ffi::OsString, fs, process::Command};

pub fn run(args: &[OsString]) -> anyhow::Result<()> {
    let (_directory, mut command) = command(args)?;
    anyhow::ensure!(command.status()?.success(), "template operation refused");
    Ok(())
}

/// The embedded tool with `args`, and the directory it lives in, which must outlive the run.
pub fn command(args: &[OsString]) -> anyhow::Result<(tempfile::TempDir, Command)> {
    let dir = tempfile::Builder::new().prefix("aq-template-").tempdir()?;
    macro_rules! tools {
        ($($name:literal),*) => {$(
            fs::write(dir.path().join($name), include_bytes!(concat!("../../../packaging/sandbox/", $name)))?;
        )*};
    }
    tools! {"template_tool.py", "template_contract.py", "credential_scan.py", "environment_recipe.py", "boat_materialize.py"}
    let mut tool = Command::new("python3");
    tool.arg("-I").arg(dir.path().join("template_tool.py"));
    tool.args(args);
    Ok((dir, tool))
}
