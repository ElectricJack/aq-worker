//! Standalone template packaging tools, embedded so installed workers need no checkout.
use std::{ffi::OsString, fs, process::Command};

pub fn run(args: &[OsString]) -> anyhow::Result<()> {
    let directory = tempfile::Builder::new().prefix("aq-template-").tempdir()?;
    macro_rules! tool {
        ($name:literal) => {
            fs::write(
                directory.path().join($name),
                include_bytes!(concat!("../../../packaging/sandbox/", $name)),
            )?
        };
    }
    tool!("template_tool.py");
    tool!("credential_scan.py");
    tool!("environment_recipe.py");
    tool!("boat_materialize.py");
    let status = Command::new("python3")
        .arg("-I")
        .arg(directory.path().join("template_tool.py"))
        .args(args)
        .status()?;
    anyhow::ensure!(status.success(), "template operation refused");
    Ok(())
}
