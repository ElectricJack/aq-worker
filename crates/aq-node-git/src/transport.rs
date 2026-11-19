//! How the node reaches central aq-git: the server's `/v1/git/<repository>`
//! smart-HTTP proxy (spec 0016 §5.2), authenticated with the node's own worker
//! credential plus the attempt's git token. Everything that depends on the
//! server's exact contract is in this module.
use std::process::Command;
use uuid::Uuid;

/// The request header that carries the attempt's git token (aq-server
/// `aq_delivery::git_grant::TOKEN_HEADER`).
pub const TOKEN_HEADER: &str = "aq-git-token";

/// One attempt's view of central aq-git. Holds secrets: no `Debug`, and they
/// reach git only through its environment, never its argv.
#[derive(Clone)]
pub struct Remote {
    origin: String,
    headers: Vec<String>,
    secrets: Vec<String>,
}

impl Remote {
    /// `server_url` is the worker API origin; `credential` the node's worker
    /// credential (`Authorization: Bearer`), `cf_access` its optional
    /// Cloudflare Access pair, `token` the attempt's git token from the offer.
    pub fn new(
        server_url: &str,
        credential: &str,
        cf_access: Option<(&str, &str)>,
        token: &str,
    ) -> Self {
        let mut headers = vec![
            format!("Authorization: Bearer {credential}"),
            format!("{TOKEN_HEADER}: {token}"),
        ];
        let mut secrets = vec![credential.to_owned(), token.to_owned()];
        if let Some((id, secret)) = cf_access {
            headers.push(format!("cf-access-client-id: {id}"));
            headers.push(format!("cf-access-client-secret: {secret}"));
            secrets.extend([id.to_owned(), secret.to_owned()]);
        }
        secrets.retain(|s| !s.is_empty());
        Self {
            origin: server_url.trim_end_matches('/').to_owned(),
            headers,
            secrets,
        }
    }

    /// The git URL of one project's repository.
    pub fn url(&self, repository: Uuid) -> String {
        format!("{}/v1/git/{repository}", self.origin)
    }

    /// Adds the authentication headers to a git command's environment.
    pub(crate) fn apply(&self, command: &mut Command) {
        command.env("GIT_CONFIG_COUNT", self.headers.len().to_string());
        for (n, line) in self.headers.iter().enumerate() {
            command
                .env(format!("GIT_CONFIG_KEY_{n}"), "http.extraHeader")
                .env(format!("GIT_CONFIG_VALUE_{n}"), line);
        }
    }

    pub(crate) fn redact(&self, text: &str) -> String {
        self.secrets
            .iter()
            .fold(text.to_owned(), |t, s| t.replace(s.as_str(), "<redacted>"))
    }
}
