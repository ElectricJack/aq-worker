use super::*;

/// Written in a slot's `current` when the service stops, so an attempt that had no driver
/// directory yet (still preparing) is still handed off as a service stop, not a crash.
pub const SERVICE_STOP_MARKER: &str = "service-stop";

fn write_marker(path: &Path, reason: HandoffReason) -> Result<()> {
    let body = serde_json::json!({"schema_version": 1, "reason": reason}).to_string();
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(mut f) => {
            std::io::Write::write_all(&mut f, body.as_bytes()).context("write handoff marker")
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e).context("create handoff marker"),
    }
}
/// Writes <driver dir>/handoff (create-new, 0600) for the driver whose driver.json names `attempt`,
/// searching current/host/driver-*/driver.json of `slots`: the slots whose attempt is still
/// running (a kept-for-recovery or held `current` is not). True when written or already present.
pub fn mark_handoff(
    layout: &Layout,
    slots: &[u32],
    attempt: Uuid,
    reason: HandoffReason,
) -> Result<bool> {
    for (dir, id) in driver_dirs(layout, slots) {
        if id == attempt {
            return write_marker(&dir.join(aq_handoff_note::HANDOFF_MARKER), reason).map(|()| true);
        }
    }
    Ok(false)
}
/// The slot and attempt id of each driver running in `slots`: driver.json present, exit.json
/// absent.
pub fn running_attempts(layout: &Layout, slots: &[u32]) -> Result<Vec<(u32, Uuid)>> {
    Ok(slots
        .iter()
        .flat_map(|&slot| {
            let dirs = driver_dirs_in(&layout.host_root(slot));
            dirs.into_iter()
                .filter(|(d, _)| !d.join("exit.json").exists())
                .map(move |(_, id)| (slot, id))
        })
        .collect())
}
fn driver_dirs(layout: &Layout, slots: &[u32]) -> Vec<(PathBuf, Uuid)> {
    slots
        .iter()
        .flat_map(|slot| driver_dirs_in(&layout.host_root(*slot)))
        .collect()
}
fn driver_dirs_in(host: &Path) -> Vec<(PathBuf, Uuid)> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(host) else {
        return found;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        if !entry.file_name().to_string_lossy().starts_with("driver-") {
            continue;
        }
        let Ok(bytes) = std::fs::read(dir.join("driver.json")) else {
            continue;
        };
        let id = serde_json::from_slice::<serde_json::Value>(&bytes)
            .ok()
            .and_then(|v| v["owner"]["attempt_id"].as_str()?.parse().ok());
        if let Some(id) = id {
            found.push((dir, id));
        }
    }
    found
}
/// The reason the service recorded for `attempt` before the cut: the handoff marker in its
/// driver directory under `host_root`, else the slot's service-stop marker in `state_dir`;
/// None when neither exists.
pub fn recorded_handoff(
    host_root: &Path,
    state_dir: &Path,
    attempt: Uuid,
) -> Option<HandoffReason> {
    let driver = driver_dirs_in(host_root)
        .into_iter()
        .find(|(_, id)| *id == attempt)
        .and_then(|(dir, _)| {
            aq_handoff_note::read_handoff_marker(&dir.join(aq_handoff_note::HANDOFF_MARKER))
        });
    let slot = state_dir.join(SERVICE_STOP_MARKER);
    driver.or_else(|| {
        slot.exists()
            .then(|| aq_handoff_note::read_handoff_marker(&slot))
            .flatten()
    })
}


impl<B: SlotBackend, C: ControlSource> Shared<B, C> {
    /// Marks and announces the running attempts. Each notice post is bounded by
    /// `HANDOFF_POST_WAIT`; the caller bounds the whole call by the grace.
    pub(super) async fn hand_off_running(&self, posted: &mut HashSet<Uuid>) {
        let layout = &self.sup.layout;
        let slots = self.active_slots();
        write_stop_markers(layout, &slots);
        let running = mark_running_attempts(layout, &slots);
        for (slot, id) in running {
            if posted.insert(id) {
                self.post_handoff(slot, id).await;
            }
        }
    }

    async fn post_handoff(&self, slot: u32, id: Uuid) {
        let notice = HandoffNotice {
            schema_version: 1,
            attempt_id: id,
            reason: HandoffReason::ServiceStop,
        };
        let post = self.sup.control.handoff(slot, &notice);
        let sent = tokio::time::timeout(HANDOFF_POST_WAIT, post);
        match sent.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::warn!(%id, "handoff notice: {e:#}"),
            Err(_) => tracing::warn!(%id, "handoff notice timed out"),
        }
    }
}

fn write_stop_markers(layout: &Layout, slots: &[u32]) {
    for &slot in slots {
        let current = layout.current(slot);
        if let Err(e) = write_marker(
            &current.join(SERVICE_STOP_MARKER),
            HandoffReason::ServiceStop,
        ) {
            tracing::warn!(slot, "service-stop marker: {e:#}");
        }
    }
    // Every marker first: the driver acts on it, the notice only informs, so a hung
    // server can never delay another slot's marker.
}

fn mark_running_attempts(layout: &Layout, slots: &[u32]) -> Vec<(u32, Uuid)> {
    let running = running_attempts(layout, slots).unwrap_or_default();
    for &(slot, id) in &running {
        if let Err(e) = mark_handoff(layout, &[slot], id, HandoffReason::ServiceStop) {
            tracing::warn!(%id, "handoff marker: {e:#}");
        }
    }
    running
}
