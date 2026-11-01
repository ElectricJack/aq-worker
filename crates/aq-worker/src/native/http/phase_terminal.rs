//! Explicit opt-in phase terminal facts; no execution observation/permit decoder.
use super::*;
use aq_phase_terminal_protocol as wire;
impl NativeClient {
    async fn phase_terminal_post(
        &self,
        path: &str,
        q: &impl Serialize,
        safety: bool,
    ) -> Result<Response> {
        let bytes = serde_json::to_vec(q)?;
        ensure!(
            bytes.len() <= wire::MAX_BYTES,
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
            return Err(HttpStatus(response.status()).into());
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
    pub async fn phase_terminal_started(
        &self,
        q: &wire::StartedRequest,
    ) -> Result<wire::StartedReceipt> {
        q.validate()?;
        self.scope(&q.original.binding)?;
        let _slot = self.control_slots.try_acquire()?;
        let response = self.phase_terminal_post("started", q, false).await?;
        Ok(wire::decode_started_receipt(
            &body(response, wire::MAX_BYTES).await?,
            q,
        )?)
    }
    /// Separate single safety-report lane; blocked Started/bulk/renew traffic
    /// cannot consume it. Physical local stop never waits for this network lane.
    pub async fn phase_terminal_stopped(
        &self,
        q: &wire::StoppedRequest,
    ) -> Result<wire::StoppedReceipt> {
        q.validate()?;
        self.scope(&q.original.evidence.binding)?;
        let _slot = self.phase_terminal_safety_slots.try_acquire()?;
        let response = self.phase_terminal_post("stopped", q, true).await?;
        Ok(wire::decode_stopped_receipt(
            &body(response, wire::MAX_BYTES).await?,
            q,
        )?)
    }
}
