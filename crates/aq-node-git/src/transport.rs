//! How the node reaches central aq-git: the server's git grant route and its
//! `/v1/git/<repository>` smart-HTTP proxy (aq-server spec 0016 §5.2), both
//! authenticated with the node's own worker credential. Everything that
//! depends on the server's exact contract is in this module.
use crate::{
    Grant,
    workspaces::{Output, Workspaces},
};
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
    pub(crate) fn url(&self, endpoint: &str) -> String {
        format!("{}{endpoint}", self.origin)
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
        ensure!(
            grant.endpoint == crate::endpoint(grant.repository_id),
            "home git grant endpoint"
        );
        Ok(Some(grant))
    }

    pub(crate) async fn workspaces(&self, attempt: Uuid, timeout: Duration) -> Result<Workspaces> {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(timeout)
            .build()?;
        let mut request = http.get(format!(
            "{}/v1/git/grants/{attempt}/workspaces",
            self.origin
        ));
        for (name, value) in &self.headers {
            request = request.header(name, value);
        }
        let mut response = request
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("workspace grant request failed"))?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(Workspaces::default());
        }
        let status = response.status();
        ensure!(
            matches!(
                status,
                reqwest::StatusCode::OK | reqwest::StatusCode::BAD_REQUEST
            ),
            "workspace grant refused ({status})",
        );
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            ensure!(
                bytes.len().saturating_add(chunk.len()) <= 32_768,
                "workspace grant size"
            );
            bytes.extend_from_slice(&chunk);
        }
        if status == reqwest::StatusCode::BAD_REQUEST {
            // Before this route existed, the generic Git wildcard parsed
            // "grants" as a repository UUID and rejected it before auth.
            let old_route = std::str::from_utf8(&bytes).is_ok_and(|body| {
                body.starts_with("Invalid URL: Cannot parse")
                    && body.contains("`grants`")
                    && body.contains("UUID parsing failed:")
            });
            ensure!(old_route, "workspace grant refused ({status})");
            return Ok(Workspaces::default());
        }
        let plan: Workspaces = serde_json::from_slice(&bytes)?;
        plan.validate()?;
        Ok(plan)
    }

    pub(crate) async fn workspace_bundle(
        &self,
        attempt: Uuid,
        endpoint: &str,
        bundle: std::fs::File,
        timeout: Duration,
    ) -> Result<()> {
        use sha2::{Digest, Sha256};
        use tokio::io::AsyncWriteExt;

        const CHUNK: u64 = 1_048_576;
        const MAX_BYTES: u64 = 1024 * CHUNK;
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        let mut file = tokio::fs::File::from_std(bundle);
        let transfer = async {
            let mut offset = 0;
            let mut total = None;
            let mut digest = None;
            let mut hash = Sha256::new();
            loop {
                let mut request = http
                    .get(format!(
                        "{}{endpoint}/aq-source-bundle?offset={offset}",
                        self.origin
                    ))
                    .header(ATTEMPT_HEADER, attempt.to_string());
                for (name, value) in &self.headers {
                    request = request.header(name, value);
                }
                let mut reply = request
                    .send()
                    .await
                    .map_err(|_| anyhow::anyhow!("workspace bundle request failed"))?;
                ensure!(
                    reply.status().is_success(),
                    "workspace bundle refused ({})",
                    reply.status()
                );
                let size: u64 = reply
                    .headers()
                    .get("aq-workspace-bundle-size")
                    .context("workspace bundle size missing")?
                    .to_str()?
                    .parse()?;
                ensure!(
                    size > offset
                        && size <= MAX_BYTES
                        && total.is_none_or(|n| n == size)
                        && reply
                            .headers()
                            .get("content-type")
                            .is_some_and(|v| v == "application/x-git-bundle"),
                    "workspace bundle size or type differs"
                );
                let current_digest = reply
                    .headers()
                    .get("aq-workspace-bundle-sha256")
                    .context("workspace bundle digest missing")?
                    .to_str()?;
                ensure!(
                    current_digest.len() == 64
                        && current_digest
                            .bytes()
                            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                        && digest
                            .as_deref()
                            .is_none_or(|known| known == current_digest),
                    "workspace bundle digest differs"
                );
                digest.get_or_insert_with(|| current_digest.to_owned());
                total = Some(size);
                let start = offset;
                let stop = offset + (size - offset).min(CHUNK);
                while let Some(bytes) = reply
                    .chunk()
                    .await
                    .map_err(|_| anyhow::anyhow!("workspace bundle interrupted"))?
                {
                    offset += bytes.len() as u64;
                    ensure!(offset <= stop, "workspace bundle exceeds requested range");
                    hash.update(&bytes);
                    file.write_all(&bytes).await?;
                }
                ensure!(
                    offset == stop && offset > start,
                    "workspace bundle range incomplete"
                );
                if offset == size {
                    ensure!(
                        digest.as_deref() == Some(format!("{:x}", hash.finalize()).as_str()),
                        "workspace bundle bytes differ from manifest"
                    );
                    file.flush().await?;
                    return Ok(());
                }
            }
        };
        tokio::time::timeout(timeout, transfer).await?
    }

    pub(crate) async fn workspace_result(
        &self,
        attempt: Uuid,
        outputs: &[Output],
        timeout: Duration,
    ) -> Result<()> {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(timeout)
            .build()?;
        let mut request = http
            .put(format!(
                "{}/v1/git/grants/{attempt}/workspaces/result",
                self.origin
            ))
            .json(&serde_json::json!({"workspaces": outputs}));
        for (name, value) in &self.headers {
            request = request.header(name, value);
        }
        let response = request
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("workspace result request failed"))?;
        ensure!(
            response.status().is_success(),
            "workspace result refused ({})",
            response.status()
        );
        Ok(())
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
