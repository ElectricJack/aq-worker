//! Pure finite-fixture data adapter. No network, execution, config or live owner.
use anyhow::{Result, ensure};
use aq_finite_responses as shared;
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::{Read, Write};

#[derive(Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
enum Input {
    Vectors {
        model: String,
        commands: Vec<Command>,
        completion: String,
    },
    Poll {
        model: String,
        response_id: String,
        item_id: String,
        call_id: String,
        session_id: i32,
        offered_tools: Vec<Value>,
    },
    Check {
        model: String,
        output_cap: u64,
        forbidden_material: Vec<String>,
        body_utf8: String,
    },
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Command {
    response_id: String,
    item_id: String,
    call_id: String,
    command: String,
}
fn text(value: &str, maximum: usize) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= maximum,
        "bounded fixture text"
    );
    Ok(())
}
fn identifier(value: &str) -> Result<()> {
    text(value, 128)?;
    ensure!(
        value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
        "fixed fixture identifier"
    );
    Ok(())
}
fn apply(input: Input) -> Result<Value> {
    match input {
        Input::Vectors {
            model,
            commands,
            completion,
        } => {
            text(&model, 128)?;
            text(&completion, 4096)?;
            ensure!(
                !commands.is_empty() && commands.len() <= 4,
                "finite command count"
            );
            let mut responses = Vec::new();
            for command in commands {
                for id in [&command.response_id, &command.item_id, &command.call_id] {
                    identifier(id)?;
                }
                text(&command.command, 4096)?;
                responses.push(shared::exec_command(
                    &model,
                    &command.response_id,
                    &command.item_id,
                    &command.call_id,
                    &command.command,
                ));
            }
            responses.push(shared::completed(&model, &completion));
            Ok(
                json!({"schema_version":1,"responses":responses,"scope":"finite local fixture data; no model/provider qualification"}),
            )
        }
        Input::Poll {
            model,
            response_id,
            item_id,
            call_id,
            session_id,
            offered_tools,
        } => {
            text(&model, 128)?;
            for id in [&response_id, &item_id, &call_id] {
                identifier(id)?;
            }
            ensure!(
                session_id > 0 && offered_tools.len() <= 64,
                "finite actual session/tool bound"
            );
            ensure!(
                offered_tools.iter().any(|tool| tool["type"] == "function"
                    && tool["name"] == "write_stdin"
                    && tool["parameters"]["properties"]["session_id"].is_object()
                    && tool["parameters"]["properties"]["chars"].is_object()),
                "actual write_stdin tool must be offered"
            );
            Ok(
                json!({"schema_version":1,"response":shared::write_stdin(&model,&response_id,&item_id,&call_id,session_id)}),
            )
        }
        Input::Check {
            model,
            output_cap,
            forbidden_material,
            body_utf8,
        } => {
            text(&model, 128)?;
            ensure!((16..=1_048_576).contains(&output_cap), "fixture cap bound");
            ensure!(
                forbidden_material.len() <= 8,
                "bounded forbidden material count"
            );
            for value in &forbidden_material {
                text(value, 16_384)?;
            }
            let forbidden: Vec<_> = forbidden_material.iter().map(String::as_str).collect();
            let request =
                shared::request(body_utf8.as_bytes(), Some(&model), output_cap, &forbidden)?;
            // Return metadata only. The parent HTTP fixture retains its own exact
            // bytes for tool-result assertions; this program never dumps inputs.
            Ok(
                json!({"schema_version":1,"valid":true,"body_bytes":body_utf8.len(),
                "body_digest":aq_session_protocol::Digest::of(body_utf8.as_bytes()),
                "model":request["model"],"output_cap":request["max_output_tokens"]}),
            )
        }
    }
}
pub(super) fn run() -> Result<()> {
    // The caller owns a finite subprocess pipe and closes it after one request.
    // Bound encoded JSON as well as the shared helper's actual HTTP body bound.
    const MAX_INPUT: usize = 8 * 1024 * 1024;
    let mut bytes = Vec::new();
    std::io::stdin()
        .lock()
        .take((MAX_INPUT + 1) as u64)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= MAX_INPUT, "bounded fixture adapter request");
    let output = apply(serde_json::from_slice(&bytes)?)?;
    let mut writer = std::io::stdout().lock();
    serde_json::to_writer(&mut writer, &output)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn export_delegates_exact_shared_vectors_for_separate_edit_and_claim_turns() {
        let model = "gpt-5.1-codex-mini";
        let edit = "/usr/bin/printf gate04-proof > gate04-proof.txt";
        let claim = "\"$AQ_AGENT_RESULT_COMMAND\" result submit --request-file \"$AQ_AGENT_RESULT_DIR/submissions/request.json\"";
        let actual = apply(Input::Vectors {
            model: model.into(),
            commands: vec![
                Command {
                    response_id: "resp_edit".into(),
                    item_id: "item_edit".into(),
                    call_id: "call_edit".into(),
                    command: edit.into(),
                },
                Command {
                    response_id: "resp_claim".into(),
                    item_id: "item_claim".into(),
                    call_id: "call_claim".into(),
                    command: claim.into(),
                },
            ],
            completion: "claim pending; ending turn".into(),
        })
        .unwrap();
        assert_eq!(
            actual["responses"],
            json!([
                shared::exec_command(model, "resp_edit", "item_edit", "call_edit", edit),
                shared::exec_command(model, "resp_claim", "item_claim", "call_claim", claim),
                shared::completed(model, "claim pending; ending turn"),
            ])
        );
    }
    #[test]
    fn poll_requires_offered_tool_and_preserves_actual_session() {
        let poll = |tools| Input::Poll {
            model: "m".into(),
            response_id: "resp_poll_1".into(),
            item_id: "item_poll_1".into(),
            call_id: "call_poll_1".into(),
            session_id: 723,
            offered_tools: tools,
        };
        assert!(apply(poll(vec![])).is_err());
        let actual = apply(poll(vec![json!({"type":"function","name":"write_stdin","parameters":{"properties":{"session_id":{"type":"integer"},"chars":{"type":"string"}}}})])).unwrap();
        assert_eq!(
            actual["response"],
            shared::write_stdin("m", "resp_poll_1", "item_poll_1", "call_poll_1", 723)
        );
    }
    #[test]
    fn request_adapter_returns_only_metadata_and_refuses_credential_or_model_drift() {
        let body=json!({"model":"m","stream":true,"store":false,"max_output_tokens":4096,"tools":[],"input":[{"text":"private task text"}]}).to_string();
        let request = |body_utf8, model| Input::Check {
            model,
            output_cap: 4096,
            forbidden_material: vec!["node-secret".into()],
            body_utf8,
        };
        let checked = apply(request(body.clone(), "m".into())).unwrap();
        assert_eq!(checked["valid"], true);
        assert!(!checked.to_string().contains("private task text"));
        assert!(apply(request(body.clone(), "other".into())).is_err());
        assert!(
            apply(request(
                body.replace("private task text", "node-secret"),
                "m".into()
            ))
            .is_err()
        );
        assert!(serde_json::from_str::<Input>(r#"{"operation":"check","model":"m","output_cap":4096,"forbidden_material":[],"body_utf8":"{}","execute":true}"#).is_err());
    }
}
