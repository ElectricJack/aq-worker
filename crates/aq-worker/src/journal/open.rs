use super::*;

pub(super) fn lock(root: &Path) -> Result<OwnerLock> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(root)?;
    ensure!(
        !std::fs::symlink_metadata(root)?.file_type().is_symlink(),
        "journal root cannot be a symlink"
    );
    std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700))?;
    let owner = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(root.join("owner.lock"))?;
    let owner = OwnerLock::acquire(owner)?;
    // Rust opens Files CLOEXEC; verify rather than relying on an implicit
    // inheritance assumption for the guardian's exec boundary.
    use std::os::fd::AsRawFd;
    ensure!(
        unsafe { libc::fcntl(owner.file.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC != 0,
        "journal lock must not survive exec"
    );
    let parent = root
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    File::open(parent)?.sync_all()?;
    Ok(owner)
}
pub(super) fn load(file: &Path, identity: &NodeIdentity) -> Result<State> {
    if !file.exists() {
        return Ok(State {
            format_version: STATE_FORMAT,
            identity: identity.clone(),
            inventory_sequence: 0,
            attempts: BTreeMap::new(),
            directive_receipts: BTreeMap::new(),
            directive_contents: BTreeMap::new(),
            reconciliation_required: false,
            local_drain: false,
        });
    }
    ensure!(
        std::fs::metadata(file)?.len() <= MAX_JOURNAL_BYTES,
        "journal exceeds bounded state size"
    );
    let mut s: State = serde_json::from_slice(&std::fs::read(file)?)
        .context("parse journal; refusing to advertise capacity")?;
    ensure!(
        matches!(s.format_version, 1 | 2 | 3 | STATE_FORMAT),
        "unsupported state format {}",
        s.format_version
    );
    // Version 1 has no guardian handshake. Missing new fields never
    // establish that a legacy launch was unstarted or stopped.
    s.format_version = STATE_FORMAT;
    ensure!(
        s.identity.node_id == identity.node_id,
        "journal belongs to another node"
    );
    for (id, record) in &s.attempts {
        validate_attempt(id, record, s.identity.node_id)?;
    }
    Ok(s)
}
fn validate_attempt(id: &Uuid, r: &AttemptRecord, node_id: Uuid) -> Result<()> {
    ensure!(*id == r.offer.attempt.attempt_id, "corrupt attempt key");
    r.offer.validate()?;
    r.acceptance.validate()?;
    if let Some(source) = &r.source {
        super::source::validate(r, source, node_id)?;
    }
    ensure!(
        r.acceptance.attempt == r.offer.attempt
            && r.acceptance.offer_id == r.offer.offer_id
            && r.acceptance.resolved_spec_digest == r.offer.spec.resolved_spec_digest,
        "corrupt acceptance binding"
    );
    Ok(())
}
