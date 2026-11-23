//! Warm build caches for docker-local sandboxes, so an attempt does not compile cold.
//!
//! Per project and variant (sandbox template plus the project's toolchain files):
//!
//! ```text
//! <root>/<project>/lock                      serializes every change below
//! <root>/<project>/<variant>/gen/<id>/       one warm build of a source tree
//!     p<i>/         the build output of cache path i, read-only once `done/ok` exists
//!     done/         warm.sh, and `ok` written by the warm container when it succeeded
//!     src/          the tree the warm container builds (deleted once sealed)
//!     ledger.json   {path: [sha256, 0]} of that tree
//! <root>/<project>/<variant>/slot/<task>/    one task's build output, kept across attempts
//!     p<i>/upper, p<i>/work   overlay upper over gen/<id>/p<i>
//!     gen           the generation this slot overlays (empty: none)
//!     ledger.json   {path: [sha256, mtime]} as last uploaded to a sandbox
//!     lock          held while an attempt uses the slot; its mtime is the last use
//! ```
//!
//! A sandbox sees only its own project's directories: a generation as a
//! read-only overlay lower and its task's slot as the upper. Build tools judge
//! freshness by mtime, so the ledger gives a file the mtime it had when its
//! content was last uploaded and "now" when the content is new: a build reused
//! from main can never be taken for a build of changed sources.
use crate::tar::TreeEntry;
use anyhow::{Context, Result, bail, ensure};
use aq_handoff_note::now_ms;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    time::{Duration, UNIX_EPOCH},
};
use uuid::Uuid;

/// Paths the driver copies in or out, scans or mounts itself. A cache may not
/// overlap them, so copy-back and the secret scan never see a cache.
const RESERVED: [&str; 9] = [
    "/aq/work",
    "/aq/in",
    "/aq/out",
    "/aq/private",
    "/aq/channel",
    "/aq/home/projects",
    "/aq/home/codex",
    "/aq/src",
    "/aq/warm",
];

/// Every limit is configuration; none has a built-in value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    /// Bound on one project's caches on this node. Above it the least recently
    /// used idle slots go first, then generations no slot overlays.
    pub max_bytes: u64,
    /// A slot may not grow past this: the attempt using it is stopped, and the
    /// slot is deleted so the task starts from the generation again.
    pub slot_max_bytes: u64,
    /// How often a running attempt's slot is measured against `slot_max_bytes`.
    pub slot_watch_interval_ms: u64,
    /// An idle slot unused for this long is deleted.
    pub slot_max_age_ms: u64,
    /// A changed source tree starts a new generation only when the newest one is this old.
    pub refresh_interval_ms: u64,
    /// The warm container is stopped after this long; an unsealed generation older than it is deleted.
    pub warm_timeout_ms: u64,
    pub warm_cpu_millis: u64,
    pub warm_memory_bytes: u64,
    pub warm_pids: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CachePath {
    /// Absolute container path, outside the workspace (`/tmp/aq-target`).
    pub target: String,
    /// Environment variable set to `target` (`CARGO_TARGET_DIR`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectCache {
    pub paths: Vec<CachePath>,
    /// Shell lines run in `/aq/work` to build a generation. Empty: slots only.
    #[serde(default)]
    pub warm: Vec<String>,
    /// Workspace files that select the toolchain (`rust-toolchain.toml`). A
    /// change starts a new variant: no generation or slot is reused across it.
    #[serde(default)]
    pub toolchain_files: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Settings {
    root: PathBuf,
    limits: Limits,
    projects: BTreeMap<Uuid, ProjectCache>,
}

/// The node's build-cache settings; validated when read.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Settings", into = "Settings")]
pub struct BuildCacheConfig(Settings);
impl From<BuildCacheConfig> for Settings {
    fn from(config: BuildCacheConfig) -> Self {
        config.0
    }
}
impl TryFrom<Settings> for BuildCacheConfig {
    type Error = anyhow::Error;
    fn try_from(settings: Settings) -> Result<Self> {
        // A bind is `source:target:options`; neither may hold a separator.
        let plain = |text: &str| !text.contains([':', ',', '\n', '\0']);
        let normal = |path: &str| {
            path.starts_with('/')
                && path[1..]
                    .split('/')
                    .all(|c| !c.is_empty() && c != "." && c != "..")
        };
        let root = settings.root.to_str().context("build cache root UTF-8")?;
        ensure!(
            normal(root) && plain(root),
            "build cache root must be a plain absolute path"
        );
        let inside = |a: &str, b: &str| a == b || a.starts_with(&format!("{b}/"));
        for cache in settings.projects.values() {
            ensure!(
                !cache.paths.is_empty(),
                "a project build cache needs a path"
            );
            for (i, path) in cache.paths.iter().enumerate() {
                let target = path.target.as_str();
                ensure!(
                    normal(target) && plain(target) && target.matches('/').count() >= 2,
                    "build cache target {target:?} must be a plain absolute path below a top-level directory"
                );
                let others = cache.paths[..i].iter().map(|p| p.target.as_str());
                for other in RESERVED.into_iter().chain(others) {
                    ensure!(
                        !inside(target, other) && !inside(other, target),
                        "build cache target {target:?} overlaps {other:?}"
                    );
                }
                if let Some(env) = &path.env {
                    ensure!(
                        !env.is_empty()
                            && env
                                .bytes()
                                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
                            && !["PATH", "HOME"].contains(&env.as_str()),
                        "build cache environment name {env:?} refused"
                    );
                }
            }
            for file in &cache.toolchain_files {
                ensure!(
                    normal(&format!("/{file}")),
                    "toolchain file {file:?} must be a relative path"
                );
            }
        }
        Ok(Self(settings))
    }
}
impl BuildCacheConfig {
    /// The cache of one task of `project` in sandboxes of `template`, when the
    /// project has one. Nothing an offer says selects a directory but these ids.
    pub fn slot(&self, project: Uuid, task: Uuid, template: &str) -> Option<SlotCache> {
        Some(SlotCache {
            dir: self.0.root.join(project.hyphenated().to_string()),
            task,
            template: template.to_owned(),
            limits: self.0.limits.clone(),
            cache: self.0.projects.get(&project)?.clone(),
        })
    }

    /// Deletes a finished or cancelled task's slots (every variant) unless an
    /// attempt is using one. The node calls this when it learns the task ended.
    pub fn discard_task(&self, project: Uuid, task: Uuid) -> Result<()> {
        let dir = self.0.root.join(project.hyphenated().to_string());
        if !dir.exists() {
            return Ok(());
        }
        let _project = lock(&dir.join("lock"), true)?;
        for variant in subdirs(&dir)? {
            let slot = variant.join("slot").join(task.hyphenated().to_string());
            if slot.exists() && lock(&slot.join("lock"), false).is_ok() {
                fs::remove_dir_all(&slot)?;
            }
        }
        Ok(())
    }
}

/// One task's cache, as the launch hands it to the sandbox driver.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlotCache {
    dir: PathBuf,
    task: Uuid,
    template: String,
    limits: Limits,
    cache: ProjectCache,
}

