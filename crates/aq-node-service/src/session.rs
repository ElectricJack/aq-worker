//! A service-lifetime node boot and the coordination shared by its agent slots.
use anyhow::{Result, ensure};
use aq_protocol::{ControlResponse, NODE_SLOTS_SCHEMA_VERSION};
use std::{
    future::Future,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
};
use tokio::sync::Mutex as AsyncMutex;
use uuid::Uuid;

#[derive(Default)]
pub struct InventoryState {
    /// Serializes publication and consumers that require the current snapshot.
    pub gate: AsyncMutex<()>,
    allocated: AtomicU64,
    pub acknowledged: AtomicU64,
    /// Boot-clock expiry of the acknowledged report; zero before the first.
    deadline: AtomicU64,
    /// Slots between receiving an offer and holding their execution grant.
    in_flight: AtomicU32,
}
/// While a slot is in flight a report is renewed only this close to its expiry.
const REFRESH_FLOOR_NS: u64 = 5_000_000_000;
/// Held by a slot from its offer until its execution request has been answered.
pub struct InFlight(Arc<InventoryState>);
impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}
impl InventoryState {
    /// The node's acknowledged sequence, while its report has more than `need_ns` left at `now`.
    /// Every slot polls with this one sequence; a new report is due only when this is `None`.
    pub fn current(&self, now: u64, need_ns: u64) -> Option<u64> {
        let sequence = self.acknowledged.load(Ordering::SeqCst);
        let left = self.deadline.load(Ordering::SeqCst).saturating_sub(now);
        (sequence > 0 && left > need_ns).then_some(sequence)
    }
    /// The life a poll asks of the report: half its TTL while the node is idle (renewed early,
    /// between offers), only the floor while any slot is in flight (a renewal then waits).
    pub fn poll_need(&self, ttl_ms: u64) -> u64 {
        let ttl = ttl_ms.saturating_mul(1_000_000);
        if self.in_flight.load(Ordering::SeqCst) > 0 {
            REFRESH_FLOOR_NS.min(ttl / 4)
        } else {
            ttl / 2
        }
    }
    /// Records the report the server acknowledged (under `gate`).
    pub fn record(&self, sequence: u64, deadline_ns: u64) {
        self.acknowledged.store(sequence, Ordering::SeqCst);
        self.deadline.store(deadline_ns, Ordering::SeqCst);
    }
    pub fn flight(self: &Arc<Self>) -> InFlight {
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        InFlight(self.clone())
    }
    pub fn next(&self, minimum: u64) -> Result<u64> {
        self.allocated.fetch_max(minimum, Ordering::SeqCst);
        self.allocated
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                n.checked_add(1).filter(|v| *v <= i64::MAX as u64)
            })
            .map(|n| n + 1)
            .map_err(|_| anyhow::anyhow!("inventory highwater exhausted"))
    }
}

