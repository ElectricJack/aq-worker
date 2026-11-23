async fn assert_private_auth(h: &Inputs) -> String {
    let state = h.pipeline().snapshot().await.unwrap();
    let history = state.attempts[&h.id].native_host.as_ref().unwrap();
    assert!(history.baseline.is_some() && history.running.is_some());
    eprintln!(
        "fixture physical Running observed_boot_ms={} started_ack={}",
        history
            .running
            .as_ref()
            .unwrap()
            .observation
            .observed_boot_ms,
        history
            .terminal
            .as_ref()
            .is_some_and(|v| v.started.receipt.is_some())
    );
    let auth = history
        .intent
        .host_root
        .join(history.intent.launch_id.to_string())
        .join("codex-home/auth.json");
    assert_eq!(
        fs::metadata(&auth).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let private: Value = serde_json::from_slice(&fs::read(auth).unwrap()).unwrap();
    let capability = private["OPENAI_API_KEY"].as_str().unwrap().to_owned();
    assert!(capability.len() >= 16 && capability.bytes().all(|b| b.is_ascii_graphic()));
    assert!(
        capability != "synthetic-upstream-key" && capability != "synthetic-key",
        "child capability must be fresh, not an upstream/source credential"
    );
    capability
}

fn user_input(request: &Value) -> Vec<&str> {
    request["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|v| v["role"] == "user")
        .flat_map(|v| v["content"].as_array().unwrap())
        .filter_map(|v| v["text"].as_str())
        .collect()
}
fn check_provider_input(request: &Value) {
    let user = user_input(request);
    assert_eq!(
        user.len(),
        2,
        "only observed CLI environment and exact stdin user messages"
    );
    assert!(user[0].starts_with("<environment_context>\n"));
    assert!(
        user[0].contains("<permission_profile type=\"managed\">")
            && user[0].contains("<file_system type=\"restricted\">")
    );
    assert_eq!(user[1], "instructions\n\ntask-context");
    assert_eq!(
        user.iter()
            .filter(|s| **s == "instructions\n\ntask-context")
            .count(),
        1
    );
}
fn check_ready_output(output: &OutputPeer) {
    let uploads = output.uploads.lock().unwrap();
    let q: PeerUpload = serde_json::from_slice(&uploads[0]).unwrap();
    let bytes: Vec<u8> = q.frames.iter().flat_map(|f| f.bytes.clone()).collect();
    let literal = String::from_utf8_lossy(&bytes);
    assert!(
        literal.contains("thread.started") && literal.contains("turn.started"),
        "genuine CLI JSONL readiness must be observed through physical tmux bytes: {literal}"
    );
    assert!(
        !literal.contains("synthetic-key"),
        "scoped private auth must not appear in terminal output"
    );
}
async fn fixture_workspace(h: &Inputs) -> crate::source::Workspace {
    let state = h.pipeline().snapshot().await.unwrap();
    let created = state.attempts[&h.id].source.as_ref().unwrap().operations["create"]
        .result
        .as_ref()
        .unwrap();
    serde_json::from_value(created.clone()).unwrap()
}
fn tool_result(request: &Value) -> anyhow::Result<()> {
    let outputs: Vec<_> = request["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|v| v["type"] == "function_call_output" && v["call_id"] == "call_aq_local")
        .collect();
    anyhow::ensure!(
        outputs.len() == 1,
        "actual CLI must return the exact controlled tool call"
    );
    let output = serde_json::to_string(&outputs[0]["output"]).unwrap();
    anyhow::ensure!(
        output.contains("aq-local-tool-proof"),
        "actual local tool did not produce expected output: {output}"
    );
    Ok(())
}
async fn wait_for_requests(peer: &Provider, count: usize) {
    while peer.requests.lock().unwrap().len() < count {
        peer.entered.notified().await;
    }
}
async fn expect_clean_stop(h: &Inputs) -> crate::native::journal::State {
    stopped(h, true).await;
    let state = h.pipeline().snapshot().await.unwrap();
    assert_eq!(
        state.attempts[&h.id]
            .native_host
            .as_ref()
            .unwrap()
            .stopped
            .as_ref()
            .unwrap()
            .observation
            .root_status,
        Some(0)
    );
    state
}
fn check_named_output(output: &OutputPeer, capability: &str) -> anyhow::Result<()> {
    let uploads = output.uploads.lock().unwrap();
    let request: PeerUpload = serde_json::from_slice(&uploads[0]).unwrap();
    let literal: Vec<u8> = request
        .frames
        .iter()
        .flat_map(|f| f.bytes.clone())
        .collect();
    let literal = String::from_utf8_lossy(&literal);
    anyhow::ensure!(
        literal.contains("thread.started") && literal.contains("turn.started"),
        "genuine named CLI readiness must be observed"
    );
    assert!(!literal.contains(capability) && !literal.contains("synthetic-upstream-key"));
    Ok(())
}
