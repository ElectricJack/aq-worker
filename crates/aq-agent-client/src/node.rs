//! Node-only credential issuance/recovery. These operations are absent from aq-agent.
use crate::client::{AgentEgressCredential, Http, SessionCredential};
use anyhow::{Result, ensure};
use aq_agent_api::{Binding, Context, canonical, valid};
use reqwest::Method;
use serde_json::{Value, json};
use uuid::Uuid;

pub struct NodeClient {
    pub(crate) node_id: Uuid,
    pub(crate) origin: String,
    credential: String,
    edge: Option<AgentEgressCredential>,
    http: Http,
}
impl NodeClient {
    pub fn new(
        origin: &str,
        node_id: Uuid,
        credential: String,
        edge: Option<AgentEgressCredential>,
    ) -> Result<Self> {
        ensure!(
            !node_id.is_nil()
                && !credential.starts_with("aqa_")
                && (32..=4096).contains(&credential.len())
                && !credential.chars().any(char::is_whitespace),
            "node credential class/bound refused"
        );
        Ok(Self {
            node_id,
            origin: origin.into(),
            credential,
            edge,
            http: Http::new(origin)?,
        })
    }
    pub(crate) fn distinct_agent_egress(&self, edge: Option<&AgentEgressCredential>) -> Result<()> {
        ensure!(
            self.edge.is_none() || edge.is_some(),
            "dedicated agent egress credential required"
        );
        if let (Some(node), Some(agent)) = (&self.edge, edge) {
            ensure!(
                node.client_id != agent.client_id && node.client_secret != agent.client_secret,
                "node egress credential cannot be distributed to an agent"
            );
        }
        Ok(())
    }
    pub(crate) async fn issue(
        &self,
        binding: &Binding,
        expected_version: u64,
    ) -> Result<SessionCredential> {
        binding.validate()?;
        ensure!(binding.node_id == self.node_id, "issuance node mismatch");
        let request = json!({"identity":{"node_id":binding.node_id,"boot_id":binding.boot_id},
            "attempt":{"attempt_id":binding.attempt_id,"session_id":binding.session_id,"generation":binding.generation},
            "expected_credential_version":expected_version});
        let (status, value) = self
            .http
            .exchange(
                Method::POST,
                "/v2/worker/agent-session",
                &self.credential,
                self.edge.as_ref(),
                Some(canonical(&request)?),
                1_048_576,
            )
            .await?;
        ensure!(
            status == 200,
            "credential issuance refused or unconfirmed; plaintext cannot be recovered"
        );
        let issued: SessionCredential = serde_json::from_value(value)
            .map_err(|_| anyhow::anyhow!("invalid credential issuance response"))?;
        Context::new(issued.context.clone(), binding)?;
        ensure!(
            issued.api_version == 1
                && issued.token.starts_with("aqa_")
                && (36..=4096).contains(&issued.token.len())
                && !issued.token.chars().any(char::is_whitespace)
                && issued.context["credential"]["version"]
                    .as_str()
                    .and_then(|s| s.parse::<u64>().ok())
                    == expected_version.checked_add(1),
            "credential issuance association mismatch"
        );
        Ok(issued)
    }
    /// Recover only existing centrally retained intents. This never rotates an
    /// agent credential, changes a payload or renews ordinary agent writes.
    pub async fn recover(
        &self,
        binding: &Binding,
        request: Value,
        retry_one: bool,
    ) -> Result<Value> {
        binding.validate()?;
        let schema = if retry_one {
            "RecoveryRetry"
        } else {
            "RecoveryRequest"
        };
        let attempt = json!({"attempt_id":binding.attempt_id,"session_id":binding.session_id,"generation":binding.generation});
        ensure!(
            binding.node_id == self.node_id
                && valid(schema, &request)
                && request["node_id"] == binding.node_id.to_string()
                && request["attempt"] == attempt,
            "node recovery scope/contract refused"
        );
        let path = if retry_one {
            "/v2/worker/agent-operations/retry"
        } else {
            "/v2/worker/agent-operations/query"
        };
        let (status, response) = self
            .http
            .exchange(
                Method::POST,
                path,
                &self.credential,
                self.edge.as_ref(),
                Some(canonical(&request)?),
                1_048_576,
            )
            .await?;
        ensure!(
            status == 200
                && valid("RecoveryResponse", &response)
                && response["request_id"] == request["request_id"]
                && response["attempt"] == attempt,
            "node recovery response unconfirmed"
        );
        let operations = response["operations"].as_array().unwrap();
        if request["mode"] == "lookup" {
            ensure!(
                response["next_cursor"].is_null(),
                "lookup response cannot create a pagination scope"
            );
        } else {
            ensure!(
                operations.len() as u64 <= request["limit"].as_u64().unwrap(),
                "node list response exceeded requested page bound"
            );
        }
        ensure!(
            response["next_cursor"].is_null() || response["coverage"]["status"] == "partial",
            "continuation cursor cannot claim complete coverage"
        );
        let mut seen = std::collections::BTreeSet::new();
        for operation in operations {
            let operation_id: Uuid = operation["operation_id"].as_str().unwrap().parse()?;
            ensure!(
                seen.insert(operation_id),
                "node recovery repeated an operation ID"
            );
            crate::journal::check_operation_response(
                operation,
                operation_id,
                operation["payload_digest"].as_str().unwrap(),
                binding,
            )?;
            ensure!(
                operation["binding"] == serde_json::to_value(binding)?,
                "node recovery returned another scope"
            );
            if request["mode"] == "lookup" {
                ensure!(
                    request["operation_ids"]
                        .as_array()
                        .unwrap()
                        .contains(&operation["operation_id"]),
                    "node lookup returned unrequested operation"
                );
            }
        }
        Ok(response)
    }
}