type Ledger = BTreeMap<String, (String, u64)>;

fn hex(hash: Sha256) -> String {
    hash.finalize().iter().map(|b| format!("{b:02x}")).collect()
}
fn mkdir(path: &Path) -> Result<()> {
    Ok(fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)?)
}
/// An exclusive `flock`; without `wait`, an error when another process holds it.
fn lock(path: &Path, wait: bool) -> Result<fs::File> {
    use std::os::fd::AsRawFd;
    let file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let how = libc::LOCK_EX | if wait { 0 } else { libc::LOCK_NB };
    // SAFETY: `file` is an open descriptor for the whole call.
    ensure!(
        unsafe { libc::flock(file.as_raw_fd(), how) } == 0,
        "build cache {} is in use",
        path.display()
    );
    Ok(file)
}
fn subdirs(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    if dir.is_dir() {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                found.push(entry.path());
            }
        }
    }
    found.sort();
    Ok(found)
}
/// Disk blocks below `path`, in bytes; links are not followed.
fn disk_bytes(path: &Path) -> u64 {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return 0;
    };
    let own = meta.blocks() * 512;
    if !meta.is_dir() {
        return own;
    }
    let children = fs::read_dir(path).into_iter().flatten().flatten();
    own + children.map(|c| disk_bytes(&c.path())).sum::<u64>()
}
/// A build writes read-only directories (the cargo registry) that plain removal refuses.
fn remove(path: &Path) {
    fn writable(path: &Path) {
        if let Ok(meta) = fs::symlink_metadata(path)
            && meta.is_dir()
        {
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
            for child in fs::read_dir(path).into_iter().flatten().flatten() {
                writable(&child.path());
            }
        }
    }
    if fs::remove_dir_all(path).is_err() {
        writable(path);
        let _ = fs::remove_dir_all(path);
    }
}
fn name(path: &Path) -> &str {
    path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
}
fn sealed(generation: &Path) -> bool {
    generation.join("done/ok").exists()
}
/// A generation is named `<created ms, 13 digits>-<tree hash>`.
fn created_ms(generation: &Path) -> u64 {
    name(generation)
        .get(..13)
        .and_then(|ms| ms.parse().ok())
        .unwrap_or(0)
}
fn read_ledger(path: &Path) -> Ledger {
    let bytes = fs::read(path).unwrap_or_default();
    serde_json::from_slice(&bytes).unwrap_or_default()
}
fn write_ledger(path: &Path, ledger: &Ledger) -> Result<()> {
    let partial = path.with_extension("partial");
    fs::write(&partial, serde_json::to_vec(ledger)?)?;
    Ok(fs::rename(partial, path)?)
}

impl SlotCache {
    /// The attempt's environment's warm commands (its recipe's `## Warm`),
    /// used when the node config sets none: a set node `warm` still wins.
    pub fn warm_from_environment(&mut self, warm: Vec<String>) {
        if self.cache.warm.is_empty() {
            self.cache.warm = warm;
        }
    }

    /// `NAME=target` for every cache path that names a variable.
    pub fn environment(&self) -> impl Iterator<Item = (&str, &str)> {
        let paths = self.cache.paths.iter();
        paths.filter_map(|p| Some((p.env.as_deref()?, p.target.as_str())))
    }

    /// Template plus the content of the project's toolchain files in `workspace`.
    fn variant(&self, workspace: &Path) -> String {
        let mut hash = Sha256::new();
        for file in &self.cache.toolchain_files {
            hash.update(file.as_bytes());
            hash.update([0]);
            hash.update(fs::read(workspace.join(file)).unwrap_or_default());
            hash.update([0]);
        }
        let template: String = (self.template.chars())
            .filter(char::is_ascii_alphanumeric)
            .collect();
        format!("{template}-{}", &hex(hash)[..16])
    }

