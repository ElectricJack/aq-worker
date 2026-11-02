//! Shared bounded Responses fixture data. No listener, credential owner, model
//! inference, process launch or admission authority. Usage values are synthetic.
use anyhow::{Result, ensure};
use serde_json::{Value, json};

pub const MAX_REQUEST_BYTES: usize = 1024 * 1024;

fn event(value: Value) -> String {
    format!(
        "event: {}\ndata: {}\n\n",
        value["type"].as_str().expect("fixture event type"),
        value
    )
}

/// The same complete-message sequence used by the existing Codex tool fixture.
pub fn completed(model: &str, text: &str) -> String {
    let item = json!({"id":"msg_aq_local", "type":"message", "status":"completed",
        "role":"assistant", "content":[{"type":"output_text", "text":text,"annotations":[]}]});
    [
        event(json!({"type":"response.created","response":{"id":"resp_aq_local","model":model,"status":"in_progress","output":[]}})),
        event(json!({"type":"response.output_item.added","output_index":0,"item":{"id":"msg_aq_local","type":"message","status":"in_progress","role":"assistant","content":[]}})),
        event(json!({"type":"response.output_text.delta","item_id":"msg_aq_local","output_index":0,"content_index":0,"delta":text})),
        event(json!({"type":"response.output_item.done","output_index":0,"item":item})),
        event(json!({"type":"response.completed","response":{"id":"resp_aq_local","model":model,"status":"completed","output":[item],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2,"input_tokens_details":{"cached_tokens":0},"output_tokens_details":{"reasoning_tokens":0}}}})),
    ].concat()
}

/// Separate fixed fixture turns can supply distinct IDs without implementing a
/// second provider or changing the old tool-call recipe.
pub fn exec_command(
    model: &str,
    response_id: &str,
    item_id: &str,
    call_id: &str,
    command: &str,
) -> String {
    let args = json!({"cmd":command,"login":false,"yield_time_ms":1000,"max_output_tokens":1000})
        .to_string();
    function_call(model, response_id, item_id, call_id, "exec_command", args)
}

/// A finite empty-input poll. Caller must derive this ID from the actual prior
/// tool output and confirm the actual request offers write_stdin.
pub fn write_stdin(
    model: &str,
    response_id: &str,
    item_id: &str,
    call_id: &str,
    session_id: i32,
) -> String {
    function_call(
        model,
        response_id,
        item_id,
        call_id,
        "write_stdin",
        json!({"session_id":session_id,"chars":"","yield_time_ms":1000,"max_output_tokens":2048})
            .to_string(),
    )
}
fn function_call(
    model: &str,
    response_id: &str,
    item_id: &str,
    call_id: &str,
    name: &str,
    args: String,
) -> String {
    let item = json!({"type":"function_call","id":item_id,"call_id":call_id,
        "name":name,"arguments":args,"status":"completed"});
    [
        event(json!({"type":"response.created","response":{"id":response_id,"model":model,"status":"in_progress","output":[]}})),
        event(json!({"type":"response.output_item.done","output_index":0,"item":item})),
        event(json!({"type":"response.completed","response":{"id":response_id,"model":model,"status":"completed","output":[item],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}})),
    ].concat()
}

