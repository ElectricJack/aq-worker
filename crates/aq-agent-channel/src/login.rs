//! A Codex ChatGPT login leased to one sandboxed attempt, and its write-back.
//!
//! The server's login store is the one source of truth (aq-server
//! `profiled_execution/login_refresh`, C5b): every attempt gets the newest stored
//! version. Codex refreshes a login by itself and the provider then rotates the
//! refresh token, so a login refreshed inside a sandbox must go back to the store,
//! or every other copy is stranded. The node writes back "version N+1 based on N"
//! for the lease of that attempt only; a newer stored version wins.
//!
//! `LoginLease` is the public half the node keeps beside the attempt's driver: the
//! write-back request and what identifies a refresh. It holds no token.
use crate::channel::NodeLink;
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;
use uuid::Uuid;

/// File beside `driver.json` holding the attempt's `LoginLease`.
pub const LEASE_FILE: &str = "login-lease.json";
const REFRESH_PATH: &str = "/v1/worker/profiled/material/refresh";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoginLease {
    /// The node's `WorkerIdentity`.
    pub identity: Value,
    /// The lease's `LaunchBinding`.
    pub binding: Value,
    pub lease_id: Uuid,
    /// The stored version of the leased bytes (N).
    pub base_version: i64,
    /// `last_refresh` of the leased login.
    pub last_refresh: Option<DateTime<Utc>>,
    /// A fingerprint of the login's account: a write-back is the same account.
    pub account: Uuid,
}
/// The server's answer to a write-back.
#[derive(Debug, PartialEq, Eq)]
pub enum WriteBack {
    /// Stored as this version (or it already was).
    Accepted(i64),
    /// A newer version won; ours is discarded.
    Superseded(i64),
}