    /// Opens this task's slot for one attempt on `workspace`, enforcing the
    /// limits first. The slot stays locked until the result is dropped. `None`
    /// (logged) when it cannot be opened, for example while another attempt holds it.
    pub fn open(&self, workspace: &Path) -> Option<Opened> {
        let opened = self.open_at(workspace, now_ms());
        // A cache that cannot be opened costs a cold build, never the attempt.
        opened
            .inspect_err(|e| println!("aq-sandbox-harness: no build cache: {e:#}"))
            .ok()
    }
    fn open_at(&self, workspace: &Path, now: u64) -> Result<Opened> {
        mkdir(&self.dir)?;
        let _project = lock(&self.dir.join("lock"), true)?;
        let variant = self.dir.join(self.variant(workspace));
        let slot = variant
            .join("slot")
            .join(self.task.hyphenated().to_string());
        mkdir(&variant.join("gen"))?;
        mkdir(&variant.join("slot"))?;
        self.sweep(&variant, &slot, now)?;
        let fresh = !slot.exists();
        if fresh {
            mkdir(&slot)?;
            let generation = subdirs(&variant.join("gen"))?
                .into_iter()
                .rfind(|g| sealed(g));
            if let Some(generation) = &generation {
                fs::copy(generation.join("ledger.json"), slot.join("ledger.json"))?;
            }
            fs::write(slot.join("gen"), generation.as_deref().map_or("", name))?;
        }
        let held = lock(&slot.join("lock"), false)?;
        held.set_modified(UNIX_EPOCH + Duration::from_millis(now))?;
        let generation = fs::read_to_string(slot.join("gen"))?;
        let generation = (!generation.is_empty()).then(|| variant.join("gen").join(generation));
        for i in 0..self.cache.paths.len() {
            mkdir(&slot.join(format!("p{i}/upper")))?;
            mkdir(&slot.join(format!("p{i}/work")))?;
        }
        Ok(Opened {
            cache: self.clone(),
            variant,
            slot,
            generation,
            fresh,
            watched: now.into(),
            _held: held,
        })
    }

    /// Deletes what the limits no longer allow. Never an in-use slot, the slot
    /// being opened, a generation a slot overlays or one still being built.
    fn sweep(&self, current: &Path, opening: &Path, now: u64) -> Result<()> {
        let limits = &self.limits;
        let (mut slots, mut generations) = (Vec::new(), Vec::new());
        for variant in subdirs(&self.dir)? {
            for generation in subdirs(&variant.join("gen"))? {
                if sealed(&generation) {
                    remove(&generation.join("src"));
                    generations.push(generation);
                } else if now.saturating_sub(created_ms(&generation)) > limits.warm_timeout_ms {
                    remove(&generation);
                }
            }
            slots.extend(subdirs(&variant.join("slot"))?);
        }
        // (last use, path, bytes) of every slot, oldest first; `None` bytes: in use.
        let mut used = Vec::new();
        for slot in slots {
            let last = fs::metadata(slot.join("lock")).and_then(|m| m.modified());
            let last = last.ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok());
            let last = last.map_or(0, |d| d.as_millis() as u64);
            let bytes = disk_bytes(&slot);
            let idle = slot != opening && lock(&slot.join("lock"), false).is_ok();
            let overlaid = fs::read_to_string(slot.join("gen")).unwrap_or_default();
            let orphan = !overlaid.is_empty()
                && !slot
                    .parent()
                    .is_some_and(|s| s.with_file_name("gen").join(&overlaid).exists());
            if (idle || slot == opening)
                && (orphan
                    || bytes > limits.slot_max_bytes
                    || now.saturating_sub(last) > limits.slot_max_age_ms)
            {
                remove(&slot);
            } else {
                used.push((last, slot, idle.then_some(bytes), bytes));
            }
        }
        used.sort();
        let size = |generations: &[PathBuf]| generations.iter().map(|g| disk_bytes(g)).sum::<u64>();
        let mut total = size(&generations) + used.iter().map(|u| u.3).sum::<u64>();
        used.retain(|(_, slot, idle, bytes)| {
            let evict = total > limits.max_bytes && idle.is_some();
            if evict {
                remove(slot);
                total -= bytes;
            }
            !evict
        });
        let overlaid: BTreeSet<PathBuf> = used
            .iter()
            .filter_map(|(_, slot, ..)| {
                let generation = fs::read_to_string(slot.join("gen")).ok()?;
                Some(slot.parent()?.with_file_name("gen").join(generation))
            })
            .collect();
        let newest = subdirs(&current.join("gen"))?
            .into_iter()
            .rfind(|g| sealed(g));
        generations.sort_by_key(|g| created_ms(g));
        for generation in generations {
            if total > limits.max_bytes
                && !overlaid.contains(&generation)
                && Some(&generation) != newest.as_ref()
            {
                total = total.saturating_sub(disk_bytes(&generation));
                remove(&generation);
            }
        }
        Ok(())
    }
}

/// A slot locked for one attempt.
pub struct Opened {
    cache: SlotCache,
    variant: PathBuf,
    slot: PathBuf,
    generation: Option<PathBuf>,
    fresh: bool,
    /// When the slot was last measured (Unix ms).
    watched: std::sync::atomic::AtomicU64,
    _held: fs::File,
}
impl Drop for Opened {
    /// The driver may be killed before this runs; the next open's sweep does the same.
    fn drop(&mut self) {
        if disk_bytes(&self.slot) > self.cache.limits.slot_max_bytes {
            remove(&self.slot);
        }
    }
}