pub struct NodeSession {
    pub boot_id: Uuid,
    pub registration_id: Uuid,
    pub inventory: Arc<InventoryState>,
    slots: AtomicU32,
    aware: AtomicBool,
    registered: AsyncMutex<bool>,
    registered_slots: OnceLock<u32>,
    occupancy: Mutex<Occupancy>,
    /// Wakes the slots waiting in `room` when capacity may have been freed.
    freed: tokio::sync::Notify,
    retired: AtomicBool,
}
#[derive(Default)]
struct Occupancy {
    queued: u32,
    manual: std::collections::HashSet<Uuid>,
}
pub struct QueuedPermit(Arc<NodeSession>);
impl Drop for QueuedPermit {
    fn drop(&mut self) {
        self.0.occupancy.lock().unwrap().queued -= 1;
        self.0.freed.notify_waiters();
    }
}
impl Default for NodeSession {
    fn default() -> Self {
        Self {
            boot_id: Uuid::new_v4(),
            registration_id: Uuid::new_v4(),
            inventory: Arc::default(),
            slots: AtomicU32::new(1),
            aware: AtomicBool::new(false),
            registered: AsyncMutex::new(false),
            registered_slots: OnceLock::new(),
            occupancy: Mutex::default(),
            freed: tokio::sync::Notify::new(),
            retired: AtomicBool::new(false),
        }
    }
}
impl NodeSession {
    pub fn pending() -> Self {
        Self {
            slots: AtomicU32::new(0),
            ..Self::default()
        }
    }
    pub fn update(&self, reply: &ControlResponse) -> Result<()> {
        crate::link::validate_control(reply)?;
        ensure!(
            !self.aware() || reply.schema_version == NODE_SLOTS_SCHEMA_VERSION,
            "node slot protocol downgraded during a boot"
        );
        self.aware.store(
            reply.schema_version == NODE_SLOTS_SCHEMA_VERSION,
            Ordering::SeqCst,
        );
        self.slots.store(reply.node_slots(), Ordering::SeqCst);
        self.freed.notify_waiters();
        Ok(())
    }
    pub fn retire(&self) {
        self.retired.store(true, Ordering::SeqCst);
        self.slots.store(0, Ordering::SeqCst);
    }
    pub fn retired(&self) -> bool {
        self.retired.load(Ordering::SeqCst)
    }
    pub fn aware(&self) -> bool {
        self.aware.load(Ordering::SeqCst)
    }
    pub fn slots(&self) -> u32 {
        self.slots.load(Ordering::SeqCst)
    }
    /// Register retries retain the exact operation id, boot and initial count.
    pub fn registered_slots(&self) -> u32 {
        *self.registered_slots.get_or_init(|| self.slots().max(1))
    }
    pub async fn register_once<F, T>(&self, register: F) -> Result<()>
    where
        F: FnOnce() -> T,
        T: Future<Output = Result<()>>,
    {
        let mut registered = self.registered.lock().await;
        if !*registered {
            register().await?;
            *registered = true;
        }
        Ok(())
    }
    /// The service reserves locally before either queued polling or manual sandbox creation.
    pub fn try_queued(self: &Arc<Self>) -> Option<QueuedPermit> {
        let mut used = self.occupancy.lock().unwrap();
        if used.queued + used.manual.len() as u32 >= self.slots() {
            return None;
        }
        used.queued += 1;
        Some(QueuedPermit(self.clone()))
    }
    pub fn try_manual(&self, id: Uuid) -> bool {
        let mut used = self.occupancy.lock().unwrap();
        if used.manual.contains(&id) {
            return true;
        }
        if used.queued + used.manual.len() as u32 >= self.slots() {
            return false;
        }
        used.manual.insert(id);
        true
    }
    /// Restore outstanding manual ownership before any new admission, even after a shrink.
    pub fn restore_manual(&self, id: Uuid) {
        self.occupancy.lock().unwrap().manual.insert(id);
    }
    pub fn release_manual(&self, id: Uuid) {
        self.occupancy.lock().unwrap().manual.remove(&id);
        self.freed.notify_waiters();
    }
    /// Waits, without polling, until a queued attempt could get a permit (`try_queued`); at once
    /// against a server without node slots, where no permit is taken.
    pub async fn room(&self) {
        loop {
            let freed = self.freed.notified();
            if !self.aware() || !self.full() {
                return;
            }
            freed.await;
        }
    }
    pub fn full(&self) -> bool {
        let used = self.occupancy.lock().unwrap();
        used.queued + used.manual.len() as u32 >= self.slots()
    }
}

