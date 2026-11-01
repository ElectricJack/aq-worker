//! Closed Codex exec input mapping. Preparing inputs grants no execution or
//! runtime qualification. The concrete host owner must verify them at exec.
use super::*;
use anyhow::Context;
use aq_admission::startup::NativeOffer;
use aq_session_host::LocalCommand;
use aq_session_protocol::{HarnessSettings, ModelSettings};
use std::{collections::BTreeMap, io::Write, path::Path};

pub const CODEX_CLI_VERSION: &str = "0.166.0";
const MAX_EVENT: usize = 32768;

/// Operator-selected exact settings, not settings inferred from an offer.
pub struct CodexExecProfile {
    pub harness: HarnessSettings,
    pub model: ModelSettings,
    pub permissions_digest: Digest,
    pub renderer: ArtifactRef,
}

/// Exact retained inputs for the native-specific mapper. This owns no lease.
pub struct CodexExecInput<'a> {
    pub offer: &'a NativeOffer,
    pub expected_manifest: &'a Digest,
    pub payload: Vec<u8>,
    pub workspace: &'a Path,
    pub private_home: &'a Path,
}
/// No Serialize or Debug: payload and derived authentication are sensitive.
/// No spawn/exec method: this is input preparation, not launch authority.
pub struct PreparedCodexExec {
    command: LocalCommand,
    startup: Zeroizing<Vec<u8>>,
    manifest_digest: Digest,
}
impl CodexExecProfile {
    pub fn prepare(
        &self,
        installed: &mut InstalledAdapter,
        offer: &NativeOffer,
        expected_manifest: &Digest,
        payload: Vec<u8>,
        workspace: &Path,
        private_home: &Path,
    ) -> Result<PreparedCodexExec> {
        let prepared = self.prepare_inputs(
            CodexExecInput {
                offer,
                expected_manifest,
                payload,
                workspace,
                private_home,
            },
            &installed.files[0].pin.path,
            &installed.adapter,
            &installed.renderer,
        )?;
        installed.revalidate()?;
        Ok(prepared)
    }
    /// Reuses the independently admitted immutable executable image; no full
    /// 319MiB executable rehash on an attempt's original execution clock.
    pub fn prepare_native(
        &self,
        installation: &mut super::native_exec::NativeExecInstallation,
        input: CodexExecInput<'_>,
    ) -> Result<PreparedCodexExec> {
        let m = installation.installation();
        let prepared = self.prepare_inputs(input, &m.executable.path, &m.adapter, &m.renderer)?;
        installation.revalidate()?;
        Ok(prepared)
    }
    fn prepare_inputs(
        &self,
        input: CodexExecInput<'_>,
        executable: &Path,
        adapter: &ArtifactRef,
        renderer: &ArtifactRef,
    ) -> Result<PreparedCodexExec> {
        let CodexExecInput {
            offer,
            expected_manifest,
            payload,
            workspace,
            private_home,
        } = input;
        offer.validate()?;
        let manifest = &offer.offered.offer.manifest;
        let descriptor = &offer.startup;
        ensure!(
            super::super::journal::canonical_digest(manifest)? == *expected_manifest
                && manifest.harness == self.harness
                && *adapter == self.harness.adapter
                && *renderer == self.renderer
                && manifest.model == self.model
                && super::super::journal::canonical_digest(&manifest.permissions)?
                    == self.permissions_digest,
            "Codex exact installed settings mismatch"
        );
        ensure!(
            self.harness.harness.as_str() == "codex"
                && self.harness.cli_version.as_str() == CODEX_CLI_VERSION
                && self.model.provider.as_str() == "openai"
                && self.model.spending_budget.is_none()
                && matches!(self.model.reasoning.as_str(), "low" | "medium" | "high")
                && !manifest.terminal.input_enabled,
            "Codex exec unsupported settings"
        );
        for cap in &self.harness.required_capabilities {
            ensure!(
                matches!(
                    cap.id.as_str(),
                    "aq.profiled-launch"
                        | "aq.terminal"
                        | "aq.harness.startup-context"
                        | "aq.harness.confined-worktree"
                        | "aq.harness.stop"
                ) && cap.version == 1,
                "Codex exec unsupported mandatory capability"
            );
        }
        descriptor.validate()?;
        descriptor.verify_payload(&payload)?;
        ensure!(
            descriptor.renderer_qualification == self.renderer
                && manifest.modules.contains(&descriptor.artifact()?)
                && manifest
                    .modules
                    .iter()
                    .filter(|a| a.id.as_str().starts_with("aq.startup-delivery/"))
                    .count()
                    == 1
                && payload.len() as u64 <= manifest.context.startup_byte_budget,
            "Codex exact startup descriptor mismatch"
        );
        ensure!(
            workspace.is_absolute()
                && workspace.is_dir()
                && std::fs::canonicalize(workspace)? == workspace
                && private_home.is_absolute()
                && private_home.is_dir()
                && std::fs::canonicalize(private_home)? == private_home
                && private_home != workspace
                && !private_home.starts_with(workspace),
            "Codex private configuration/workspace paths"
        );
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::symlink_metadata(private_home)?;
        ensure!(
            meta.is_dir()
                && meta.uid() == unsafe { libc::geteuid() }
                && meta.mode() & 0o777 == 0o700,
            "Codex private home ownership/mode"
        );
        ensure!(
            std::fs::read_dir(private_home)?.next().is_none(),
            "Codex private home must be fresh and empty"
        );
        let executable = executable.to_owned();
        let mut arguments = vec![
            "--no-daemon".into(),
            "exec".into(),
            "--ignore-user-config".into(),
            "--ignore-rules".into(),
            "--ephemeral".into(),
            "--json".into(),
            "--color".into(),
            "never".into(),
            "--sandbox".into(),
            "workspace-write".into(),
            "--model".into(),
            self.model.model.as_str().into(),
            "--cd".into(),
            workspace.to_str().context("Codex workspace UTF8")?.into(),
        ];
        let overrides = [
            format!(
                "model_reasoning_effort={}",
                toml::Value::String(self.model.reasoning.as_str().into())
            ),
            "approval_policy=\"never\"".into(),
            "project_doc_max_bytes=0".into(),
            "project_doc_fallback_filenames=[]".into(),
            "sandbox_workspace_write.writable_roots=[]".into(),
            "sandbox_workspace_write.exclude_slash_tmp=true".into(),
            "sandbox_workspace_write.exclude_tmpdir_env_var=true".into(),
            "sandbox_workspace_write.network_access=false".into(),
            "shell_environment_policy.inherit=\"none\"".into(),
            "shell_environment_policy.experimental_use_profile=false".into(),
            "agents.enabled=false".into(),
            format!(
                "projects.{}.trust_level=\"untrusted\"",
                toml::Value::String(workspace.to_str().unwrap().into())
            ),
        ];
        for value in overrides {
            arguments.push("--config".into());
            arguments.push(value);
        }
        arguments.push("-".into());
        Ok(PreparedCodexExec {
            command: LocalCommand {
                executable,
                arguments,
                environment: BTreeMap::from([(
                    "CODEX_HOME".into(),
                    private_home.to_str().context("Codex home UTF8")?.into(),
                )]),
            },
            startup: Zeroizing::new(payload),
            manifest_digest: expected_manifest.clone(),
        })
    }
}
impl PreparedCodexExec {
    pub fn command(&self) -> &LocalCommand {
        &self.command
    }
    pub fn manifest_digest(&self) -> &Digest {
        &self.manifest_digest
    }
    /// Exactly one payload; no argv prompt and no constituent concatenation.
    /// Consuming the owner prevents an implicit retry after an uncertain write.
    pub fn write_startup(self, writer: &mut impl Write) -> Result<()> {
        writer.write_all(&self.startup)?;
        writer.flush()?;
        Ok(())
    }
}

