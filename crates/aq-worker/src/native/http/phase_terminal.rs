//! Explicit opt-in phase terminal facts; no execution observation/permit decoder.
use super::*;
impl NativeClient {
    pub(super) async fn phase_terminal_post(
        &self,
        path: &str,
        q: &impl Serialize,
        safety: bool,
    ) -> Result<Response> {
        let bytes = serde_json::to_vec(q)?;
        ensure!(
            bytes.len() <= aq_phase_terminal_protocol::MAX_BYTES,
            "phase terminal request bound"
        );
        let client = if safety {
            &self.phase_terminal_safety
        } else {
            &self.control
        };
        let response = client
            .post(format!("{}/v1/worker/profiled-v2/{path}", self.base))
            .header(header::AUTHORIZATION, self.credential.clone())
            .header(header::CONTENT_TYPE, "application/json")
            .header("aq-phase-terminal-api-version", "1")
            .body(bytes)
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("phase terminal transport failed; disposition unknown"))?;
        if !response.status().is_success() {
            return Err(refused(response).await);
        }
        let mut versions = response
            .headers()
            .get_all("aq-phase-terminal-api-version")
            .iter();
        ensure!(
            versions.next().is_some_and(|v| v == "1")
                && versions.next().is_none()
                && response
                    .headers()
                    .get(header::CONTENT_TYPE)
                    .is_some_and(|v| v == "application/json"),
            "phase terminal response version/type"
        );
        Ok(response)
    }
}
impl NativeClient {
    /// No-Started launch closure on the single safety lane (B5b/AQ-08).
    pub async fn phase_terminal_closure(
        &self,
        q: &aq_phase_terminal_protocol::ClosureRequest,
    ) -> Result<aq_phase_terminal_protocol::ClosureReceipt> {
        q.validate()?;
        self.scope(&q.original.binding)?;
        let _slot = self.phase_terminal_safety_slots.try_acquire()?;
        let response = self.phase_terminal_post("launch-closure", q, true).await?;
        Ok(aq_phase_terminal_protocol::decode_closure_receipt(
            &body(response, aq_phase_terminal_protocol::MAX_BYTES).await?,
            q,
        )?)
    }
    /// Refresh-profile closure on the same route; the negotiation selects it.
    pub async fn refresh_terminal_closure(
        &self,
        q: &aq_phase_terminal_protocol::refresh_v1::ClosureRequest,
    ) -> Result<aq_phase_terminal_protocol::refresh_v1::ClosureReceipt> {
        use aq_phase_terminal_protocol::refresh_v1 as wire;
        q.validate()?;
        self.scope(&q.original.binding)?;
        let _slot = self.phase_terminal_safety_slots.try_acquire()?;
        let response = self.phase_terminal_post("launch-closure", q, true).await?;
        Ok(wire::decode_closure_receipt(
            &body(response, wire::MAX_BYTES).await?,
            q,
        )?)
    }
    /// The closure of the attempt's own terminal profile.
    pub async fn launch_closure(
        &self,
        q: &crate::native::journal::launch::ClosureRequest,
    ) -> Result<crate::native::journal::launch::ClosureReceipt> {
        use crate::native::journal::launch::{ClosureReceipt, ClosureRequest};
        Ok(match q {
            ClosureRequest::Frozen(q) => {
                ClosureReceipt::Frozen(self.phase_terminal_closure(q).await?)
            }
            ClosureRequest::Refresh(q) => {
                ClosureReceipt::Refresh(self.refresh_terminal_closure(q).await?)
            }
        })
    }
}
crate::native::phase_profile_shared::http_terminal! { aq_phase_terminal_protocol, phase_terminal_started, phase_terminal_stopped }
