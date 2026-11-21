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
        let request = client
            .post(format!("{}/v1/worker/profiled-v2/{path}", self.base))
            .header(header::AUTHORIZATION, self.credential.clone())
            .header(header::CONTENT_TYPE, "application/json")
            .header("aq-phase-terminal-api-version", "1")
            .body(bytes);
        aq_node_service::native_transport::phase_terminal_response(request, path == "stopped").await
    }
}
impl NativeClient {
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
crate::native::phase_profile_shared::http_terminal! { aq_phase_terminal_protocol, phase_terminal_started, phase_terminal_stopped, phase_terminal_closure }