/// Verify a held directory descriptor still names the same private state root.
pub fn check_state_root(path: &std::path::Path, held: &std::fs::File) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let a = held.metadata()?;
    let b = std::fs::symlink_metadata(path)?;
    ensure!(
        b.is_dir()
            && a.dev() == b.dev()
            && a.ino() == b.ino()
            && b.uid() == unsafe { libc::geteuid() }
            && b.mode() & 0o7777 == 0o700,
        "native state directory changed"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn registration_is_once_even_with_failures_siblings_and_count_changes() {
        let node = Arc::new(NodeSession::default());
        let calls = AtomicU32::new(0);
        let boot = node.boot_id;
        assert!(
            node.register_once(|| async {
                calls.fetch_add(1, Ordering::SeqCst);
                anyhow::bail!("refused")
            })
            .await
            .is_err()
        );
        let register = || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        };
        let (a, b) = tokio::join!(node.register_once(register), node.register_once(register));
        a.unwrap();
        b.unwrap();
        node.slots.store(3, Ordering::SeqCst);
        node.register_once(register).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(node.boot_id, boot);
    }
    #[test]
    fn manual_and_queued_share_count_including_shrink() {
        let node = Arc::new(NodeSession::default());
        node.slots.store(3, Ordering::SeqCst);
        let a = node.try_queued().unwrap();
        let id = Uuid::new_v4();
        assert!(node.try_manual(id));
        let b = node.try_queued().unwrap();
        assert!(node.try_queued().is_none());
        assert!(!node.try_manual(Uuid::new_v4()));
        node.slots.store(1, Ordering::SeqCst);
        drop(a);
        drop(b);
        assert!(node.try_queued().is_none());
        node.release_manual(id);
        assert!(node.try_queued().is_some());
    }
    const SECOND: u64 = 1_000_000_000;
    /// What every slot's poll does under the node's gate (the runner's `node_inventory`).
    fn poll(state: &InventoryState, now: u64, published: &mut Vec<u64>) -> u64 {
        let need = state.poll_need(30_000);
        state.current(now * SECOND, need).unwrap_or_else(|| {
            let sequence = state.next(0).unwrap();
            state.record(sequence, (now + 30) * SECOND);
            published.push(now);
            sequence
        })
    }
    #[test]
    fn eight_idle_slots_share_one_report_renewed_only_near_expiry() {
        let state = Arc::new(InventoryState::default());
        let mut published = Vec::new();
        // One of eight slots polls each second for a minute: 60 polls, each slot every 8 s.
        for now in 1..=60 {
            let sequence = poll(&state, now, &mut published);
            assert_eq!(
                sequence,
                published.len() as u64,
                "every slot polls with the node's sequence"
            );
        }
        // The first report, then one per half TTL (30 s): never one per poll or per slot.
        assert_eq!(published, [1, 16, 31, 46]);
    }
    #[test]
    fn a_sibling_poll_does_not_renew_under_a_slot_in_flight_until_the_floor() {
        let state = Arc::new(InventoryState::default());
        let mut published = Vec::new();
        assert_eq!(poll(&state, 1, &mut published), 1);
        // A slot got its offer under sequence 1 and is accepting and preparing.
        let accepting = state.flight();
        for now in 2..=25 {
            assert_eq!(
                poll(&state, now, &mut published),
                1,
                "sibling poll at {now} s"
            );
        }
        assert_eq!(published, [1], "no TTL renewal overlapped the accept");
        // The pre-execution check of that slot (a third of the TTL) still finds it live at 20 s.
        assert_eq!(state.current(20 * SECOND, 10 * SECOND), Some(1));
        // Only within 5 s of expiry (31 s) is the report renewed regardless.
        assert_eq!(poll(&state, 27, &mut published), 2);
        drop(accepting);
        let other = InventoryState::default();
        assert_eq!(poll(&other, 1, &mut published), 1);
        assert_eq!(
            poll(&other, 20, &mut published),
            2,
            "idle: renewed at half the TTL"
        );
    }
    #[tokio::test(start_paused = true)]
    async fn a_slot_without_room_waits_for_the_permit_instead_of_spinning() {
        let node = Arc::new(NodeSession::default());
        node.room().await; // a server without node slots takes no permit
        node.aware.store(true, Ordering::SeqCst);
        let held = node.try_queued().unwrap();
        let waiting = tokio::spawn({
            let node = node.clone();
            async move { node.room().await }
        });
        tokio::time::sleep(std::time::Duration::from_secs(600)).await;
        assert!(!waiting.is_finished());
        drop(held);
        waiting.await.unwrap();
        assert!(node.try_queued().is_some());
    }
    #[test]
    fn inventory_is_monotone_across_slot_producers_and_retries() {
        let state = InventoryState::default();
        assert_eq!(state.next(0).unwrap(), 1);
        assert_eq!(state.next(0).unwrap(), 2);
        assert_eq!(state.next(5).unwrap(), 6);
        assert!(state.next(i64::MAX as u64).is_err());
    }
}
