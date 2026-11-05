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
            {
                let status = response.status();
                let path = response.url().path().to_owned();
                let body: String = response
                    .text()
                    .await
                    .unwrap_or_default()
                    .chars()
                    .take(400)
                    .collect();
                tracing::warn!(%path, %status, %body, "native endpoint refused");
                return Err(HttpStatus(status).into());
            }
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
crate::native::phase_profile_shared::http_terminal! { aq_phase_terminal_protocol, phase_terminal_started, phase_terminal_stopped }
