use super::*;
/// Includes several >=250ms control round trips. Ceiling conversion can add <1s.
pub const AUTO_STOP_MARGIN_MS: u64 = 5_000;
pub(super) fn ttl(deadline: u64, now: u64) -> Result<u64, BackendError> {
    let remaining = deadline
        .checked_add(AUTO_STOP_MARGIN_MS)
        .and_then(|end| end.checked_sub(now))
        .filter(|_| deadline > now)
        .ok_or(BackendError::InvalidRequest("boat hard deadline expired"))?;
    let seconds = remaining.div_ceil(1000);
    if seconds > 2_592_000 {
        return Err(BackendError::InvalidRequest(
            "boat deadline exceeds provider TTL",
        ));
    }
    Ok(seconds)
}
pub(super) fn size(r: ResourceLimits) -> Result<&'static str, BackendError> {
    for (cpu, mem, size) in [
        (2000, 4u64 << 30, "small"),
        (4000, 8u64 << 30, "default"),
        (8000, 16u64 << 30, "large"),
    ] {
        if r.cpu_millis <= cpu && r.memory_bytes <= mem {
            return Ok(size);
        }
    }
    Err(BackendError::InvalidRequest(
        "boat resource request exceeds large VM",
    ))
}
pub(super) fn create_container(
    r: &CreateRequest,
    image: &str,
    runner: Option<&[String]>,
) -> String {
    let limits = r.resources.expect("validated limits");
    let cpu = format!(
        "{}.{:03}",
        limits.cpu_millis / 1000,
        limits.cpu_millis % 1000
    );
    // No host paths or Docker socket enter the container. Root belongs to the
    // fixed B6b runner; it transitions to the distinct unprivileged agent user.
    let command = runner
        .map(|args| {
            args.iter()
                .map(|s| format!(" '{}'", s.replace('\'', "'\\''")))
                .collect::<String>()
        })
        .unwrap_or_default();
    format!(
        "sudo docker volume create {name}-work >/dev/null && sudo docker create --name {name} --cap-drop=ALL --security-opt=no-new-privileges --pids-limit {pids} --memory {memory} --memory-swap {memory} --cpus {cpu} --network=bridge --restart=no --mount type=volume,source={name}-work,target=/aq/work --workdir /aq/work {image}{command}",
        name = r.name,
        pids = limits.pids,
        memory = limits.memory_bytes
    )
}
pub(super) fn prefix(node: Uuid) -> String {
    format!("aq-{}-", node.simple())
}
pub(super) fn attempt(name: &str, node: Uuid) -> Option<Uuid> {
    let value = name.strip_prefix(&prefix(node))?;
    let id = Uuid::parse_str(value).ok()?;
    (id.simple().to_string() == value && !id.is_nil()).then_some(id)
}