/// Validates the finite peer's observed request. HTTP authentication, actual
/// scope/credential ownership, request-count gates and transport cleanup belong
/// to its caller. This function produces data only, never a qualified model fact.
pub fn request(
    body: &[u8],
    expected_model: Option<&str>,
    output_cap: u64,
    forbidden_material: &[&str],
) -> Result<Value> {
    ensure!(body.len() < MAX_REQUEST_BYTES, "finite request byte bound");
    let text = std::str::from_utf8(body)?;
    for material in forbidden_material {
        ensure!(!material.is_empty(), "empty forbidden fixture material");
        ensure!(!text.contains(material), "credential entered model input");
    }
    let value: Value = serde_json::from_slice(body)?;
    ensure!(value["stream"] == true, "finite request must stream");
    ensure!(value["store"] == false, "finite request must be stateless");
    ensure!(
        value["max_output_tokens"] == output_cap,
        "finite output cap"
    );
    let model = value["model"].as_str().filter(|v| !v.is_empty());
    ensure!(model.is_some(), "finite model absent");
    if let Some(expected) = expected_model {
        ensure!(model == Some(expected), "finite model differs");
    }
    let tools = value["tools"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("finite tools absent"))?;
    ensure!(
        tools.iter().all(|v| v["type"] != "web_search"),
        "hosted web search must be disabled"
    );
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn events(bytes: &str) -> Vec<Value> {
        bytes
            .split("\n\n")
            .filter(|frame| !frame.is_empty())
            .map(|frame| {
                let (event, data) = frame.split_once('\n').unwrap();
                let parsed: Value =
                    serde_json::from_str(data.strip_prefix("data: ").unwrap()).unwrap();
                assert_eq!(
                    event,
                    format!("event: {}", parsed["type"].as_str().unwrap())
                );
                parsed
            })
            .collect()
    }

    #[test]
    fn completion_preserves_exact_text_through_sse_delta_and_final_item() {
        let text = "quote\"; newline\n; λ";
        let frames = events(&completed("fixture-model", text));
        assert_eq!(frames.len(), 5);
        assert_eq!(frames[2]["delta"], text);
        assert_eq!(
            frames[4]["response"]["output"][0]["content"][0]["text"],
            text
        );
        assert_eq!(frames[4]["response"]["model"], "fixture-model");
    }

    #[test]
    fn distinct_turns_keep_exact_tool_arguments_and_function_call_identity() {
        for (response, item, call, command) in [
            (
                "resp_edit",
                "item_edit",
                "call_edit",
                "printf 'a\\nb' > proof.txt",
            ),
            (
                "resp_submit",
                "item_submit",
                "call_submit",
                "\"$AQ_AGENT_RESULT_COMMAND\" result status",
            ),
        ] {
            let frames = events(&exec_command(
                "fixture-model",
                response,
                item,
                call,
                command,
            ));
            assert_eq!(frames.len(), 3);
            assert_eq!(frames[0]["response"]["id"], response);
            let actual = &frames[2]["response"]["output"][0];
            assert_eq!(actual, &frames[1]["item"]);
            assert_eq!(actual["id"], item);
            assert_eq!(actual["call_id"], call);
            assert_eq!(actual["name"], "exec_command");
            let arguments: Value =
                serde_json::from_str(actual["arguments"].as_str().unwrap()).unwrap();
            assert_eq!(arguments["cmd"], command);
            assert_eq!(arguments["login"], false);
        }
    }

    #[test]
    fn empty_poll_retains_real_session_identity_without_new_input() {
        let frames = events(&write_stdin(
            "m",
            "resp_poll",
            "item_poll",
            "call_poll",
            723,
        ));
        let call = &frames[1]["item"];
        assert_eq!(call["name"], "write_stdin");
        assert_eq!(call["call_id"], "call_poll");
        let arguments: Value = serde_json::from_str(call["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(
            arguments,
            json!({"session_id":723,"chars":"","yield_time_ms":1000,"max_output_tokens":2048})
        );
        assert_eq!(frames[2]["response"]["output"][0], *call);
    }

    fn valid() -> Value {
        json!({"model":"fixture-model","stream":true,"store":false,"max_output_tokens":4096,
            "tools":[{"type":"function","name":"exec_command"}],"input":[]})
    }

    #[test]
    fn request_refuses_wrong_model_cap_policy_and_credential_material() {
        let good = valid();
        let bytes = serde_json::to_vec(&good).unwrap();
        assert_eq!(
            request(
                &bytes,
                Some("fixture-model"),
                4096,
                &["private-fixture-key"]
            )
            .unwrap(),
            good
        );
        assert!(request(&bytes, Some("other-model"), 4096, &[]).is_err());
        assert!(request(&bytes, None, 1024, &[]).is_err());
        for (key, value) in [
            ("stream", json!(false)),
            ("store", json!(true)),
            ("tools", json!([{"type":"web_search"}])),
            ("model", Value::Null),
        ] {
            let mut bad = valid();
            bad[key] = value;
            assert!(request(&serde_json::to_vec(&bad).unwrap(), None, 4096, &[]).is_err());
        }
        let mut bad = valid();
        bad["input"] = json!(["private-fixture-key"]);
        assert!(
            request(
                &serde_json::to_vec(&bad).unwrap(),
                None,
                4096,
                &["private-fixture-key"]
            )
            .is_err()
        );
        assert!(request(&vec![b' '; MAX_REQUEST_BYTES], None, 4096, &[]).is_err());
    }
}
