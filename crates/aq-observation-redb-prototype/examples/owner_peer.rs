//! Qualification example only: never prints or accepts credential values in argv.
use anyhow::{Context, Result, ensure};
use aq_observation_redb_prototype::{
    HttpOwnerConfig, HttpOwnerPeer, Limits, LocalIntent, LocalStage,
};
use std::path::Path;
use uuid::Uuid;
fn main() {
    if let Err(e) = run() {
        eprintln!("owner action failed: {e}");
        std::process::exit(1)
    }
}
fn run() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    ensure!(
        args.len() == 4 || args.len() == 5 || args.len() == 6,
        "usage: owner_peer PRIVATE_CONFIG OWNER_ROOT ACTION [OPERATION_ID [STORE_ID]]"
    );
    let config = HttpOwnerConfig::load(Path::new(&args[1]))?;
    let mut peer = HttpOwnerPeer::from_config(&config)?;
    let root = Path::new(&args[2]);
    let action = args[3].as_str();
    // The example accepts only explicit UUIDs; unknown replies never generate a
    // fresh operation ID or hide a mutation retry inside transport code.
    let op = || -> Result<Uuid> { Ok(args.get(4).context("operation ID required")?.parse()?) };
    let mut intent = if action == "initialize" {
        ensure!(args.len() == 6, "initialize needs operation and store IDs");
        let mut limits = Limits::fixture();
        limits.node_id = config.node_id;
        limits.boot_id = config.boot_id;
        LocalIntent::create(root, limits, op()?, args[5].parse()?)?
    } else {
        LocalIntent::open(root, config.boot_id)?
    };
    match action {
        "initialize" | "prepare" => intent.prepare(&mut peer)?,
        "create" => intent.create_store(&mut peer)?,
        "activate" => intent.activate(&mut peer, op()?)?,
        "resume" => intent.resume(&mut peer)?,
        "advance" => intent.advance_boot(&mut peer, op()?, config.boot_id, 1)?,
        "recover-boot" => intent.recover_boot(&mut peer, 1)?,
        _ => anyhow::bail!("unknown owner action"),
    }
    let summary = if intent.stage() == LocalStage::Active {
        intent.with_active(|s|{let p=s.snapshot()?;Ok(serde_json::json!({"stage":intent.stage(),"active_boot":p.active_boot,"next_sequence":p.next,"accepted_through":p.acked,"queued":p.queued}))})?
    } else {
        serde_json::json!({"stage":intent.stage()})
    };
    println!("{}", serde_json::to_string(&summary)?);
    Ok(())
}
