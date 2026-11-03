//! Exact-source collection and receipt-backed delivery, isolated from control.
use anyhow::{Context, Result, ensure};
use aq_observation::{lifecycle::*, source::SourceFile, spool::OverflowPolicy};
use aq_observation_engine::Source;
use aq_observation_redb_prototype::{CollectedFrame, Limits, Store};
use std::path::{Path, PathBuf};

pub const MAX_SPOOL_BYTES: u64 = 16 * 1024 * 1024;
pub const MAX_STORE_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_TRACKED: usize = 4096;

#[derive(Debug)]
struct SourceNotReady;
impl std::fmt::Display for SourceNotReady {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("launched transcript not ready")
    }
}
impl std::error::Error for SourceNotReady {}

/// Single owner on a telemetry thread. The store transaction commits source
/// consumption together with every fact (or the explicit gap that replaces it).
pub struct Collector {
    store: Store,
    file: Option<SourceFile>,
    source_id: uuid::Uuid,
}
impl Collector {
    pub fn open(
        root: &Path,
        source: Source,
        native_root: &Path,
        member: &str,
        authorized_roots: &[PathBuf],
    ) -> Result<Self> {
        Self::open_inner(root, source, native_root, member, authorized_roots, None)
    }
    pub fn open_for_owner(
        root: &Path,
        source: Source,
        native_root: &Path,
        member: &str,
        authorized_roots: &[PathBuf],
        owner_uid: u32,
    ) -> Result<Self> {
        Self::open_inner(
            root,
            source,
            native_root,
            member,
            authorized_roots,
            Some(owner_uid),
        )
    }
    fn open_inner(
        root: &Path,
        source: Source,
        native_root: &Path,
        member: &str,
        authorized_roots: &[PathBuf],
        owner_uid: Option<u32>,
    ) -> Result<Self> {
        source.validate()?;
        ensure!(
            native_root.is_absolute()
                && native_root.components().all(|c| !matches!(
                    c,
                    std::path::Component::ParentDir | std::path::Component::CurDir
                )),
            "absolute transcript root without traversal required"
        );
        ensure!(
            authorized_roots.iter().any(|root| root.is_absolute()
                && root.components().all(|c| !matches!(
                    c,
                    std::path::Component::ParentDir | std::path::Component::CurDir
                ))
                && native_root.starts_with(root)),
            "transcript outside launched session roots"
        );
        let SessionIdentity::Remote {
            node_id, boot_id, ..
        } = source.binding.session
        else {
            anyhow::bail!("sandbox observations require remote session")
        };
        let limits = Limits {
            node_id,
            boot_id,
            physical_bytes: MAX_STORE_FILE_BYTES,
            logical_bytes: MAX_SPOOL_BYTES,
            cache_bytes: 4 * 1024 * 1024,
            quick_repair: false,
        };
        let existing = match std::fs::symlink_metadata(root) {
            Ok(m) => {
                ensure!(
                    m.is_dir() && !m.file_type().is_symlink(),
                    "store root redirected"
                );
                Some(Store::open(root, limits.clone())?)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        let row = existing
            .as_ref()
            .map(|s| s.source_optional(source.source_id))
            .transpose()?
            .flatten();
        let permit = if let Some(row) = row {
            ensure!(
                serde_json::to_value(&row.source)? == serde_json::to_value(&source)?,
                "persisted source differs from launched session"
            );
            let reader = row.reader.context("persisted source has no file reader")?;
            ensure!(
                reader.permit.root == native_root
                    && reader.permit.member == member
                    && reader.permit.owner_uid == owner_uid,
                "persisted transcript path/owner differs from launched session"
            );
            reader.permit
        } else {
            if let Some(store) = &existing {
                ensure!(store.source_count()? == 0, "foreign source store");
            }
            ensure!(
                std::fs::canonicalize(native_root)? == native_root,
                "transcript root must be canonical without symlinks"
            );
            ensure!(
                authorized_roots
                    .iter()
                    .any(|root| std::fs::canonicalize(root).ok().as_deref()
                        == Some(root.as_path())
                        && native_root.starts_with(root)),
                "authorized transcript root redirected"
            );
            // Exact-member probing is the only discovery operation allowed here.
            let discovery = aq_observation::discovery::Discovery {
                source_id: source.source_id,
                root: native_root.into(),
                native_session_key: source.native_session_key.clone(),
                format: serde_json::from_value(serde_json::to_value(&source.format)?)?,
                exact_member: Some(member.into()),
                maximum_entries: 1,
                maximum_candidates: 1,
            };
            let resolution = if let Some(uid) = owner_uid {
                discovery.resolve_for_owner(uid)?
            } else {
                discovery.resolve()?
            };
            match resolution {
                aq_observation::discovery::Resolution::Found(permit) => permit,
                aq_observation::discovery::Resolution::NotReady
                | aq_observation::discovery::Resolution::Missing => {
                    return Err(SourceNotReady.into());
                }
                _ => anyhow::bail!("launched transcript native identity mismatched or unsupported"),
            }
        };
        let store = if let Some(store) = existing {
            store
        } else {
            Store::create(root, limits)?
        };
        if store.source_optional(source.source_id)?.is_none() {
            store.attach_file_engine(source.clone(), MAX_TRACKED, permit.clone())?;
        }
        // A lost/replaced source must not prevent replaying already committed
        // packets. No new handle/identity is ever substituted on reopen.
        let file = SourceFile::open(permit).ok();
        Ok(Self {
            store,
            file,
            source_id: source.source_id,
        })
    }
    pub fn collect(&self, observed_at_ms: u64) -> Result<CollectedFrame> {
        let file = self
            .file
            .as_ref()
            .context("persisted transcript unavailable; cursor retained")?;
        self.store.collect_file(
            self.source_id,
            file,
            observed_at_ms,
            OverflowPolicy::GapAndHold,
        )
    }
    pub fn prepare(&self) -> Result<Option<Packet>> {
        self.store.prepare()
    }
    pub fn acknowledge(&self, receipt: &Receipt) -> Result<()> {
        // Validate before opening a store write transaction: an untrusted bad
        // receipt must not poison the otherwise replayable durable packet.
        if let Some(packet) = self.prepare()? {
            receipt.validate_against(&packet)?;
        }
        self.store.acknowledge(receipt)
    }
    pub fn cursor(&self) -> Result<u64> {
        Ok(self.store.source(self.source_id)?.cursor)
    }
    pub fn audit(&self) -> Result<()> {
        self.store.audit()
    }
}

use super::channels::SandboxChannels;
use aq_session_host::{LaunchIntent, TranscriptSource};
use serde::{Deserialize, Serialize};
use std::{
    io::Write,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::watch;

/// This bounded state is coalesced and persisted only when it changes. It is
/// evidence of completeness, never execution authority or a task outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationStatus {
    Starting,
    SourceNotReady,
    Collecting,
    Uploading,
    Idle,
    SourceHeld,
    SourceUnavailable,
    DeliveryUnavailable,
    StoreUnavailable,
    StoppedIncomplete,
}

pub struct ObservationTask {
    stop: watch::Sender<bool>,
    pub status: watch::Receiver<ObservationStatus>,
    thread: Option<std::thread::JoinHandle<Result<()>>>,
}
impl ObservationTask {
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        root: PathBuf,
        source: Source,
        transcript: TranscriptSource,
        intent: &LaunchIntent,
        authorized_roots: Vec<PathBuf>,
        channels: SandboxChannels,
        owner_uid: u32,
    ) -> Result<Self> {
        // Profiled attempts carry the server-derived identity (whose catalog
        // entry the runner cannot re-check here); otherwise the launch ID.
        let identity = match &source.registered_session_key {
            Some(key) => *key == profiled_session_key(&source.binding)?,
            None => source.source_id == intent.launch_id,
        };
        ensure!(
            &transcript.launch == intent
                && source.binding.session == intent.session
                && identity
                && source.native_session_key == transcript.native_session_key
                && source.provider == transcript.provider
                && source.account_reference == transcript.account_reference,
            "transcript descriptor differs from exact launched session"
        );
        ensure!(
            format_id(&source.format) == transcript.format_id,
            "transcript format mismatch"
        );
        channels.check_observation_scope(&source.binding)?;
        let (stop, stopped) = watch::channel(false);
        let (status, receiver) = watch::channel(ObservationStatus::Starting);
        let runtime = tokio::runtime::Handle::current();
        let thread = std::thread::Builder::new()
            .name("aq-observations".into())
            .spawn(move || {
                let path = root.with_extension("status.json");
                let result = observation_loop(
                    &runtime,
                    &root,
                    source,
                    transcript,
                    authorized_roots,
                    channels,
                    owner_uid,
                    stopped,
                    &status,
                    &path,
                );
                if result.is_err() {
                    set_status(&status, &path, ObservationStatus::StoreUnavailable)?;
                }
                result
            })?;
        Ok(Self {
            stop,
            status: receiver,
            thread: Some(thread),
        })
    }
    /// Nonblocking: control never joins disk I/O or waits for a transcript HTTP request.
    pub fn stop(&self) {
        self.stop.send_replace(true);
    }
    /// For lifecycle owners/tests after control has finished, never on its path.
    pub async fn wait(mut self) -> Result<()> {
        let thread = self.thread.take().context("observation thread absent")?;
        tokio::task::spawn_blocking(move || {
            thread
                .join()
                .map_err(|_| anyhow::anyhow!("observation thread failed"))?
        })
        .await?
    }
}
impl Drop for ObservationTask {
    fn drop(&mut self) {
        self.stop();
    }
}

pub fn format_id(format: &aq_observation_engine::Format) -> &'static str {
    use aq_observation_engine::Format::*;
    match format {
        CodexRollout67f0994 => "codex-rollout-jsonl-67f0994",
        ClaudeV1Jsonl9a9b8e8 => "claude-jsonl-v1-reference-9a9b8e8",
        GeminiRecording6814e00 => "gemini-recording-jsonl-6814e00",
        OpenCodeSessionDto6c24ec9 => "opencode-public-session-dto-6c24ec9",
    }
}
/// Domain separator of aq-server `observations::catalog::source_id`.
pub const PROFILED_SOURCE_DOMAIN: &str = "aq.observation-source/v1";

