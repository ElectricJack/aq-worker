//! Outbound execution transport with typed protocol errors and bounded responses.
use crate::config::WorkerConfig;
use anyhow::{Context, Result, bail, ensure};
use aq_worker_protocol::*;
use serde::{Serialize, de::DeserializeOwned};
use std::time::Duration;

pub struct ExecutionClient {
    http: reqwest::Client,
    base: String,
    credential: reqwest::header::HeaderValue,
}
impl ExecutionClient {
    pub fn from_config(config: &WorkerConfig) -> Result<Self> {
        let url = reqwest::Url::parse(&config.server_url)?;
        ensure!(
            matches!(url.scheme(), "http" | "https") && url.host_str().is_some(),
            "invalid coordinator URL"
        );
        let mut credential =
            reqwest::header::HeaderValue::from_str(&format!("Bearer {}", config.credential))?;
        credential.set_sensitive(true);
        let mut headers = reqwest::header::HeaderMap::new();
        if let (Some(id), Some(secret)) = (&config.cf_client_id, &config.cf_client_secret) {
            for (key, value) in [
                ("cf-access-client-id", id),
                ("cf-access-client-secret", secret),
            ] {
                let mut v = reqwest::header::HeaderValue::from_str(value)?;
                v.set_sensitive(true);
                headers.insert(key, v);
            }
        }
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .default_headers(headers)
            .build()?;
        Ok(Self {
            http,
            base: config.server_url.trim_end_matches('/').into(),
            credential,
        })
    }
    async fn post<T: Serialize + Validate, R: DeserializeOwned + Validate>(
        &self,
        path: &str,
        request: &T,
    ) -> Result<R> {
        request.validate()?;
        let mut response = self
            .http
            .post(format!("{}/v2/worker/{path}", self.base))
            .header(reqwest::header::AUTHORIZATION, self.credential.clone())
            .json(request)
            .send()
            .await
            .map_err(crate::client::error_chain)
            .map_err(anyhow::Error::msg)?;
        let status = response.status();
        let mut bytes = vec![];
        while let Some(chunk) = response.chunk().await? {
            ensure!(
                bytes.len() + chunk.len() <= 1024 * 1024,
                "coordinator response exceeds 1MiB"
            );
            bytes.extend_from_slice(&chunk);
        }
        if !status.is_success() {
            if let Ok(error) = serde_json::from_slice::<ProtocolError>(&bytes) {
                bail!(
                    "execution {}: {} (retryable={})",
                    serde_json::to_string(&error.code)?,
                    error.message,
                    error.retryable
                );
            }
            bail!("execution endpoint returned {status}");
        }
        let parsed: R = serde_json::from_slice(&bytes).context("invalid execution response")?;
        parsed.validate()?;
        Ok(parsed)
    }
    pub async fn register(&self, r: &RegisterRequest) -> Result<RegisterResponse> {
        self.post("register", r).await
    }
    pub async fn poll(&self, r: &PollRequest) -> Result<PollResponse> {
        self.post("poll", r).await
    }
    pub async fn accept(&self, r: &AcceptRequest) -> Result<AcceptResponse> {
        self.post("accept", r).await
    }
    pub async fn decline(&self, r: &DeclineRequest) -> Result<Ack> {
        self.post("decline", r).await
    }
    pub async fn renew(&self, r: &RenewRequest) -> Result<RenewResponse> {
        self.post("renew", r).await
    }
    pub async fn observe(&self, r: &Observation) -> Result<Ack> {
        self.post("observe", r).await
    }
    pub async fn directive_ack(&self, r: &DirectiveAck) -> Result<Ack> {
        self.post("directive-ack", r).await
    }
    pub async fn report(&self, r: &ResultReport) -> Result<ResultResponse> {
        self.post("result", r).await
    }
    pub async fn reconcile(&self, r: &ReconcileRequest) -> Result<ReconcileResponse> {
        self.post("reconcile", r).await
    }
}