/// The future private configuration owner writes these bytes to owner-only
/// auth.json. Never place this returned value in LocalCommand or public records.
pub fn api_key_auth(
    material: &MaterialHandoff<'_>,
    manifest: &aq_session_protocol::LaunchManifest,
) -> Result<Zeroizing<Vec<u8>>> {
    manifest.validate()?;
    let lease = &material.installed.metadata().delivery.lease;
    ensure!(
        lease.binding.launch_manifest_digest == super::super::journal::canonical_digest(manifest)?
            && lease.provider.as_str() == "openai"
            && manifest
                .credentials
                .iter()
                .any(|r| r.credential_profile == lease.credential_profile
                    && r.purpose.as_str() == lease.purpose.as_str()),
        "Codex private material provider/profile binding"
    );
    auth_bytes(&material.read_private()?)
}
fn auth_bytes(key: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
    ensure!(
        !key.is_empty() && key.len() <= 4096 && key.iter().all(|b| b.is_ascii_graphic()),
        "Codex API key format"
    );
    let mut bytes = Zeroizing::new(Vec::with_capacity(key.len() + 32));
    bytes.extend_from_slice(b"{\"OPENAI_API_KEY\":");
    serde_json::to_writer(&mut *bytes, std::str::from_utf8(key)?)?;
    bytes.push(b'}');
    Ok(bytes)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CodexEvent {
    ThreadStarted(uuid::Uuid),
    TurnStarted(uuid::Uuid),
    TurnCompleted,
    Failed,
}
/// Bounded literal JSONL parser. Events are observations only; no start receipt,
/// model qualification, process identity or execution authority is produced.
#[derive(Default)]
pub struct CodexEvents {
    pending: Vec<u8>,
    thread: Option<uuid::Uuid>,
    turn: bool,
    failed: bool,
    finished: bool,
}
impl CodexEvents {
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<CodexEvent>> {
        if bytes.len() > MAX_EVENT {
            self.failed = true;
        }
        ensure!(
            !self.failed && bytes.len() <= MAX_EVENT,
            "Codex event reader refused"
        );
        let result = self.read(bytes);
        if result.is_err() {
            self.pending.clear();
            self.failed = true;
        }
        result
    }
    fn read(&mut self, bytes: &[u8]) -> Result<Vec<CodexEvent>> {
        let mut out = Vec::new();
        for byte in bytes {
            if *byte != b'\n' {
                ensure!(self.pending.len() < MAX_EVENT, "Codex event byte bound");
                self.pending.push(*byte);
                continue;
            }
            while self.pending.last() == Some(&b'\r') {
                self.pending.pop();
            }
            let value: serde_json::Value = serde_json::from_slice(&self.pending)?;
            self.pending.clear();
            match value.get("type").and_then(|v| v.as_str()) {
                Some("thread.started") => {
                    ensure!(
                        self.thread.is_none() && !self.turn,
                        "Codex duplicate thread event"
                    );
                    let id = uuid::Uuid::parse_str(
                        value
                            .get("thread_id")
                            .and_then(|v| v.as_str())
                            .context("Codex thread identity")?,
                    )?;
                    ensure!(!id.is_nil(), "Codex nil thread");
                    self.thread = Some(id);
                    out.push(CodexEvent::ThreadStarted(id));
                }
                Some("turn.started") => {
                    ensure!(!self.finished, "Codex extra turn after completion");
                    let id = self.thread.context("Codex turn before thread")?;
                    ensure!(!self.turn, "Codex duplicate turn event");
                    self.turn = true;
                    out.push(CodexEvent::TurnStarted(id));
                }
                Some("turn.completed") => {
                    ensure!(self.turn, "Codex completion before turn");
                    self.turn = false;
                    self.finished = true;
                    out.push(CodexEvent::TurnCompleted);
                }
                Some("turn.failed" | "error") => {
                    self.failed = true;
                    out.push(CodexEvent::Failed);
                }
                Some(kind) if kind.starts_with("item.") => {
                    ensure!(self.turn, "Codex item outside turn");
                }
                _ => anyhow::bail!("Codex unsupported event"),
            }
            if self.failed {
                break;
            }
        }
        Ok(out)
    }
    pub fn finish(&mut self) -> Result<()> {
        ensure!(
            self.pending.is_empty() && !self.turn && !self.failed && self.finished,
            "Codex incomplete/failed event stream"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests;