/// Server-derived source ID for a profiled attempt: SHA-256 over the
/// sorted-key compact JSON array `[domain, attempt_id, attempt_generation,
/// catalog_entry]`, truncated to 16 bytes with RFC 9562 version 8 and variant
/// bits set. The server registers exactly this ID; any other is unregistered.
pub fn profiled_source_id(
    attempt_id: uuid::Uuid,
    attempt_generation: u64,
    catalog_entry: &ArtifactRef,
) -> Result<uuid::Uuid> {
    use sha2::Digest as _;
    let mut value = serde_json::json!([
        PROFILED_SOURCE_DOMAIN,
        attempt_id,
        attempt_generation,
        catalog_entry
    ]);
    value.sort_all_objects();
    let hash = sha2::Sha256::digest(serde_json::to_vec(&value)?);
    let mut id = [0; 16];
    id.copy_from_slice(&hash[..16]);
    id[6] = (id[6] & 0x0f) | 0x80;
    id[8] = (id[8] & 0x3f) | 0x80;
    Ok(uuid::Uuid::from_bytes(id))
}

/// Server-registered session key `aq.attempt/<attempt_id>/<generation>`.
pub fn profiled_session_key(binding: &Binding) -> Result<Text> {
    Ok(Text::new(format!(
        "aq.attempt/{}/{}",
        binding.attempt_id, binding.attempt_generation
    ))?)
}

