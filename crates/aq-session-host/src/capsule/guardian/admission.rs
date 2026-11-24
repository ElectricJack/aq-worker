//! Validate original capsule inputs before starting the setup process.
use super::*;

pub(super) fn has_live_deadline(plan: &PlanRecord, now: u64) -> bool {
    !plan.nonce.is_nil() && plan.deadline > now && plan.deadline - now <= MAX_LIFETIME_MS
}
pub(super) fn validate(plan: &PlanRecord, files: &mut [File]) -> Result<()> {
    validate_bounds(plan, files.len())?;
    seal::verify(&mut files[0], &plan.program, MAX_PROGRAM)?;
    ensure!(plan.program.executable, "capsule program executable");
    seal::verify(&mut files[1], &plan.stdin, MAX_STDIN as u64)?;
    ensure!(!plan.stdin.executable, "capsule stdin nonexecutable");
    validate_root(plan, &files[2])?;
    validate_grants(plan, &files[3..])?;
    prepared::validate_plan(plan, files)?;
    Ok(())
}
fn validate_bounds(plan: &PlanRecord, count: usize) -> Result<()> {
    let now = ownership::raw_boot_ms();
    ensure!(
        has_live_deadline(plan, now) && count == plan.grants.len() + 3 && plan.grants.len() <= 10,
        "capsule plan bounds"
    );
    ensure!(
        plan.arguments.len() <= 16
            && plan
                .arguments
                .iter()
                .all(|a| a.len() <= 2048 && !a.contains('\0')),
        "capsule argument limits"
    );
    Ok(())
}
fn validate_root(plan: &PlanRecord, root: &File) -> Result<()> {
    plan.root_pin.verify(root)?;
    ensure!(
        std::fs::canonicalize(&plan.root)? == plan.root
            && std::fs::read_dir(&plan.root)?.next().is_none(),
        "capsule retained fresh root"
    );
    let named = std::fs::symlink_metadata(&plan.root)?;
    ensure!(
        named.is_dir()
            && named.dev() == plan.root_pin.device
            && named.ino() == plan.root_pin.inode
            && named.uid() == unsafe { libc::geteuid() }
            && named.mode() & 0o777 == 0o700,
        "capsule root binding/mode"
    );
    Ok(())
}
fn validate_grants(plan: &PlanRecord, files: &[File]) -> Result<()> {
    let mut targets = Vec::new();
    for (grant, file) in plan.grants.iter().zip(files) {
        ensure!(
            grant.source.is_absolute()
                && std::fs::canonicalize(&grant.source)? == grant.source
                && !plan.root.starts_with(&grant.source)
                && !grant.source.starts_with(&plan.root),
            "capsule original source route"
        );
        grant.target.relative()?;
        grant.pin.verify(file)?;
        ensure!(
            !targets.contains(&grant.target)
                && (!grant.writable
                    || matches!(
                        grant.target,
                        MountTarget::Workspace | MountTarget::GitMetadata | MountTarget::Allowed(_)
                    )),
            "capsule mount target policy"
        );
        targets.push(grant.target);
    }
    ensure!(
        targets.contains(&MountTarget::Workspace),
        "capsule workspace absent"
    );
    Ok(())
}
