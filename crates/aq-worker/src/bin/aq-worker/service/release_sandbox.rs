use super::*;

#[derive(Args)]
pub struct ReleaseSandbox {
    #[arg(long)]
    profile: String,
    /// Enrolled node index. Without this flag, --slot keeps its historical node-index meaning.
    #[arg(long)]
    node: Option<u32>,
    #[arg(long)]
    slot: u32,
    #[arg(long)]
    attempt_id: Uuid,
    #[arg(long)]
    reason: String,
    /// Release it even while the slot's recovery still holds the attempt for salvage.
    #[arg(long)]
    force: bool,
}
impl ReleaseSandbox {
    pub(super) async fn run(self) -> Result<()> {
        let Self {
            profile,
            node: node_index,
            slot,
            attempt_id: attempt,
            reason,
            force,
        } = self;
        let (machine, config) = loaded(&profile)?;
        let (node_index, slot) = node_index.map_or((slot, 1), |n| (n, slot));
        ensure!(
            machine.enrolled_nodes().contains(&node_index),
            "node {node_index} has no enrollment"
        );
        ensure!(
            (1..=aq_protocol::MAX_NODE_SLOTS).contains(&slot),
            "invalid agent slot"
        );
        // Retired nodes remain addressable: their old attempts still belong to their identity.
        let layout = machine.for_node(node_index);
        let node = EnrollmentFile::load(&layout.slot_enrollment(slot))?.node_id;
        // Only the executor settings matter here; the slot's state directory stands in for the
        // attempt's own (which may be archived).
        let state_dir = std::fs::canonicalize(layout.slot_state(slot))?;
        let ctx = SlotContext {
            slot,
            node_id: node,
            enrollment: layout.slot_enrollment(slot),
            git_socket: layout.git_socket(slot),
            host_root: state_dir.join("host"),
            state_dir,
            hold: std::time::Duration::ZERO,
        };
        let cfg = slot_native_config(&config.native_template, &ctx)?;
        let current = layout.current(slot);
        let retained = || {
            let at = SlotContext {
                state_dir: std::fs::canonicalize(&current)?,
                ..ctx.clone()
            };
            retained_attempts(&slot_native_config(&config.native_template, &at)?, node)
        };
        ensure!(
            force || !held_for_salvage(&current, attempt, retained),
            "attempt {attempt} is still held for salvage by slot {slot}'s recovery; its sandbox holds unsaved work. Pass --force to release it anyway"
        );
        let released =
            aq_worker::native::runner::release_preserved_sandbox(&cfg, node, attempt, &reason)
                .await?;
        println!(
            "operator release: attempt {attempt} sandbox(es) {} destroyed; reason recorded as destroy evidence",
            released.join(", ")
        );
        Ok(())
    }
}
