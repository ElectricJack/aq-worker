//! Bounded installed permission metadata, never a grant or execution authority.
use aq_session_protocol::Digest;
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

pub fn permission_plans_valid(primary: &Digest, additional: &[Digest], supported: bool) -> bool {
    additional.len() <= 31
        && (supported || additional.is_empty())
        && additional.iter().all(|d| d != primary)
        && additional
            .iter()
            .enumerate()
            .all(|(i, d)| !additional[..i].contains(d))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn explicit_plans_are_distinct_bounded_and_opted_in() {
        let a = Digest::of(b"scratch");
        let b = Digest::of(b"aq-server");
        assert!(permission_plans_valid(&a, &[], false));
        assert!(permission_plans_valid(&a, std::slice::from_ref(&b), true));
        assert!(!permission_plans_valid(&a, &[b.clone()], false));
        assert!(!permission_plans_valid(&a, &[a.clone()], true));
        assert!(!permission_plans_valid(&a, &[b.clone(), b], true));
        assert!(!permission_plans_valid(
            &a,
            &vec![Digest::of(b"x"); 32],
            true
        ));
    }
}