/// What `stamp` decided for one upload of the workspace.
pub struct Stamped {
    mtimes: BTreeMap<String, u64>,
    /// A generation to build from this tree, in its own container.
    pub warm: Option<WarmJob>,
}
impl Stamped {
    /// The mtime (Unix seconds) a workspace file gets in the sandbox.
    pub fn mtime(&self, path: &str) -> u64 {
        self.mtimes.get(path).copied().unwrap_or(0)
    }
}

impl Opened {
    /// Daemon binds: each cache path is the slot's own directory, over the
    /// generation as a read-only overlay lower when there is one (podman `:O`).
    pub fn binds(&self) -> Vec<String> {
        let paths = self.cache.cache.paths.iter().enumerate();
        paths
            .map(|(i, path)| {
                let slot = self.slot.join(format!("p{i}"));
                let (upper, work) = (slot.join("upper"), slot.join("work"));
                match &self.generation {
                    Some(generation) => format!(
                        "{}:{}:O,upperdir={},workdir={}",
                        generation.join(format!("p{i}")).display(),
                        path.target,
                        upper.display(),
                        work.display()
                    ),
                    None => format!("{}:{}", upper.display(), path.target),
                }
            })
            .collect()
    }

    /// The disk watchdog for the slot, which the daemon's own disk samples do
    /// not cover: the stop reason once the slot is larger than `slot_max_bytes`.
    /// Measures once per watch interval.
    pub fn exceeded(&self) -> Option<aq_handoff_note::Stop> {
        use std::sync::atomic::Ordering::Relaxed;
        let (now, limits) = (now_ms(), &self.cache.limits);
        if now.saturating_sub(self.watched.load(Relaxed)) < limits.slot_watch_interval_ms {
            return None;
        }
        self.watched.store(now, Relaxed);
        let over = disk_bytes(&self.slot) > limits.slot_max_bytes;
        over.then_some(aq_handoff_note::Stop::BuildCacheBytes)
    }

    /// The workspace upload for this attempt (`tree_archive` with the ledger's
    /// mtimes); starts the warm container when the tree calls for a generation.
    pub async fn work_archive(
        &self,
        entries: &[TreeEntry],
        socket: &Path,
        image: Option<&str>,
    ) -> Result<Vec<u8>> {
        let stamped = self.stamp(entries)?;
        if let (Some(job), Some(image)) = (&stamped.warm, image) {
            let started = job.start(socket, image).await;
            println!(
                "aq-sandbox-harness: warm build {} started: {started:?}",
                job.name
            );
        }
        Ok(crate::tar::tree_archive_at("aq/work", entries, &|p| {
            stamped.mtime(p)
        })?)
    }

    /// Records the tree being uploaded and gives every file its mtime: the one
    /// it had when this content was last uploaded (0 for the generation's own
    /// content), else now. Starts a generation when the tree calls for one.
    pub fn stamp(&self, entries: &[TreeEntry]) -> Result<Stamped> {
        self.stamp_at(entries, now_ms())
    }
    fn stamp_at(&self, entries: &[TreeEntry], now: u64) -> Result<Stamped> {
        let known = read_ledger(&self.slot.join("ledger.json"));
        let mut ledger = Ledger::new();
        let mut tree = Sha256::new();
        for entry in entries {
            let TreeEntry::File { path, data, .. } = entry else {
                continue;
            };
            let sha = hex(Sha256::new_with_prefix(data));
            tree.update(format!("{path}\0{sha}\n"));
            let mtime = match known.get(path) {
                Some((old, mtime)) if *old == sha => *mtime,
                _ => now / 1000,
            };
            ledger.insert(path.clone(), (sha, mtime));
        }
        write_ledger(&self.slot.join("ledger.json"), &ledger)?;
        let warm = match self.fresh && !self.cache.cache.warm.is_empty() {
            true => self.generation_for(entries, &ledger, &hex(tree)[..16], now)?,
            false => None,
        };
        let mtimes = ledger.into_iter().map(|(path, (_, m))| (path, m)).collect();
        Ok(Stamped { mtimes, warm })
    }

    /// A new generation directory for `tree`, unless one is being built, the
    /// newest is this tree, or the newest is younger than the refresh interval.
    fn generation_for(
        &self,
        entries: &[TreeEntry],
        ledger: &Ledger,
        tree: &str,
        now: u64,
    ) -> Result<Option<WarmJob>> {
        let limits = &self.cache.limits;
        let _project = lock(&self.cache.dir.join("lock"), true)?;
        let generations = subdirs(&self.variant.join("gen"))?;
        if let Some(newest) = generations.last()
            && (!sealed(newest)
                || name(newest).ends_with(tree)
                || now.saturating_sub(created_ms(newest)) < limits.refresh_interval_ms)
        {
            return Ok(None);
        }
        let id = format!("{now:013}-{tree}");
        let generation = self.variant.join("gen").join(&id);
        let src = generation.join("src");
        mkdir(&src)?;
        mkdir(&generation.join("done"))?;
        for entry in entries {
            let path = src.join(entry.path());
            match entry {
                TreeEntry::Dir { .. } => mkdir(&path)?,
                TreeEntry::Symlink { target, .. } => std::os::unix::fs::symlink(target, &path)?,
                TreeEntry::File { mode, data, .. } => {
                    use std::io::Write;
                    let mut file = fs::File::create(&path)?;
                    file.write_all(data)?;
                    file.set_modified(UNIX_EPOCH)?;
                    file.set_permissions(fs::Permissions::from_mode(*mode))?;
                }
            }
        }
        let base = ledger
            .iter()
            .map(|(p, (sha, _))| (p.clone(), (sha.clone(), 0)));
        write_ledger(&generation.join("ledger.json"), &base.collect())?;
        fs::write(
            generation.join("done/warm.sh"),
            self.cache.cache.warm.join("\n"),
        )?;
        let mut binds = vec![
            format!("{}:/aq/src:ro", src.display()),
            format!("{}:/aq/warm", generation.join("done").display()),
        ];
        for (i, path) in self.cache.cache.paths.iter().enumerate() {
            mkdir(&generation.join(format!("p{i}")))?;
            binds.push(format!(
                "{}:{}",
                generation.join(format!("p{i}")).display(),
                path.target
            ));
        }
        let environment = self.cache.environment();
        Ok(Some(WarmJob {
            name: format!("aq-warm-{}-{id}", name(&self.cache.dir).replace('-', "")),
            binds,
            environment: environment.map(|(k, v)| format!("{k}={v}")).collect(),
            limits: limits.clone(),
        }))
    }
}

