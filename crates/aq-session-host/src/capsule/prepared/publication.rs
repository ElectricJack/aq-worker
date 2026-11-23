//! Publish and authenticate the two files bound to a prepared commit.
use super::*;
use std::io::{Read, Write};
use zeroize::Zeroizing;
pub(super) const AUTH: &str = "auth.json";
const CONFIG: &str = "candidate-config.json";
const MAX_FILE: u64 = 65536;
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct FilePin {
    device: u64,
    inode: u64,
    bytes: u64,
    digest: Digest,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidatePublishedInputs {
    auth: FilePin,
    config: FilePin,
    capability_digest: Digest,
}
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Config {
    profile: String,
    owner_intent: ArtifactRef,
    preparation_digest: Digest,
    capsule_id: Uuid,
    origin: String,
    deadline_boot_ms: u64,
    capability_digest: Digest,
}
fn config(info: &CandidatePreparedInfo, cap: Digest) -> Config {
    Config {
        profile: PROFILE.into(),
        owner_intent: info.owner_intent.clone(),
        preparation_digest: info.preparation_digest.clone(),
        capsule_id: info.capsule_id,
        origin: info.origin.clone(),
        deadline_boot_ms: info.original_deadline_boot_ms,
        capability_digest: cap,
    }
}
pub(super) fn private_directory(path: &Path, expected: (u64, u64)) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let m = file.metadata()?;
    ensure!(
        m.is_dir()
            && m.uid() == unsafe { libc::geteuid() }
            && m.mode() & 0o7777 == 0o700
            && (m.dev(), m.ino()) == expected,
        "prepared derived directory identity"
    );
    Ok(file)
}
pub(super) fn read_member(directory: &File, name: &str) -> Result<(Zeroizing<Vec<u8>>, FilePin)> {
    let name = std::ffi::CString::new(name)?;
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    ensure!(fd >= 0, "prepared derived member unavailable");
    let mut file = unsafe { File::from_raw_fd(fd) };
    let before = file.metadata()?;
    verify_member_metadata(&before)?;
    let mut body = Zeroizing::new(Vec::new());
    Read::by_ref(&mut file)
        .take(MAX_FILE + 1)
        .read_to_end(&mut body)?;
    let after = file.metadata()?;
    ensure!(
        body.len() as u64 == before.len() && stable_member(&before, &after),
        "prepared derived member changed"
    );
    let pin = FilePin {
        device: before.dev(),
        inode: before.ino(),
        bytes: before.len(),
        digest: Digest::of(&body),
    };
    Ok((body, pin))
}
fn verify_member_metadata(meta: &std::fs::Metadata) -> Result<()> {
    let private = meta.uid() == unsafe { libc::geteuid() } && meta.mode() & 0o7777 == 0o600;
    let bounded = meta.len() > 0 && meta.len() <= MAX_FILE;
    ensure!(
        meta.is_file() && private && meta.nlink() == 1 && bounded,
        "prepared derived member shape"
    );
    Ok(())
}
fn stable_member(before: &std::fs::Metadata, after: &std::fs::Metadata) -> bool {
    let inode = (before.dev(), before.ino()) == (after.dev(), after.ino());
    let mtime = (before.mtime(), before.mtime_nsec()) == (after.mtime(), after.mtime_nsec());
    inode && before.len() == after.len() && mtime
}
fn write_member(directory: &File, name: &str, body: &[u8]) -> Result<()> {
    let name = std::ffi::CString::new(name)?;
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    ensure!(fd >= 0, "prepared synthetic publication reserve");
    let mut file = unsafe { File::from_raw_fd(fd) };
    file.write_all(body)?;
    file.sync_all()?;
    Ok(())
}
impl CandidatePublishedInputs {
    /// Synthetic fixture publication only; no original provider source is accepted.
    pub fn publish_synthetic(
        info: &CandidatePreparedInfo,
        path: &Path,
        local_capability: &[u8],
    ) -> Result<Self> {
        ensure!(
            ownership::raw_boot_ms() < info.original_deadline_boot_ms,
            "prepared publication expired"
        );
        let directory = private_directory(path, info.auth_directory)?;
        ensure!(
            std::fs::read_dir(format!("/proc/thread-self/fd/{}", directory.as_raw_fd()))?
                .next()
                .is_none(),
            "prepared publication must be empty"
        );
        let capability_digest = Digest::of(local_capability);
        let mut auth = Zeroizing::new(b"{\"OPENAI_API_KEY\":\"".to_vec());
        auth.extend_from_slice(local_capability);
        auth.extend_from_slice(b"\"}");
        crate::private_inputs::verify_local_capability_auth(&auth, &capability_digest)?;
        let cfg = serde_json::to_vec(&config(info, capability_digest.clone()))?;
        write_member(&directory, AUTH, &auth)?;
        write_member(&directory, CONFIG, &cfg)?;
        directory.sync_all()?;
        let (_, auth) = read_member(&directory, AUTH)?;
        let (_, config) = read_member(&directory, CONFIG)?;
        Ok(Self {
            auth,
            config,
            capability_digest,
        })
    }
}
impl CandidatePublishedInputs {
    pub(super) fn verify(&self, scope: &CandidatePreparedInfo) -> Result<()> {
        let directory = private_directory(Path::new("/auth"), scope.auth_directory)?;
        verify_members()?;
        let (auth, auth_pin) = read_member(&directory, AUTH)?;
        let (cfg, cfg_pin) = read_member(&directory, CONFIG)?;
        ensure!(
            auth_pin == self.auth && cfg_pin == self.config,
            "prepared actual file digest/identity"
        );
        crate::private_inputs::verify_local_capability_auth(&auth, &self.capability_digest)?;
        let actual: Config = serde_json::from_slice(&cfg)?;
        ensure!(
            actual == config(scope, self.capability_digest.clone()),
            "prepared actual config binding"
        );
        Ok(())
    }
}
fn verify_members() -> Result<()> {
    let names = std::fs::read_dir("/auth")?
        .map(|entry| Ok(entry?.file_name()))
        .collect::<Result<Vec<_>>>()?;
    ensure!(
        names.len() == 2 && names.iter().all(|n| n == AUTH || n == CONFIG),
        "prepared fixed derived members"
    );
    Ok(())
}
