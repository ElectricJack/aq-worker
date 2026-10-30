//! Deliberately synthetic one-process harness. It executes no shell, providers,
//! graph writes or source operations. Real process-tree adapters are separate.
use crate::journal::ProcessEvidence;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus},
};
use uuid::Uuid;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureInput {
    output: String,
}
pub struct FixtureHandle {
    child: Child,
    launch_id: Uuid,
    directory: PathBuf,
}
impl FixtureHandle {
    pub fn spawn(executable: &Path, root: &Path, launch_id: Uuid, output: &str) -> Result<Self> {
        let directory = root.join("launches").join(launch_id.to_string());
        std::fs::create_dir_all(&directory)?;
        durable_write_new(
            &directory.join("input.json"),
            &serde_json::to_vec(&FixtureInput {
                output: output.into(),
            })?,
        )?;
        File::open(directory.parent().unwrap())?.sync_all()?;
        let child = Command::new(executable)
            .arg("--launch-dir")
            .arg(&directory)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .context("spawn deterministic fixture; launch intent remains uncertain")?;
        Ok(Self {
            child,
            launch_id,
            directory,
        })
    }
    pub fn launch_id(&self) -> Uuid {
        self.launch_id
    }
    pub fn evidence(&self) -> Result<ProcessEvidence> {
        process_evidence(self.child.id())
    }
    pub fn kill(&mut self) -> Result<()> {
        self.child.kill().context("stop owned fixture")
    }
    pub fn try_wait(&mut self) -> Result<Option<ExitStatus>> {
        Ok(self.child.try_wait()?)
    }
    pub fn wait(mut self) -> Result<(ExitStatus, String)> {
        let status = self.child.wait()?;
        let output = if status.success() {
            std::fs::read_to_string(self.directory.join("output.txt"))?
        } else {
            String::new()
        };
        Ok((status, output))
    }
}
/// Child execution owns a durable one-shot claim before writing output. Duplicate
/// invocation with the same launch identity exits without re-executing the work.
pub fn run_fixture(directory: &Path) -> Result<()> {
    durable_write_new(
        &directory.join("started.claim"),
        b"deterministic-agent-v1\n",
    )?;
    let input: FixtureInput =
        serde_json::from_slice(&std::fs::read(directory.join("input.json"))?)?;
    ensure!(
        input.output.len() <= aq_worker_protocol::MAX_TEXT,
        "fixture input too large"
    );
    durable_write_new(
        &directory.join("process.json"),
        &serde_json::to_vec(&process_evidence(std::process::id())?)?,
    )?;
    durable_write_new(&directory.join("output.txt"), input.output.as_bytes())?;
    Ok(())
}
pub fn process_evidence(pid: u32) -> Result<ProcessEvidence> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let fields: Vec<_> = stat
        .rsplit_once(") ")
        .context("malformed process stat")?
        .1
        .split_whitespace()
        .collect();
    let start_ticks = fields.get(19).context("missing starttime")?.parse()?;
    Ok(ProcessEvidence { pid, start_ticks })
}
fn durable_write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    File::open(path.parent().unwrap())?.sync_all()?;
    Ok(())
}
