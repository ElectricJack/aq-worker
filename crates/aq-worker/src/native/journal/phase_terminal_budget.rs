use super::*;
pub const HOST_HISTORY_BUDGET: usize = 1024 * 1024;
pub const BASE_HISTORY_MAX: usize = 512 * 1024;
pub(super) fn remaining_reserve(state: &State) -> Result<usize> {
    let mut reserve = 20usize - state.revision.to_string().len();
    for a in state.attempts.values() {
        if let Some(h) = &a.native_host {
            let used = canonical_bytes(h)?.len();
            ensure!(
                used <= HOST_HISTORY_BUDGET,
                "native host reserved history bound"
            );
            reserve = reserve
                .checked_add(HOST_HISTORY_BUDGET - used)
                .context("host reserve overflow")?;
        }
    }
    Ok(reserve)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn worst_case_four_envelopes_and_base_fit_reserved_host_budget() {
        // Each independently validated public document may use all64KiB.
        // Private container punctuation + two u64 counters stay below1024bytes.
        const {
            assert!(
                BASE_HISTORY_MAX + 4 * aq_phase_terminal_protocol::MAX_BYTES + 1024
                    < HOST_HISTORY_BUDGET
            )
        };
    }
    #[test]
    fn revision_growth_cannot_spend_reserved_host_or_safety_capacity() {
        let identity = WorkerIdentity {
            node_id: Uuid::from_u128(1),
            boot_id: Uuid::from_u128(2),
        };
        let mut state = State {
            format: FORMAT,
            identity,
            kernel_boot_id: Uuid::from_u128(3),
            revision: 1,
            attempts: BTreeMap::new(),
        };
        let charged = canonical_bytes(&state).unwrap().len() + remaining_reserve(&state).unwrap();
        for revision in [9, 10, 99, 100, u64::MAX] {
            state.revision = revision;
            assert_eq!(
                charged,
                canonical_bytes(&state).unwrap().len() + remaining_reserve(&state).unwrap()
            );
        }
    }
}