/// `last_refresh` and the account fingerprint of a ChatGPT `auth.json`.
fn stamp(auth: &[u8]) -> Result<(Option<DateTime<Utc>>, Uuid)> {
    let login: Value = serde_json::from_slice(auth).context("Codex login JSON")?;
    let account = login["tokens"]["account_id"].as_str();
    let account = account
        .filter(|a| !a.is_empty())
        .context("Codex login account")?;
    let refreshed = match &login["last_refresh"] {
        Value::Null => None,
        at => Some(serde_json::from_value(at.clone()).context("Codex login last_refresh")?),
    };
    Ok((
        refreshed,
        Uuid::new_v5(&Uuid::NAMESPACE_OID, account.as_bytes()),
    ))
}
impl LoginLease {
    /// `auth`: the leased login, already shape-checked by the caller.
    pub fn of(
        auth: &[u8],
        identity: Value,
        binding: Value,
        lease_id: Uuid,
        base_version: i64,
    ) -> Result<Self> {
        let (last_refresh, account) = stamp(auth)?;
        Ok(Self {
            identity,
            binding,
            lease_id,
            base_version,
            last_refresh,
            account,
        })
    }
    /// Seconds since the leased login was last refreshed; None when it does not say.
    pub fn age_secs(&self, now: DateTime<Utc>) -> Option<i64> {
        self.last_refresh.map(|at| (now - at).num_seconds())
    }
    /// Refuses a leased login that is older than the node's configured limit.
    pub fn check_age(&self, max_age_secs: Option<u64>, now: DateTime<Utc>) -> Result<()> {
        let (Some(max), Some(age)) = (max_age_secs, self.age_secs(now)) else {
            return Ok(());
        };
        ensure!(
            age <= i64::try_from(max)?,
            "codex_login_stale: the stored Codex login was last refreshed {} days ago (limit {} days); \
             import the owner's current login (aq-server login import) and retry",
            age / 86400,
            max / 86400
        );
        Ok(())
    }
    /// Whether `after` (the sandbox's `auth.json` when it stopped, shape-checked by
    /// the caller) is this login refreshed: the same account, refreshed later.
    pub fn refreshed(&self, after: &[u8]) -> bool {
        stamp(after).is_ok_and(|(at, account)| {
            account == self.account
                && at.is_some_and(|at| self.last_refresh.is_none_or(|old| at > old))
        })
    }
}
impl NodeLink {
    /// C5b write-back under the node credential: `bytes` as version
    /// `base_version + 1` of the login leased as `lease`. `digest` is the server's
    /// content digest of `bytes` (`aq_session_protocol::Digest`, serialized).
    pub async fn write_back_login(
        &self,
        lease: &LoginLease,
        bytes: &[u8],
        digest: Value,
    ) -> Result<WriteBack> {
        use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
        use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderValue};
        let refresh = serde_json::json!({
            "schema_version": 1,
            "operation_id": Uuid::new_v4(),
            "identity": lease.identity,
            "binding": lease.binding,
            "lease_id": lease.lease_id,
            "base_version": lease.base_version,
            "content_digest": digest,
        });
        let refresh = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&refresh)?);
        ensure!(refresh.len() <= 8192, "login write-back header size");
        let (origin, credential, access) = self.parts();
        let upstream = crate::forward::Upstream::new(
            origin,
            crate::forward::Origins::Node,
            Duration::from_secs(10),
        )?;
        let mut bearer = HeaderValue::from_str(&format!("Bearer {credential}"))?;
        bearer.set_sensitive(true);
        let mut request = upstream
            .client
            .post(upstream.origin.join(REFRESH_PATH)?)
            .header(AUTHORIZATION, bearer)
            .header(CONTENT_TYPE, "application/octet-stream")
            .header("aq-material-refresh", refresh)
            .body(bytes.to_vec());
        if let Some(access) = access {
            for (name, value) in access.headers()? {
                request = request.header(name, value);
            }
        }
        let response = request
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("login write-back unavailable; disposition unknown"))?;
        let status = response.status().as_u16();
        let outcome: Value = serde_json::from_slice(&response.bytes().await?).unwrap_or_default();
        match (
            status,
            outcome["accepted"].as_bool(),
            outcome["version"].as_i64(),
        ) {
            (200, Some(true), Some(version)) => Ok(WriteBack::Accepted(version)),
            (409, Some(false), Some(version)) => Ok(WriteBack::Superseded(version)),
            _ => anyhow::bail!("login write-back refused (status {status})"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn login(account: &str, refreshed: Option<&str>) -> Vec<u8> {
        json!({"auth_mode": "chatgpt", "last_refresh": refreshed,
            "tokens": {"id_token": "i", "access_token": "a", "refresh_token": "r", "account_id": account}})
        .to_string()
        .into_bytes()
    }
    fn lease(auth: &[u8]) -> LoginLease {
        LoginLease::of(auth, json!({}), json!({}), Uuid::from_u128(1), 3).unwrap()
    }
    #[test]
    fn only_a_later_refresh_of_the_same_account_is_written_back() {
        let leased = lease(&login("acct", Some("2026-11-12T15:12:29.436594182Z")));
        assert!(leased.refreshed(&login("acct", Some("2026-11-18T22:00:00Z"))));
        assert!(!leased.refreshed(&login("acct", Some("2026-11-12T15:12:29.436594182Z"))));
        assert!(!leased.refreshed(&login("acct", Some("2026-11-01T00:00:00Z"))));
        assert!(!leased.refreshed(&login("acct", None)));
        assert!(!leased.refreshed(&login("other", Some("2026-11-18T22:00:00Z"))));
        assert!(!leased.refreshed(b"{not json"));
        // A login that never recorded a refresh is refreshed by any dated one.
        assert!(
            lease(&login("acct", None)).refreshed(&login("acct", Some("2026-11-18T22:00:00Z")))
        );
        // The lease file holds no token.
        let text = serde_json::to_string(&leased).unwrap();
        assert!(!text.contains("\"r\"") && !text.contains("acct"), "{text}");
    }
    #[test]
    fn a_login_older_than_the_configured_age_refuses_the_launch() {
        let leased = lease(&login("acct", Some("2026-11-12T00:00:00Z")));
        let now = "2026-11-18T00:00:00Z".parse().unwrap();
        assert_eq!(leased.age_secs(now), Some(6 * 86400));
        leased.check_age(None, now).unwrap();
        leased.check_age(Some(6 * 86400), now).unwrap();
        let error = leased
            .check_age(Some(5 * 86400), now)
            .unwrap_err()
            .to_string();
        assert!(
            error.starts_with("codex_login_stale") && error.contains("6 days ago"),
            "{error}"
        );
        lease(&login("acct", None)).check_age(Some(1), now).unwrap();
    }
    /// The write-back is the server's `MaterialRefresh` request under the node
    /// credential, and 200/409 are its two answers; anything else is an error.
    #[tokio::test]
    async fn write_back_sends_the_lease_scoped_request_and_reads_both_answers() {
        use axum::{Router, body::Bytes, http::HeaderMap, routing::post};
        use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let record = seen.clone();
        let app = Router::new().route(
            REFRESH_PATH,
            post(move |headers: HeaderMap, body: Bytes| async move {
                let header = headers["aq-material-refresh"].as_bytes();
                let request: Value =
                    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(header).unwrap()).unwrap();
                let bearer = headers["authorization"].to_str().unwrap().to_owned();
                let base = request["base_version"].as_i64().unwrap();
                record
                    .lock()
                    .unwrap()
                    .push((request, bearer, body.to_vec()));
                match base {
                    3 => (
                        axum::http::StatusCode::OK,
                        r#"{"accepted":true,"version":4,"replayed":false}"#,
                    ),
                    4 => (
                        axum::http::StatusCode::CONFLICT,
                        r#"{"accepted":false,"version":9,"replayed":false}"#,
                    ),
                    _ => (axum::http::StatusCode::CONFLICT, r#"{"error":"conflict"}"#),
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let link = NodeLink::new(&origin, "node-credential-0123456789", None, None).unwrap();
        let mut leased = lease(&login("acct", Some("2026-11-12T00:00:00Z")));
        leased.binding = json!({"attempt_id": Uuid::from_u128(7)});
        let sent = link
            .write_back_login(&leased, b"refreshed", json!("digest"))
            .await;
        assert_eq!(sent.unwrap(), WriteBack::Accepted(4));
        let (request, bearer, body) = seen.lock().unwrap()[0].clone();
        assert_eq!(bearer, "Bearer node-credential-0123456789");
        assert_eq!(body, b"refreshed");
        assert_eq!(request["schema_version"], 1);
        assert_eq!(request["lease_id"], json!(leased.lease_id));
        assert_eq!(request["binding"], leased.binding);
        assert_eq!(request["content_digest"], "digest");
        leased.base_version = 4;
        let lost = link
            .write_back_login(&leased, b"refreshed", json!("digest"))
            .await;
        assert_eq!(lost.unwrap(), WriteBack::Superseded(9));
        leased.base_version = 5;
        let refused = link
            .write_back_login(&leased, b"refreshed", json!("digest"))
            .await;
        assert!(
            refused
                .unwrap_err()
                .to_string()
                .contains("refused (status 409)")
        );
    }
}