/// The workspace upload: `Opened::work_archive` on `daemon` (its socket and the
/// template image) with a cache, the plain `tree_archive` without one.
pub async fn work_archive(
    cache: Option<&Opened>,
    entries: &[TreeEntry],
    daemon: (&Path, Option<&str>),
) -> Result<Vec<u8>> {
    match cache {
        Some(cache) => cache.work_archive(entries, daemon.0, daemon.1).await,
        None => Ok(crate::tar::tree_archive("aq/work", entries)?),
    }
}

/// A generation to build: a container of the attempt's template with the tree,
/// no agent, no login and no channel. It seals the generation when it succeeds.
pub struct WarmJob {
    name: String,
    binds: Vec<String>,
    environment: Vec<String>,
    limits: Limits,
}
impl WarmJob {
    /// The daemon's create body. Same confinement as an attempt's sandbox.
    pub fn body(&self, image: &str) -> serde_json::Value {
        let script = format!(
            "set -e\nmkdir -p /aq/work\ncp -a /aq/src/. /aq/work/\ncd /aq/work\ntimeout -k 10 {} bash -e /aq/warm/warm.sh\ntouch /aq/warm/ok",
            self.limits.warm_timeout_ms.div_ceil(1000)
        );
        serde_json::json!({
            "Image": image,
            "Labels": { "aq.managed-by": "aq-build-cache" },
            "Env": self.environment,
            "Cmd": ["/bin/bash", "-c", script],
            "HostConfig": {
                "Privileged": false,
                "CapDrop": ["ALL"],
                "SecurityOpt": ["no-new-privileges"],
                "NanoCpus": self.limits.warm_cpu_millis * 1_000_000,
                "Memory": self.limits.warm_memory_bytes,
                "MemorySwap": self.limits.warm_memory_bytes,
                "PidsLimit": self.limits.warm_pids,
                "NetworkMode": "bridge",
                "Binds": self.binds,
                "AutoRemove": true,
            },
        })
    }

    /// Creates and starts the container on the sandbox daemon, and returns; the
    /// container removes itself. A failure only leaves the generation unsealed.
    pub async fn start(&self, socket: &Path, image: &str) -> Result<()> {
        let body = serde_json::to_vec(&self.body(image))?;
        let created = post(
            socket,
            &format!("/containers/create?name={}", self.name),
            &body,
        )
        .await?;
        ensure!(created == 201, "warm container create: HTTP {created}");
        let started = post(socket, &format!("/containers/{}/start", self.name), b"").await?;
        ensure!(started == 204, "warm container start: HTTP {started}");
        Ok(())
    }
}

