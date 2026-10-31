//! Explicit local integration checkpoints. Never enabled by normal startup.
use anyhow::Result;
use std::path::Path;
use uuid::Uuid;
static CONFIG: std::sync::OnceLock<(String, String)> = std::sync::OnceLock::new();
pub fn configure(name: String, action: String) -> Result<()> {
    anyhow::ensure!(
        [
            "grant-durable",
            "launch-intent-durable",
            "guardian-spawned",
            "guardian-identity-durable",
            "start-intent-durable",
            "start-sent-before-receipt",
            "start-receipt-durable",
            "report-durable"
        ]
        .contains(&name.as_str()),
        "unknown fault checkpoint"
    );
    anyhow::ensure!(
        ["crash", "pause"].contains(&action.as_str()),
        "unknown fault action"
    );
    CONFIG
        .set((name, action))
        .map_err(|_| anyhow::anyhow!("fault configuration already set"))?;
    Ok(())
}
pub fn checkpoint(root: &Path, name: &str, launch_id: Option<Uuid>) -> Result<()> {
    let selected = CONFIG
        .get()
        .map(|c| c.0.clone())
        .or_else(|| std::env::var("AQ_WORKER_FAULT_CHECKPOINT").ok());
    if selected.as_deref() != Some(name) {
        return Ok(());
    }
    let directory = root.join("faults");
    std::fs::create_dir_all(&directory)?;
    crate::guardian::durable_replace(
        &directory.join(format!("{name}.json")),
        &serde_json::to_vec(
            &serde_json::json!({"checkpoint":name,"launch_id":launch_id,"pid":std::process::id()}),
        )?,
    )?;
    let action = CONFIG
        .get()
        .map(|c| c.1.clone())
        .or_else(|| std::env::var("AQ_WORKER_FAULT_ACTION").ok())
        .unwrap_or_else(|| "crash".into());
    match action.as_str() {
        "crash" => std::process::exit(86),
        "pause" => {
            while !directory.join(format!("{name}.release")).exists() {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        action => anyhow::bail!("unsupported fault action {action}"),
    }
    Ok(())
}
