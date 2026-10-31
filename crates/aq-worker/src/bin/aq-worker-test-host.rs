//! Test-only process host. Excluded from normal/release binary selection.
use anyhow::Result;
use aq_worker::{
    clock::{BootClock, Clock},
    journal::{Journal, Limits},
    runtime::Runtime,
};
use aq_worker_protocol::{testing::*, *};
use clap::Parser;
use uuid::Uuid;
#[derive(Parser)]
struct Args {
    #[arg(long)]
    state_dir: std::path::PathBuf,
    #[arg(long)]
    fixture_agent: std::path::PathBuf,
    #[arg(long, default_value = "launch")]
    action: String,
    #[arg(long, default_value_t = 30_000)]
    lease_ms: u64,
    #[arg(long)]
    fault_checkpoint: Option<String>,
    #[arg(long, default_value = "pause")]
    fault_action: String,
}
fn main() -> Result<()> {
    let args = Args::parse();
    if let Some(name) = args.fault_checkpoint {
        aq_worker::fault::configure(name, args.fault_action)?;
    }
    aq_worker::guardian::probe_profile()?;
    let identity = NodeIdentity {
        node_id: Uuid::from_u128(1),
        boot_id: Uuid::new_v4(),
    };
    let limits = Limits {
        capacity: Resources {
            slots: 1,
            cpu_millis: 10000,
            memory_bytes: 1000000,
            disk_bytes: 1000000,
        },
        max_pending_results: 16,
        max_pending_bytes: 65536,
        max_preparing: 1,
        minimum_free_disk_bytes: 0,
    };
    let mut runtime = Runtime::new(
        Journal::open(&args.state_dir, identity.clone(), limits)?,
        BootClock,
    );
    if args.action == "recover" {
        let uncertain = runtime.recover_owned_launches()?;
        println!(
            "{}",
            serde_json::json!({"uncertain":uncertain,"protected_usage":runtime.journal.protected_usage()?,"available":runtime.journal.available()?})
        );
        if !uncertain.is_empty() {
            std::process::exit(2);
        }
        return Ok(());
    }
    let now = BootClock.now_ms();
    let mut offer = sample_offer();
    offer.identity = identity;
    offer.offer_expires_at_ms = now + 60_000;
    offer.reservation_ttl_ms = args.lease_ms;
    let mut peer = FakeCoordinator::new(offer.clone(), now);
    let request = runtime.reserve(offer)?;
    let grant = peer
        .accept(&request)
        .map_err(|e| anyhow::anyhow!("{:?}", e.code))?
        .grant;
    runtime.authorize(grant, now)?;
    aq_worker::fault::checkpoint(runtime.journal.root(), "grant-durable", None)?;
    let mut handle = runtime.launch_fixture(request.attempt.attempt_id, &args.fixture_agent)?;
    while handle.try_wait()?.is_none() {
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    runtime.finish_fixture(request.attempt.attempt_id, handle)?;
    println!("{}", serde_json::to_string(runtime.journal.state())?);
    Ok(())
}
