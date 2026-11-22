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
/// Instead of [`ATTEMPT_HEADER`]: the environment job (aq-server spec 0017) a
/// fetch is for. The server serves it to the node holding the job's claim,
/// for fetches of the job's repository only.
#[cfg(feature = "environment-job")]
pub const ENVIRONMENT_JOB_HEADER: &str = "AQ-Environment-Job";

/// Whom a git request is for; the server derives its authority from it.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Scope {
    Attempt(Uuid),
    #[cfg(feature = "environment-job")]
    EnvironmentJob(Uuid),
}
/// On the result report (S6): the pushed task ref and the commit it is at,
/// or, instead of both, that the push met a moved task ref.
pub const PUSHED_REF_HEADER: &str = "AQ-Pushed-Ref";
pub const PUSHED_HEAD_HEADER: &str = "AQ-Pushed-Head";
pub const PUSH_CONFLICT_HEADER: &str = "AQ-Push-Conflict";

/// The result request's headers for what the push did (S6). The server
/// refuses one of the pushed pair alone or mixed with the conflict.
pub fn result_headers(pushed: &crate::Pushed) -> Vec<(&'static str, String)> {
    match pushed {
        crate::Pushed::Pushed { task_ref, commit } => vec![
            (PUSHED_REF_HEADER, task_ref.clone()),
            (PUSHED_HEAD_HEADER, commit.clone()),
        ],
        crate::Pushed::Conflict => vec![(PUSH_CONFLICT_HEADER, "true".into())],
    }
}

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

    /// The git URL of a repository (a valid grant's `endpoint`).
    pub(crate) fn url(&self, repository: Uuid) -> String {
        format!("{}{}", self.origin, crate::endpoint(repository))
    }

    /// `GET /v1/git/grants/<attempt>`: only a 404 means no grant.
    pub async fn grant(&self, attempt: Uuid, timeout: Duration) -> Result<Option<Grant>> {
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
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        ensure!(
            status == reqwest::StatusCode::OK,
            "git grant refused ({status})"
        );
        let grant: Grant = response.json().await.context("git grant shape")?;
        grant.validate()?;
        Ok(Some(grant))
    }

    /// Adds the authentication and scope headers to a git command's environment.
    pub(crate) fn apply(&self, command: &mut Command, scope: Scope) {
        let scope = match scope {
            Scope::Attempt(id) => (ATTEMPT_HEADER.to_owned(), id.to_string()),
            #[cfg(feature = "environment-job")]
            Scope::EnvironmentJob(id) => (ENVIRONMENT_JOB_HEADER.to_owned(), id.to_string()),
        };
        let headers: Vec<_> = self.headers.iter().chain([&scope]).collect();
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
