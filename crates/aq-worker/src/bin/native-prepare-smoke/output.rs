//! Opt-in physical output probe. No network failure may bypass the caller's stop.
use super::*;
use aq_terminal_bridge::http::{NodeCredential, RelayHttp};
use aq_terminal_relay_protocol::{Effect, Peer, RelayPoll};
use aq_worker::native::launch::LocalPhaseHost;

pub(super) async fn run(
    pipeline: &Arc<InputPipeline>,
    host: &LocalPhaseHost,
    config: &WorkerConfig,
    id: Uuid,
    input: &mut impl BufRead,
) -> Result<serde_json::Value> {
    ensure!(
        config.cf_client_id.is_none() && config.cf_client_secret.is_none(),
        "local output probe does not support proxy credentials"
    );
    let credential = NodeCredential::new(config.credential.clone())?;
    let http = RelayHttp::development_loopback(&config.server_url, &credential)?;
    let relay = pipeline.start_phase_output(host, http.clone())?;
    let started = pipeline.report_phase_terminal_started(id).await?;
    let info = relay.register().await?;
    ensure!(
        info.terminal == *relay.terminal(),
        "actual registration identity"
    );
    emit(&json!({"phase":"terminal_registered","terminal_info":info,
        "phase_started_ack":started}))?;
    // The parent establishes a real human Watch, then explicitly continues.
    let next: Continue = frame(input, 1024)?;
    ensure!(next.r#continue, "output probe watch continuation required");
    // Pull the real daemon-owned Watch. The stdin frame coordinates the test;
    // it neither invents a subscription nor substitutes for node-outbound poll.
    let aq_session_protocol::SessionIdentity::Remote {
        node_id, boot_id, ..
    } = &info.terminal.session
    else {
        anyhow::bail!("output fixture requires remote terminal");
    };
    let watch_pull = http
        .poll(&RelayPoll {
            version: 1,
            capability: aq_session_protocol::Text::new(aq_terminal_relay_protocol::CAPABILITY)?,
            peer: Peer::Remote {
                node_id: *node_id,
                boot_id: *boot_id,
            },
            cursor: 0,
            hold_ms: 0,
            active_subscriptions: 1,
            active_controllers: 0,
            advertised_capabilities: vec![Capability {
                id: aq_session_protocol::Text::new(aq_terminal_relay_protocol::CAPABILITY)?,
                version: 1,
            }],
        })
        .await?;
    ensure!(
        watch_pull.effects.len() == 1
            && matches!(&watch_pull.effects[0], Effect::Watch(w) if w.terminal == info.terminal),
        "actual daemon Watch pull required before output publication"
    );
    // The opt-in fixture emits a fixed tick every 250 ms. This observation
    // window does not alter the original grant or guardian deadline.
    tokio::time::sleep(std::time::Duration::from_millis(350)).await;
    let mut ack = None;
    for _ in 0..50 {
        if let Some(value) = relay.upload_once().await? {
            ack = Some(value);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let ack = ack.context("actual post-Running output upload required")?;
    let status = relay.status()?;
    ensure!(
        status.next_sequence > 1 && status.retained_bytes > 0,
        "actual output producer did not retain bytes"
    );
    Ok(json!({"terminal_info":info,"phase_started_ack":started,
        "watch_pull":watch_pull,"upload_ack":ack,"next_sequence":status.next_sequence,
        "retained_bytes":status.retained_bytes}))
}