/// `catalog_entry` is the server's observation catalog entry for the attempt's
/// selection. With it, events carry the server-derived source identity; without
/// it, the legacy launch-ID identity is kept (which the server does not register).
pub fn source_for_launch(
    frozen: &super::FrozenLaunch,
    descriptor: &TranscriptSource,
    catalog_entry: Option<&ArtifactRef>,
) -> Result<Source> {
    use aq_observation_engine::Format;
    let format = [
        Format::CodexRollout67f0994,
        Format::ClaudeV1Jsonl9a9b8e8,
        Format::GeminiRecording6814e00,
    ]
    .into_iter()
    .find(|f| format_id(f) == descriptor.format_id)
    .context("unsupported file transcript format")?;
    let b = &frozen.manifest.bindings;
    let binding = Binding {
        project_id: b.project_id,
        task_id: b.task_id,
        lineage_id: b.graph_id,
        attempt_id: b.attempt_id,
        attempt_generation: b.assignment_generation,
        session: b.session.clone(),
    };
    let (source_id, registered_session_key) = match catalog_entry {
        Some(entry) => (
            profiled_source_id(binding.attempt_id, binding.attempt_generation, entry)?,
            Some(profiled_session_key(&binding)?),
        ),
        None => (frozen.launch_id, None),
    };
    let source = Source {
        binding,
        source_id,
        generation: 1,
        // Native transcript identity; provenance uses the registered key.
        native_session_key: descriptor.native_session_key.clone(),
        format,
        provider: descriptor.provider.clone(),
        // The session host reports the selection's account reference.
        account_reference: descriptor.account_reference.clone(),
        registered_session_key,
    };
    source.validate()?;
    Ok(source)
}