/// One `POST` to the daemon, returning the status.
// shortcut: HTTP/1.0 by hand, because the worker's Docker transport lives in the
// aq-worker lib, which may not grow; use that transport once it is its own crate.
async fn post(socket: &Path, path: &str, body: &[u8]) -> Result<u16> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let exchange = async {
        let mut stream = tokio::net::UnixStream::connect(socket).await?;
        let head = format!(
            "POST {path} HTTP/1.0\r\nHost: docker\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        stream.write_all(head.as_bytes()).await?;
        stream.write_all(body).await?;
        let mut response = Vec::new();
        stream.take(64 * 1024).read_to_end(&mut response).await?;
        anyhow::Ok(response)
    };
    let response = tokio::time::timeout(Duration::from_secs(30), exchange).await??;
    let status = std::str::from_utf8(response.get(9..12).unwrap_or_default());
    match status.ok().and_then(|s| s.parse().ok()) {
        Some(status) => Ok(status),
        None => bail!("warm container: no HTTP status from the daemon"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(root: &Path, project: Uuid, max_bytes: u64) -> BuildCacheConfig {
        serde_json::from_value(serde_json::json!({
            "root": root,
            "limits": {
                "max_bytes": max_bytes, "slot_max_bytes": 1 << 30, "slot_max_age_ms": 10_000,
                "slot_watch_interval_ms": 0,
                "refresh_interval_ms": 1000, "warm_timeout_ms": 500, "warm_cpu_millis": 2000,
                "warm_memory_bytes": 1 << 30, "warm_pids": 64,
            },
            "projects": { project.to_string(): {
                "paths": [{ "target": "/tmp/aq-target", "env": "CARGO_TARGET_DIR" }],
                "warm": ["cargo check"],
                "toolchain_files": ["rust-toolchain.toml"],
            }},
        }))
        .unwrap()
    }
    fn file(path: &str, data: &str) -> TreeEntry {
        TreeEntry::File {
            path: path.into(),
            mode: 0o644,
            data: data.into(),
        }
    }
    /// What the warm container does when its build succeeds.
    fn seal(root: &Path) -> PathBuf {
        let unsealed = |g: &PathBuf| !sealed(g);
        let mut generations = Vec::new();
        for project in subdirs(root).unwrap() {
            for variant in subdirs(&project).unwrap() {
                generations.extend(subdirs(&variant.join("gen")).unwrap());
            }
        }
        let generation = generations.into_iter().find(unsealed).unwrap();
        fs::write(generation.join("p0/built"), "main").unwrap();
        fs::write(generation.join("done/ok"), "").unwrap();
        generation
    }

    #[test]
    fn changed_sources_are_newer_than_every_earlier_build_and_unchanged_ones_keep_their_time() {
        let (root, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (project, t1, t2) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let config = config(root.path(), project, u64::MAX);
        let main = [file("a.rs", "a"), file("b.rs", "b")];
        // First task: no generation yet. Its tree is uploaded as new and warms one.
        let first = config.slot(project, t1, "aa").unwrap();
        let opened = first.open_at(work.path(), 1_000).unwrap();
        assert!(!opened.binds()[0].contains(":O"), "{:?}", opened.binds());
        let stamped = opened.stamp_at(&main, 1_000).unwrap();
        assert_eq!((stamped.mtime("a.rs"), stamped.mtime("b.rs")), (1, 1));
        let job = stamped.warm.expect("the first tree warms a generation");
        let body = job.body("image@sha256:aa");
        assert_eq!(body["Labels"]["aq.managed-by"], "aq-build-cache");
        assert_eq!(body["Env"][0], "CARGO_TARGET_DIR=/tmp/aq-target");
        assert_eq!(body["HostConfig"]["CapDrop"][0], "ALL");
        let generation = seal(root.path());
        let src = fs::metadata(generation.join("src/a.rs")).unwrap();
        assert_eq!(src.modified().unwrap(), UNIX_EPOCH);
        drop(opened);
        // A second task of the changed tree overlays it: only the changed file is new.
        let second = config.slot(project, t2, "aa").unwrap();
        let branch = [file("a.rs", "a"), file("b.rs", "B")];
        let opened = second.open_at(work.path(), 5_000).unwrap();
        let bind = &opened.binds()[0];
        assert!(bind.starts_with(&format!(
            "{}/p0:/tmp/aq-target:O,upperdir=",
            generation.display()
        )));
        assert!(
            !generation.join("src").exists(),
            "a sealed generation drops its source copy"
        );
        let stamped = opened.stamp_at(&branch, 5_000).unwrap();
        assert_eq!((stamped.mtime("a.rs"), stamped.mtime("b.rs")), (0, 5));
        assert!(
            stamped.warm.is_some(),
            "the tree moved and the generation is old enough"
        );
        drop(opened);
        // Its continuation keeps those times; an edit since then is new again.
        let opened = second.open_at(work.path(), 9_000).unwrap();
        let stamped = opened.stamp_at(&branch, 9_000).unwrap();
        assert_eq!((stamped.mtime("a.rs"), stamped.mtime("b.rs")), (0, 5));
        assert!(stamped.warm.is_none(), "only a task's first attempt warms");
        let edited = [file("a.rs", "a2"), file("b.rs", "B")];
        let stamped = opened.stamp_at(&edited, 9_500).unwrap();
        assert_eq!((stamped.mtime("a.rs"), stamped.mtime("b.rs")), (9, 5));
        // The slot is held: a second attempt of the task cannot share it.
        assert!(second.open_at(work.path(), 9_600).is_err());
    }

    /// The `warm.sh` a fresh slot of `config` writes for one tree, if any.
    fn warm_script(
        config: &BuildCacheConfig,
        project: Uuid,
        environment: &[&str],
    ) -> Option<String> {
        let work = tempfile::tempdir().unwrap();
        let mut slot = config.slot(project, Uuid::new_v4(), "aa").unwrap();
        slot.warm_from_environment(environment.iter().map(|w| w.to_string()).collect());
        let opened = slot.open_at(work.path(), 1_000).unwrap();
        opened.stamp_at(&[file("a.rs", "a")], 1_000).unwrap().warm?;
        let generation = subdirs(&opened.variant.join("gen")).unwrap().pop().unwrap();
        Some(fs::read_to_string(generation.join("done/warm.sh")).unwrap())
    }

    #[test]
    fn warm_commands_come_from_the_admitted_environment() {
        let (root, project) = (tempfile::tempdir().unwrap(), Uuid::new_v4());
        let mut config = config(root.path(), project, u64::MAX);
        config.0.projects.get_mut(&project).unwrap().warm.clear();
        let environment = ["pnpm install --frozen-lockfile", "pnpm build"];
        assert_eq!(
            warm_script(&config, project, &environment).as_deref(),
            Some("pnpm install --frozen-lockfile\npnpm build")
        );
        assert_eq!(
            warm_script(&config, project, &[]),
            None,
            "neither warms: slots only"
        );
    }

    #[test]
    fn node_config_warm_still_wins_while_set() {
        let (root, project) = (tempfile::tempdir().unwrap(), Uuid::new_v4());
        let config = config(root.path(), project, u64::MAX);
        let script = warm_script(&config, project, &["pnpm install --frozen-lockfile"]);
        assert_eq!(script.as_deref(), Some("cargo check"));
    }

    #[test]
    fn a_generation_is_not_rebuilt_while_building_for_the_same_tree_or_inside_the_interval() {
        let (root, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let project = Uuid::new_v4();
        let config = config(root.path(), project, u64::MAX);
        let warm = |tree: &str, now: u64| {
            let slot = config.slot(project, Uuid::new_v4(), "aa").unwrap();
            let opened = slot.open_at(work.path(), now).unwrap();
            opened
                .stamp_at(&[file("a.rs", tree)], now)
                .unwrap()
                .warm
                .is_some()
        };
        assert!(warm("main", 1_000));
        assert!(!warm("other", 1_100), "one is being built");
        // The build died: past the warm timeout the unsealed generation is deleted.
        assert!(warm("main", 2_000));
        seal(root.path());
        assert!(!warm("main", 9_000), "same tree");
        assert!(
            !warm("moved", 2_500),
            "newest generation younger than the interval"
        );
        assert!(warm("moved", 9_000));
    }

    #[test]
    fn another_template_or_toolchain_never_reuses_a_generation_or_a_slot() {
        let (root, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (project, task) = (Uuid::new_v4(), Uuid::new_v4());
        let config = config(root.path(), project, u64::MAX);
        fs::write(work.path().join("rust-toolchain.toml"), "1.96.1").unwrap();
        let tree = [file("a.rs", "a")];
        let slot = config.slot(project, task, "aa").unwrap();
        let opened = slot.open_at(work.path(), 1_000).unwrap();
        opened.stamp_at(&tree, 1_000).unwrap();
        seal(root.path());
        let first = opened.binds();
        drop(opened);
        let again = |template: &str| {
            let slot = config.slot(project, task, template).unwrap();
            let opened = slot.open_at(work.path(), 2_000).unwrap();
            (
                opened.binds(),
                opened.stamp_at(&tree, 2_000).unwrap().mtime("a.rs"),
            )
        };
        assert_eq!(
            again("aa"),
            (first.clone(), 1),
            "the same variant reuses the slot"
        );
        for (template, toolchain) in [("bb", "1.96.1"), ("aa", "1.97.0")] {
            fs::write(work.path().join("rust-toolchain.toml"), toolchain).unwrap();
            let (binds, mtime) = again(template);
            assert!(
                !binds[0].contains(":O"),
                "no generation of another variant: {binds:?}"
            );
            assert_ne!(binds, first, "no slot of another variant");
            assert_eq!(mtime, 2, "every source is new to an empty cache");
        }
    }

    #[test]
    fn eviction_is_least_recently_used_and_spares_what_is_in_use_or_overlaid() {
        let (root, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let project = Uuid::new_v4();
        let tasks: Vec<Uuid> = (0..4).map(|_| Uuid::new_v4()).collect();
        let open = |task: usize, now: u64, max: u64| {
            let slot = config(root.path(), project, max).slot(project, tasks[task], "aa");
            let opened = slot.unwrap().open_at(work.path(), now).unwrap();
            fs::write(opened.slot.join("p0/upper/out"), vec![1u8; 64 * 1024]).unwrap();
            opened
        };
        let stamped = open(0, 1_000, u64::MAX).stamp_at(&[file("a.rs", "a")], 1_000);
        assert!(stamped.unwrap().warm.is_some());
        let generation = seal(root.path());
        let overlay = open(1, 2_000, u64::MAX);
        assert!(overlay.generation.is_some());
        let overlay = { overlay }.slot.clone();
        let held = open(2, 3_000, u64::MAX);
        // Far over a 100 kB bound: the idle slots go, oldest first; the held one stays.
        let last = open(3, 4_000, 100_000);
        let slots = subdirs(last.slot.parent().unwrap()).unwrap();
        assert_eq!(slots.len(), 2, "{slots:?}");
        assert!(held.slot.exists() && last.slot.exists() && !overlay.exists());
        assert!(
            generation.exists(),
            "the newest generation of the variant is kept"
        );
        // A task that ended: its slot is discarded unless an attempt holds it.
        let config = config(root.path(), project, u64::MAX);
        config.discard_task(project, tasks[2]).unwrap();
        assert!(held.slot.exists());
        let slot = held.slot.clone();
        drop(held);
        config.discard_task(project, tasks[2]).unwrap();
        assert!(!slot.exists());
        // Idle longer than the age limit: deleted at the next open.
        let slot = last.slot.clone();
        drop(last);
        open(0, 4_000 + 10_001, u64::MAX);
        assert!(!slot.exists());
    }

    #[test]
    fn a_generation_a_slot_overlays_is_never_evicted() {
        let (root, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let project = Uuid::new_v4();
        let open = |tree: &str, now: u64, max: u64| {
            let slot = config(root.path(), project, max).slot(project, Uuid::new_v4(), "aa");
            let opened = slot.unwrap().open_at(work.path(), now).unwrap();
            opened.stamp_at(&[file("a.rs", tree)], now).unwrap();
            opened
        };
        drop(open("one", 1_000, u64::MAX));
        let old = seal(root.path());
        fs::write(old.join("p0/big"), vec![1u8; 256 * 1024]).unwrap();
        let live = open("two", 5_000, u64::MAX);
        assert_eq!(live.generation.as_ref(), Some(&old));
        let new = seal(root.path());
        // Over the bound with a newer generation present: the old one still has a slot.
        let other = open("two", 6_000, 1);
        assert!(old.exists() && new.exists());
        drop((live, other));
        // Its slots gone, the old generation goes; the newest stays for the next task.
        open("two", 7_000, 1);
        assert!(!old.exists() && new.exists());
    }

    #[test]
    fn a_slot_past_its_size_limit_is_reported_and_deleted_when_the_attempt_lets_go() {
        let (root, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let project = Uuid::new_v4();
        let mut settings = serde_json::to_value(config(root.path(), project, u64::MAX)).unwrap();
        settings["limits"]["slot_max_bytes"] = 200_000.into();
        let config: BuildCacheConfig = serde_json::from_value(settings).unwrap();
        let slot = config.slot(project, Uuid::new_v4(), "aa").unwrap();
        let opened = slot.open(work.path()).unwrap();
        fs::write(opened.slot.join("p0/upper/small"), vec![1u8; 4096]).unwrap();
        assert_eq!(opened.exceeded(), None);
        fs::write(opened.slot.join("p0/upper/big"), vec![1u8; 400_000]).unwrap();
        assert_eq!(
            opened.exceeded(),
            Some(aq_handoff_note::Stop::BuildCacheBytes)
        );
        let slot = opened.slot.clone();
        drop(opened);
        assert!(!slot.exists());
    }

    #[test]
    fn settings_refuse_targets_the_driver_copies_scans_or_mounts() {
        let parse = |target: &str, env: &str| {
            serde_json::from_value::<BuildCacheConfig>(serde_json::json!({
                "root": "/var/cache/aq",
                "limits": {
                    "max_bytes": 1, "slot_max_bytes": 1, "slot_max_age_ms": 1, "refresh_interval_ms": 1,
                    "slot_watch_interval_ms": 1,
                    "warm_timeout_ms": 1, "warm_cpu_millis": 1, "warm_memory_bytes": 1, "warm_pids": 1,
                },
                "projects": { Uuid::new_v4().to_string(): { "paths": [
                    { "target": "/aq/home/.cargo/registry" }, { "target": target, "env": env },
                ]}},
            }))
        };
        assert!(parse("/tmp/aq-target", "CARGO_TARGET_DIR").is_ok());
        for target in [
            "/aq/work/target",
            "/aq/work",
            "/aq",
            "/aq/home",
            "/aq/in/x",
            "/aq/out",
            "/aq/private/material",
            "/aq/channel",
            "/aq/home/projects/-aq-work",
            "/aq/home/codex/sessions",
            "/aq/home/codex/cargo",
            "/aq/home/.cargo/registry/cache",
            "/tmp",
            "tmp/x",
            "/tmp/../aq/work",
            "/tmp/a:/aq/work",
            "/tmp/a,upperdir=/x",
        ] {
            assert!(parse(target, "CARGO_TARGET_DIR").is_err(), "{target}");
        }
        for env in ["PATH", "HOME", "", "a=b", "lower"] {
            assert!(parse("/tmp/aq-target", env).is_err(), "{env}");
        }
        // No limit has a default: leaving one out is an error, not a constant.
        assert!(serde_json::from_value::<Limits>(serde_json::json!({ "max_bytes": 1 })).is_err());
    }

    /// Live daemon (rootless podman): `AQ_LIVE_SOCKET=<sandbox socket>
    /// AQ_LIVE_IMAGE=<template image> cargo test -p aq-sandbox-archive -- --ignored live`.
    /// The warm container builds and seals a generation from the uploaded tree.
    #[tokio::test]
    #[ignore = "needs the sandbox daemon and a template image"]
    async fn live_warm_container_seals_a_generation() {
        let socket = PathBuf::from(std::env::var("AQ_LIVE_SOCKET").unwrap());
        let image = std::env::var("AQ_LIVE_IMAGE").unwrap();
        // Under the home directory: the rootless daemon must be able to bind it.
        let root = tempfile::tempdir_in(std::env::var("AQ_LIVE_ROOT").unwrap()).unwrap();
        let work = tempfile::tempdir().unwrap();
        let project = Uuid::new_v4();
        let mut settings = serde_json::to_value(config(root.path(), project, u64::MAX)).unwrap();
        settings["limits"]["warm_timeout_ms"] = 60_000.into();
        settings["projects"][project.to_string()]["warm"] = serde_json::json!([
            "stat -c %Y a.rs > $CARGO_TARGET_DIR/mtime",
            "cat a.rs > $CARGO_TARGET_DIR/built"
        ]);
        let config: BuildCacheConfig = serde_json::from_value(settings).unwrap();
        let slot = config.slot(project, Uuid::new_v4(), "live").unwrap();
        let opened = slot.open(work.path()).unwrap();
        let archive = opened
            .work_archive(&[file("a.rs", "main")], &socket, Some(&image))
            .await;
        archive.unwrap();
        let generation = subdirs(&opened.variant.join("gen")).unwrap().pop().unwrap();
        for _ in 0..120 {
            if sealed(&generation) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        assert!(
            sealed(&generation),
            "the warm container did not seal the generation"
        );
        assert_eq!(
            fs::read_to_string(generation.join("p0/built")).unwrap(),
            "main"
        );
        assert_eq!(
            fs::read_to_string(generation.join("p0/mtime"))
                .unwrap()
                .trim(),
            "0"
        );
    }
}
