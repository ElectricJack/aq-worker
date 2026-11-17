//! The closed result adapter profiles.
use serde::{Deserialize, Serialize};
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ResultAdapterKind {
    #[serde(rename = "codex-0.166-trusted-node-named-readonly-result-local-v1")]
    Codex0166TrustedNodeNamedReadonlyLocalV1,
    #[serde(rename = "codex-0.166-chatgpt-interactive-result-local-v1")]
    Codex0166ChatgptInteractiveLocalV1,
    /// MS1: Claude Code in a docker-local sandbox driven from the node.
    #[serde(rename = "claude-2.1.318-docker-local-result-local-v1")]
    Claude2318DockerLocalV1,
}