fn set_status(
    sender: &watch::Sender<ObservationStatus>,
    path: &Path,
    status: ObservationStatus,
) -> Result<()> {
    if *sender.borrow() == status && path.exists() {
        return Ok(());
    }
    // Runner-private parent. A fixed small record replaces repeated state traffic.
    let parent = path.parent().context("status parent absent")?;
    super::private::check_directory(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.as_file()
        .set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    file.write_all(&serde_json::to_vec(&status)?)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|e| e.error)?;
    std::fs::File::open(parent)?.sync_all()?;
    sender.send_replace(status);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn observation_loop(
    runtime: &tokio::runtime::Handle,
    root: &Path,
    source: Source,
    transcript: TranscriptSource,
    roots: Vec<PathBuf>,
    channels: SandboxChannels,
    owner_uid: u32,
    mut stop: watch::Receiver<bool>,
    status: &watch::Sender<ObservationStatus>,
    status_path: &Path,
) -> Result<()> {
    set_status(status, status_path, ObservationStatus::Starting)?;
    let collector = loop {
        if *stop.borrow() {
            set_status(status, status_path, ObservationStatus::StoppedIncomplete)?;
            return Ok(());
        }
        match Collector::open_for_owner(
            root,
            source.clone(),
            &transcript.root,
            &transcript.member,
            &roots,
            owner_uid,
        ) {
            Ok(c) => break c,
            Err(e)
                if e.downcast_ref::<SourceNotReady>().is_some()
                    || e.downcast_ref::<std::io::Error>()
                        .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                set_status(status, status_path, ObservationStatus::SourceNotReady)?;
                runtime.block_on(pause(&mut stop));
            }
            Err(_) => {
                set_status(status, status_path, ObservationStatus::SourceUnavailable)?;
                return Ok(());
            }
        }
    };
    loop {
        if *stop.borrow() {
            set_status(status, status_path, ObservationStatus::StoppedIncomplete)?;
            return Ok(());
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)?
            .as_millis()
            .try_into()?;
        let collected = collector.collect(now);
        let source_status = match &collected {
            Ok(c) if c.blocked => ObservationStatus::SourceHeld,
            Ok(_) => ObservationStatus::Idle,
            Err(_) => ObservationStatus::SourceUnavailable,
        };
        if let Some(packet) = collector.prepare()? {
            set_status(status, status_path, ObservationStatus::Uploading)?;
            let delivered = runtime.block_on(async {
                tokio::select! { biased;
                    _ = stop.changed() => None,
                    result = channels.deliver_observations(&packet) => Some(result),
                }
            });
            let Some(delivered) = delivered else { continue };
            match delivered.and_then(|receipt| collector.acknowledge(&receipt)) {
                Ok(()) => set_status(status, status_path, source_status)?,
                Err(_) => {
                    set_status(status, status_path, ObservationStatus::DeliveryUnavailable)?;
                    runtime.block_on(pause(&mut stop));
                }
            }
        } else {
            set_status(status, status_path, source_status)?;
            if !collected.is_ok_and(|c| c.consumed_bytes > 0) {
                runtime.block_on(pause(&mut stop));
            }
        }
    }
}
async fn pause(stop: &mut watch::Receiver<bool>) {
    if *stop.borrow() {
        return;
    }
    tokio::select! {_ = stop.changed() => {}, _ = tokio::time::sleep(Duration::from_millis(250)) => {}}
}
