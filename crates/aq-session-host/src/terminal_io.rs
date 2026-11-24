//! Guarded, finite terminal effects. This module never launches a session and
//! deliberately provides no snapshot/capture or unknown-resolution operation.
use super::*;
use aq_session_protocol::{ControllerLease, InputDisposition, InputReceipt, TerminalIdentity};
use aq_terminal_relay_protocol::{Effect, Fence, FenceAck, ResizeReceipt};
use rand::RngCore;
use std::sync::Arc;
use zeroize::Zeroizing;

mod effects;
mod storage;
pub(crate) use effects::TerminalHost;
use storage::{decrypt, encrypt, key, load, private_bytes, private_json, protect, typed_digest};

pub const MAX_INPUT_RECORDS: usize = 1024;
pub const MAX_CONTROL_RECORDS: usize = 1024;
const MAX_REQUEST: u64 = 96 * 1024;
// Ciphertext uses a JSON byte array (at most4 encoded bytes per raw byte).
// Leave envelope space and reserve the longer Unknown outcome before Injecting.
const MAX_LEDGER_PLAIN: usize = 480 * 1024;
const STATE: &str = "terminal-state.json";
const BINDING: &str = "terminal-binding.json";

/// A trusted embedding must qualify its clock against the coordinator. There is
/// no default: unqualified clocks must not construct an input-capable bridge.
#[derive(Clone, Copy)]
pub struct QualifiedClock {
    skew_ms: u64,
}
impl QualifiedClock {
    pub fn maximum_skew_ms(&self) -> u64 {
        self.skew_ms
    }
    pub fn assume_bounded_skew(skew_ms: u64) -> Result<Self> {
        ensure!(skew_ms <= 1000, "clock uncertainty bounds");
        Ok(Self { skew_ms })
    }
    fn effect_deadline(&self, effect: &Effect, permit_deadline: u64) -> Result<u64> {
        Ok(match effect {
            Effect::Fence(v) => v
                .controller
                .as_ref()
                .map(|c| self.project(c.expires_at_ms))
                .transpose()?
                .unwrap_or(permit_deadline),
            Effect::Input(v) => self.project(v.deadline_ms)?,
            Effect::Resize(v) => self.project(v.deadline_ms)?,
            _ => bail!("terminal operation unsupported; no snapshot qualification"),
        })
    }
    fn project(&self, deadline: u64) -> Result<u64> {
        let remaining = deadline
            .checked_sub(wall_ms()?)
            .context("terminal deadline elapsed")?
            .checked_sub(self.skew_ms)
            .context("terminal deadline uncertainty")?;
        ensure!(
            remaining > 0 && remaining <= 15000,
            "terminal deadline bounds"
        );
        ownership::raw_boot_ms()
            .checked_add(remaining)
            .context("terminal boot deadline overflow")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalBinding {
    pub terminal: TerminalIdentity,
    pub guardian: GuardianIdentity,
    pub pane: PaneIdentity,
    pub directory: DirectoryIdentity,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectoryIdentity {
    pub device: u64,
    pub inode: u64,
}
pub(crate) struct DirectoryGuard {
    file: File,
    path: PathBuf,
    identity: DirectoryIdentity,
}
impl DirectoryGuard {
    pub(crate) fn open(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)?;
        let m = file.metadata()?;
        ensure!(
            m.is_dir() && m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o777 == 0o700,
            "unsafe host directory"
        );
        let guard = Self {
            file,
            path: path.into(),
            identity: DirectoryIdentity {
                device: m.dev(),
                inode: m.ino(),
            },
        };
        guard.verify()?;
        Ok(guard)
    }
    pub(crate) fn existing(path: &Path) -> Result<Self> {
        let guard = Self::open(path)?;
        let anchor: DirectoryIdentity = private_json(&path.join("directory.json"), 1024)?;
        ensure!(anchor == guard.identity, "host directory anchor changed");
        guard.verify()?;
        Ok(guard)
    }
    pub(crate) fn anchor(&self) -> Result<()> {
        self.verify()?;
        durable_new(
            &self.path.join("directory.json"),
            &serde_json::to_vec(&self.identity)?,
        )?;
        self.verify()
    }
    pub(crate) fn verify(&self) -> Result<()> {
        let m = std::fs::symlink_metadata(&self.path)?;
        let held = self.file.metadata()?;
        ensure!(
            m.is_dir()
                && m.uid() == unsafe { libc::geteuid() }
                && m.mode() & 0o777 == 0o700
                && DirectoryIdentity {
                    device: m.dev(),
                    inode: m.ino()
                } == self.identity
                && DirectoryIdentity {
                    device: held.dev(),
                    inode: held.ino()
                } == self.identity
                && held.nlink() > 0,
            "host directory replaced"
        );
        Ok(())
    }
}
impl TerminalBinding {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.directory.inode > 0, "terminal directory identity");
        self.terminal.validate()?;
        self.guardian.intent.session.validate()?;
        ensure!(
            self.terminal.session == self.guardian.intent.session
                && !self.guardian.intent.launch_id.is_nil()
                && !self.guardian.nonce.is_nil(),
            "terminal binding identities"
        );
        ensure!(
            self.valid_guardian() && self.valid_pane(),
            "terminal physical identity bounds"
        );
        for process in [&self.guardian.process, &self.pane.server, &self.pane.pane] {
            ensure!(
                process.pid > 1 && process.start_ticks > 0,
                "terminal process evidence bounds"
            );
        }
        ensure!(
            self.guardian.process.pid != self.pane.server.pid
                && self.pane.server.pid != self.pane.pane.pid,
            "terminal process ancestry identities"
        );
        Ok(())
    }
    fn valid_guardian(&self) -> bool {
        let guardian = &self.guardian;
        Uuid::parse_str(&guardian.machine_boot_id).is_ok_and(|id| !id.is_nil())
            && guardian.pid_namespace.len() <= 128
            && guardian.pid_namespace.starts_with("pid:[")
            && guardian.pid_namespace.ends_with(']')
    }
    fn valid_pane(&self) -> bool {
        self.pane.session_name == "aq"
            && self.pane.pane_id.len() <= 32
            && self
                .pane
                .pane_id
                .strip_prefix('%')
                .is_some_and(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "body",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Receipt {
    Fence(FenceAck),
    Input(InputReceipt),
    Resize(ResizeReceipt),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    digest: Digest,
    receipt: Receipt,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    revision: u64,
    output_claim: Option<Uuid>,
    binding: TerminalBinding,
    fence: Option<Fence>,
    controller_deadline_boot_ms: u64,
    resize_sequence: u64,
    input: BTreeMap<Uuid, Record>,
    control: BTreeMap<Uuid, Record>,
}
impl State {
    fn record(&self, id: Uuid) -> Option<&Record> {
        self.input.get(&id).or_else(|| self.control.get(&id))
    }
    fn unknown(&self) -> bool {
        self.input.values().any(|r| matches!(&r.receipt,
        Receipt::Input(v) if matches!(v.disposition, InputDisposition::Injecting | InputDisposition::DeliveryUnknown)))
    }
    fn save(&mut self, dir: &Path) -> Result<()> {
        self.revision = self
            .revision
            .checked_add(1)
            .context("terminal ledger revision overflow")?;
        ensure!(
            self.input.len() <= MAX_INPUT_RECORDS && self.control.len() <= MAX_CONTROL_RECORDS,
            "terminal receipt capacity"
        );
        let mut worst = self.clone();
        let mut injecting = false;
        for record in worst.input.values_mut() {
            if let Receipt::Input(receipt) = &mut record.receipt
                && receipt.disposition == InputDisposition::Injecting
            {
                receipt.disposition = InputDisposition::DeliveryUnknown;
                injecting = true;
            }
        }
        ensure!(
            serde_json::to_vec(&worst)?.len() <= MAX_LEDGER_PLAIN - if injecting { 32 } else { 0 },
            "terminal ledger plaintext byte capacity"
        );
        let bytes = serde_json::to_vec(self)?;
        ensure!(
            bytes.len() as u64 <= MAX_LOCAL_FILE,
            "terminal receipt bytes"
        );
        durable_replace(
            &dir.join(STATE),
            &protect(dir, &self.binding, b"state", &bytes)?,
        )
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    binding: TerminalBinding,
    effect: Effect,
    deadline_boot_ms: u64,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ciphertext {
    version: u32,
    nonce: [u8; 12],
    bytes: Vec<u8>,
}

/// Opaque local capability tied to an existing immutable host, never a socket
/// selector. Submission returns only after mailbox publication is durable.
#[derive(Clone)]
pub struct TerminalHandle {
    directory: PathBuf,
    binding: TerminalBinding,
    directory_guard: Arc<DirectoryGuard>,
    pending_observation: bool,
}
impl SessionHandle {
    pub fn bind_terminal(&self, terminal: &TerminalIdentity) -> Result<TerminalHandle> {
        self.bind_terminal_inner(terminal, false)
    }
    pub(crate) fn bind_terminal_inner(
        &self,
        terminal: &TerminalIdentity,
        pending_observation: bool,
    ) -> Result<TerminalHandle> {
        terminal.validate()?;
        let _lock = RenewalLock::acquire(&self.directory.join("terminal.lock"))?;
        let directory_guard = Arc::new(DirectoryGuard::existing(&self.directory)?);
        let observation = self.observe()?;
        let config: SealedLaunch = read_json(&self.directory.join("launch.json"))?;
        ensure!(
            config.intent()? == self.identity.intent,
            "terminal sealed launch changed"
        );
        if pending_observation {
            let spec = config
                .observed
                .as_ref()
                .context("pending observer specification")?;
            spec.validate(&config)?;
            ensure!(
                &spec.terminal == terminal,
                "pending observer exact terminal"
            );
        }
        let wall_deadline = config.wall_deadline()?;
        TerminalHost {
            dir: &self.directory,
            config: &config,
            host: &observation,
            deadline: wall_deadline,
        }
        .live_phase(observation.permit_deadline_boot_ms, pending_observation)?;
        ensure!(
            observation.phase == HostPhase::Running
                || pending_observation && observation.phase == HostPhase::Starting,
            "terminal binding requires running host"
        );
        ensure!(
            terminal.session == self.identity.intent.session,
            "terminal session mismatch"
        );
        let binding = TerminalBinding {
            terminal: terminal.clone(),
            guardian: self.identity.clone(),
            pane: observation.pane.context("terminal pane unavailable")?,
            directory: directory_guard.identity,
        };
        binding.validate()?;
        binding.install(&self.directory)?;
        Ok(TerminalHandle {
            directory: self.directory.clone(),
            binding,
            directory_guard,
            pending_observation,
        })
    }
}

impl TerminalBinding {
    fn install(&self, dir: &Path) -> Result<()> {
        let path = dir.join(BINDING);
        if path.exists() {
            let old: TerminalBinding = private_json(&path, MAX_REQUEST)?;
            ensure!(old == *self, "terminal binding replacement refused");
        } else {
            // Publish binding last: a partial installation grants no authority.
            let key_path = dir.join("terminal.key");
            if !key_path.exists() {
                let mut key = Zeroizing::new([0u8; 32]);
                rand::rngs::OsRng.fill_bytes(&mut *key);
                durable_new(&key_path, &*key)?;
            }
            let _ = key(dir)?;
            let mut state = State {
                revision: 0,
                output_claim: None,
                binding: self.clone(),
                fence: None,
                controller_deadline_boot_ms: 0,
                resize_sequence: 0,
                input: BTreeMap::new(),
                control: BTreeMap::new(),
            };
            let state_path = dir.join(STATE);
            if state_path.exists() {
                let old = load(dir, self)?;
                ensure!(
                    old.binding == *self
                        && old.fence.is_none()
                        && old.input.is_empty()
                        && old.control.is_empty(),
                    "partial terminal binding mismatch"
                );
            } else {
                state.save(dir)?;
            }
            durable_new(&path, &serde_json::to_vec(self)?)?;
        }
        Ok(())
    }
}
impl TerminalHandle {
    /// Current visible plaintext from the exact owned pane. This is for local
    /// harness readiness/auth-dialog classification, not a renderer Snapshot or
    /// permission to resolve unknown input. No scrollback or caller tmux argv.
    pub fn current_viewport(&self) -> Result<String> {
        self.directory_guard.verify()?;
        let _lock = RenewalLock::acquire(&self.directory.join("terminal.lock"))?;
        let state = load(&self.directory, &self.binding)?;
        ensure!(
            state.binding == self.binding,
            "terminal viewport binding changed"
        );
        let host = SessionHandle {
            directory: self.directory.clone(),
            identity: self.binding.guardian.clone(),
            private_source: None,
        };
        let observation = host.observe()?;
        ensure!(
            observation.phase == HostPhase::Running
                && observation.pane.as_ref() == Some(&self.binding.pane),
            "terminal viewport requires original running pane"
        );
        let config: SealedLaunch = read_json(&self.directory.join("launch.json"))?;
        let output = TerminalHost {
            dir: &self.directory,
            config: &config,
            host: &observation,
            deadline: config.wall_deadline()?,
        }
        .guarded_tmux(
            observation.permit_deadline_boot_ms,
            &["capture-pane", "-p", "-t", &self.binding.pane.pane_id],
        )?;
        ensure!(output.len() <= 65_536, "terminal viewport byte bound");
        self.directory_guard.verify()?;
        String::from_utf8(output).context("terminal viewport UTF8")
    }
    pub(crate) fn output_settings(
        &self,
    ) -> Result<(PathBuf, PathBuf, aq_session_protocol::TerminalPolicy)> {
        self.directory_guard.verify()?;
        let config: SealedLaunch = read_json(&self.directory.join("launch.json"))?;
        ensure!(
            config.intent()? == self.binding.guardian.intent,
            "output sealed configuration changed"
        );
        let observation: HostObservation = read_json(&self.directory.join("state.json"))?;
        ensure!(
            observation.guardian == self.binding.guardian
                && observation.pane.as_ref() == Some(&self.binding.pane),
            "output physical identity changed"
        );
        if self.pending_observation {
            let spec = config
                .observed
                .as_ref()
                .context("pending output specification")?;
            spec.validate(&config)?;
            ensure!(
                spec.terminal == self.binding.terminal,
                "pending output terminal mismatch"
            );
        }
        TerminalHost {
            dir: &self.directory,
            config: &config,
            host: &observation,
            deadline: config.wall_deadline()?,
        }
        .live_phase(
            observation.permit_deadline_boot_ms,
            self.pending_observation,
        )?;
        self.directory_guard.verify()?;
        Ok((
            self.directory.clone(),
            config.tmux,
            config.manifest.terminal().clone(),
        ))
    }
    pub(crate) fn claim_output(&self) -> Result<Uuid> {
        let _lock = RenewalLock::acquire(&self.directory.join("terminal.lock"))?;
        self.output_settings()?;
        let mut state = load(&self.directory, &self.binding)?;
        ensure!(
            state.output_claim.is_none(),
            "output producer already claimed; no implicit restart"
        );
        let claim = Uuid::new_v4();
        state.output_claim = Some(claim);
        state.save(&self.directory)?;
        durable_new(
            &self.directory.join("terminal-output.claim"),
            &serde_json::to_vec(&self.binding)?,
        )?;
        self.directory_guard.verify()?;
        Ok(claim)
    }
    pub(crate) fn viewer_claim(&self) -> Result<Uuid> {
        self.output_settings()?;
        ensure!(!self.pending_observation, "viewer requires actual Running");
        let config: SealedLaunch = read_json(&self.directory.join("launch.json"))?;
        ensure!(
            config.observed.as_ref().is_some_and(|s| s.interactive())
                && config.manifest.terminal().input_enabled,
            "PTY viewer requires explicit interactive profile"
        );
        load(&self.directory, &self.binding)?
            .output_claim
            .context("viewer requires original control observer claim")
    }
    pub fn binding(&self) -> &TerminalBinding {
        &self.binding
    }
    pub fn recover(host: &SessionHandle, terminal: &TerminalIdentity) -> Result<Self> {
        let directory_guard = Arc::new(DirectoryGuard::existing(&host.directory)?);
        let binding: TerminalBinding = private_json(&host.directory.join(BINDING), MAX_REQUEST)?;
        ensure!(
            binding.guardian == host.identity
                && binding.terminal == *terminal
                && binding.directory == directory_guard.identity,
            "terminal recovery binding mismatch"
        );
        let state = load(&host.directory, &binding)?;
        ensure!(state.binding == binding, "terminal state binding mismatch");
        let _ = key(&host.directory)?;
        Ok(Self {
            directory: host.directory.clone(),
            binding,
            directory_guard,
            pending_observation: false,
        })
    }
    /// A duplicate returns historical metadata; it never authorizes another
    /// physical effect. Busy admission is explicit and has no implicit retry.
    pub fn submit(&self, effect: &Effect, clock: QualifiedClock) -> Result<Option<Receipt>> {
        self.directory_guard.verify()?;
        effect.validate()?;
        ensure!(
            effect.terminal() == &self.binding.terminal,
            "terminal effect identity mismatch"
        );
        let id = operation_id(effect)?;
        let digest = digest(effect)?;
        let _lock = RenewalLock::acquire(&self.directory.join("terminal.lock"))?;
        let state = load(&self.directory, &self.binding)?;
        ensure!(
            !self.directory.join("terminal.disabled").exists(),
            "terminal effects disabled"
        );
        if let Some(old) = state.record(id) {
            ensure!(old.digest == digest, "changed terminal operation payload");
            return Ok(Some(old.receipt.clone()));
        }
        let permit_deadline = self.admission_deadline(effect, &state)?;
        let deadline_boot_ms = clock.effect_deadline(effect, permit_deadline)?;
        let path = request_path(&self.directory, effect);
        if path.exists() {
            let old = decrypt(
                &self.directory,
                &self.binding,
                &private_bytes(&path, MAX_REQUEST)?,
            )?;
            ensure!(
                operation_id(&old.effect)? == id && digest == self::digest(&old.effect)?,
                "terminal mailbox busy or changed request"
            );
            return Ok(None);
        }
        let request = Request {
            binding: self.binding.clone(),
            effect: effect.clone(),
            deadline_boot_ms,
        };
        durable_new(&path, &encrypt(&self.directory, &request)?)?;
        self.directory_guard.verify()?;
        Ok(None)
    }
    fn admission_deadline(&self, effect: &Effect, state: &State) -> Result<u64> {
        let host = SessionHandle {
            directory: self.directory.clone(),
            identity: self.binding.guardian.clone(),
            private_source: None,
        };
        let observation = host.observe()?;
        ensure!(
            observation.phase == HostPhase::Running
                && observation.pane.as_ref() == Some(&self.binding.pane),
            "terminal host not running or pane changed"
        );
        let config: SealedLaunch = read_json(&self.directory.join("launch.json"))?;
        if let Effect::Input(input) = effect {
            ensure!(
                config.manifest.terminal().input_enabled
                    && input.bytes.len() <= config.manifest.terminal().max_input_bytes as usize
                    && input.bytes.len()
                        <= config.manifest.terminal().max_pending_input_bytes as usize,
                "sealed terminal input policy"
            );
            ensure!(
                state.input.len() < MAX_INPUT_RECORDS,
                "terminal input receipt capacity"
            );
        } else {
            ensure!(
                state.control.len() < MAX_CONTROL_RECORDS,
                "terminal control receipt capacity"
            );
        }
        Ok(observation.permit_deadline_boot_ms)
    }

    pub fn receipt(&self, id: Uuid) -> Result<Option<Receipt>> {
        self.directory_guard.verify()?;
        ensure!(!id.is_nil(), "terminal receipt ID");
        let _lock = RenewalLock::acquire(&self.directory.join("terminal.lock"))?;
        let mut state = load(&self.directory, &self.binding)?;
        if !supervisor::guardian_live(&self.binding.guardian)
            || self.directory.join("terminal.disabled").exists()
        {
            let mut changed = false;
            for record in state.input.values_mut() {
                if let Receipt::Input(v) = &mut record.receipt
                    && v.disposition == InputDisposition::Injecting
                {
                    v.disposition = InputDisposition::DeliveryUnknown;
                    changed = true;
                }
            }
            if changed {
                state.save(&self.directory)?;
            }
        }
        Ok(state.record(id).map(|r| r.receipt.clone()))
    }
    /// Recovery of a durably received network input whose plaintext was lost.
    /// Absence under the guardian's admission lock proves the physical boundary
    /// was never entered: cancel its mailbox and durably refuse it. An existing
    /// receipt is returned unchanged; this method never injects input.
    pub fn reject_unstarted_input(&self, id: Uuid, command_digest: &Digest) -> Result<Receipt> {
        self.directory_guard.verify()?;
        ensure!(!id.is_nil(), "terminal input recovery ID");
        let _lock = RenewalLock::acquire(&self.directory.join("terminal.lock"))?;
        let mut state = load(&self.directory, &self.binding)?;
        if let Some(old) = state.record(id) {
            ensure!(
                &old.digest == command_digest,
                "changed recovered input digest"
            );
            return Ok(old.receipt.clone());
        }
        ensure!(
            state.input.len() < MAX_INPUT_RECORDS,
            "terminal input receipt capacity"
        );
        let path = self.directory.join("terminal-effect.request");
        if path.exists() {
            let pending = decrypt(
                &self.directory,
                &self.binding,
                &private_bytes(&path, MAX_REQUEST)?,
            )?;
            if operation_id(&pending.effect)? == id {
                ensure!(
                    matches!(pending.effect, Effect::Input(_))
                        && &digest(&pending.effect)? == command_digest,
                    "recovered input mailbox mismatch"
                );
                std::fs::remove_file(&path)?;
                File::open(&self.directory)?.sync_all()?;
            }
        }
        let receipt = Receipt::Input(InputReceipt {
            operation_id: id,
            command_digest: command_digest.clone(),
            terminal: self.binding.terminal.clone(),
            disposition: if self.directory.join("terminal.disabled").exists() {
                InputDisposition::DeliveryUnknown
            } else {
                InputDisposition::Rejected
            },
        });
        state.input.insert(
            id,
            Record {
                digest: command_digest.clone(),
                receipt: receipt.clone(),
            },
        );
        state.save(&self.directory)?;
        Ok(receipt)
    }
}

fn wall_ms() -> Result<u64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis()
        .try_into()
        .context("terminal wall clock overflow")
}
fn operation_id(effect: &Effect) -> Result<Uuid> {
    match effect {
        Effect::Fence(v) => Ok(v.fence_id),
        Effect::Input(v) => Ok(v.operation_id),
        Effect::Resize(v) => Ok(v.operation_id),
        _ => bail!("terminal operation unsupported"),
    }
}
fn digest(effect: &Effect) -> Result<Digest> {
    Ok(match effect {
        Effect::Fence(v) => typed_digest(v)?,
        Effect::Input(v) => canonical_digest(v)?,
        Effect::Resize(v) => canonical_digest(v)?,
        _ => bail!("terminal operation unsupported"),
    })
}
fn request_path(dir: &Path, effect: &Effect) -> PathBuf {
    dir.join(if matches!(effect, Effect::Fence(_)) {
        "terminal-fence.request"
    } else {
        "terminal-effect.request"
    })
}
