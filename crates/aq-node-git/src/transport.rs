//! How the node reaches central aq-git: the server's git grant route and its
//! `/v1/git/<repository>` smart-HTTP proxy (aq-server spec 0016 §5.2), both
//! authenticated with the node's own worker credential. Everything that
//! depends on the server's exact contract is in this module.
use crate::Grant;
use anyhow::{Context as _, Result, ensure};
use std::{process::Command, time::Duration};
use uuid::Uuid;

/// The attempt a git request is for; the server derives its grant from it.
pub const ATTEMPT_HEADER: &str = "AQ-Attempt";
/// On the result report (S6): the pushed task ref and the commit it is at.
pub const PUSHED_REF_HEADER: &str = "AQ-Pushed-Ref";
pub const PUSHED_HEAD_HEADER: &str = "AQ-Pushed-Head";

/// The node's view of central aq-git. Holds secrets: no `Debug`, and they
/// reach git only through its environment, never its argv.
#[derive(Clone)]
pub struct Remote {
    origin: String,
    headers: Vec<(String, String)>,
    secrets: Vec<String>,
}

impl Remote {
    /// `server_url` is the worker API origin; `credential` the node's worker
    /// credential (`Authorization: Bearer`), `cf_access` its optional
    /// Cloudflare Access pair.
    pub fn new(server_url: &str, credential: &str, cf_access: Option<(&str, &str)>) -> Self {
        let mut headers = vec![("Authorization".into(), format!("Bearer {credential}"))];
        let mut secrets = vec![credential.to_owned()];
        if let Some((id, secret)) = cf_access {
            headers.push(("cf-access-client-id".into(), id.to_owned()));
            headers.push(("cf-access-client-secret".into(), secret.to_owned()));
            secrets.extend([id.to_owned(), secret.to_owned()]);
        }
        secrets.retain(|s| !s.is_empty());
        Self {
            origin: server_url.trim_end_matches('/').to_owned(),
            headers,
            secrets,
        }
    }

    /// The git URL of the grant's repository.
    pub(crate) fn url(&self, grant: &Grant) -> String {
        format!("{}{}", self.origin, grant.endpoint)
    }

    /// `GET /v1/git/grants/<attempt>`: the attempt's grant, or an error when
    /// the server has none for this node (404) or is unavailable.
    pub async fn grant(&self, attempt: Uuid, timeout: Duration) -> Result<Grant> {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(timeout)
            .build()?;
        let mut request = http.get(format!("{}/v1/git/grants/{attempt}", self.origin));
        for (name, value) in &self.headers {
            request = request.header(name, value);
        }
        let response = request
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("git grant request failed"))?;
        let status = response.status();
        ensure!(status.is_success(), "git grant refused ({status})");
        let grant: Grant = response.json().await.context("git grant shape")?;
        grant.validate()?;
        Ok(grant)
    }

    /// Adds the authentication and attempt headers to a git command's environment.
    pub(crate) fn apply(&self, command: &mut Command, attempt: Uuid) {
        let attempt = (ATTEMPT_HEADER.to_owned(), attempt.to_string());
        let headers: Vec<_> = self.headers.iter().chain([&attempt]).collect();
        command.env("GIT_CONFIG_COUNT", headers.len().to_string());
        for (n, (name, value)) in headers.into_iter().enumerate() {
            command
                .env(format!("GIT_CONFIG_KEY_{n}"), "http.extraHeader")
                .env(format!("GIT_CONFIG_VALUE_{n}"), format!("{name}: {value}"));
        }
    }

    pub(crate) fn redact(&self, text: &str) -> String {
        self.secrets
            .iter()
            .fold(text.to_owned(), |t, s| t.replace(s.as_str(), "<redacted>"))
    }
}
