//! Test-only helper executable for this crate's process tests. Production runs the helper
//! from the worker's own executable (`aq-worker session-host`).
fn main() -> anyhow::Result<()> {
    aq_session_host::helper_main()
}
