//! Spec 0006 phase 3 acceptance, live: a container calls `whoami` through the
//! per-attempt agent channel, holds no `aqa_` string anywhere that leaves the
//! sandbox, and revocation closes the channel. The real node-side driver runs
//! a real docker-local sandbox; the agent is `agent.sh` (curl through the
//! socket), run directly or by real Claude Code through its Bash tool.
//! Ignored by default. Inputs:
//! - `AQ_SANDBOX_SOCKET`, `AQ_MS1_TEMPLATE_IMAGE` (a pullable `...@sha256:...`
//!   image with bash and curl);
//! - `AQ_CHAN_PROOF_FILE` (optional): the real aq-server to test against. A
//!   JSON file with `origin`, `node_credential` and the attempt identity; the
//!   test asks for revocation by creating `<file>.revoke` and waits for
//!   `<file>.revoked`. Without it a stub server answers;
//! - `AQ_MS1_CLAUDE_TOKEN_FILE` (optional): run real Claude Code, which must
//!   run `agent.sh` with its Bash tool (proves `AQ_CHANNEL` reaches it);
//! - `AQ_CHANNEL_LIVE_EVIDENCE_DIR` (optional): copies the outputs there.
use aq_agent_channel::{
    AgentChannel, AttemptIdentity, ChannelConfig, CredentialIssuer, IssueFuture, Issued,
    NodeIssuer, holds_agent_credential,
};
use aq_worker::backend::docker_local::{AllowedTemplate, DockerLocalBackend, DockerLocalConfig};
use aq_worker::backend::{BackendKind, Ownership, ResourceLimits, SandboxIdentity, TemplateDigest};
use aq_worker::sandbox_driver::{DriverConfig, run};
use axum::{
    Json, Router,
    http::{HeaderMap, StatusCode, Uri},
    response::IntoResponse,
};
use std::{
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

/// The agent. Phase 1: whoami (with a spoofed bearer), discovery with a
/// query string in the CLI's request form, a command it holds no grant for,
/// a node route. Then it marks phase 1 and polls whoami until revocation.
const AGENT: &str = r#"#!/bin/bash
set -u
s=/aq/channel/aq.sock; w=/aq/work/out; mkdir -p "$w"
call() { m=$1; p=$2; o=$3; shift 3; curl -sS --unix-socket "$s" -o "$o" -w '%{http_code}' -X "$m" -H 'Host: localhost' -H 'X-AQ-Request: 1' "$@" "http://localhost$p" 2>/dev/null; echo " rc=$?"; }
env | grep -c '^AQ_CHANNEL=unix:/aq/channel/aq.sock$' > "$w/env-channel.txt"
env | grep -c 'aqa_' > "$w/env-aqa.txt"
who=$(call POST /api/v1/commands/whoami "$w/whoami.json" -H 'Content-Type: application/json' -H 'Authorization: Bearer spoofed-by-the-agent' -d '{"params":{}}')
list=$(call GET '/api/v1/commands?all=true' "$w/commands.json")
ask=$(call POST /api/v1/commands/question.ask "$w/grant-missing.json" -H 'Content-Type: application/json' -d '{"params":{"question":"May I?"}}')
node=$(call POST /v2/worker/agent-session "$w/channel-refused.json" -H 'Content-Type: application/json' -d '{}')
printf 'whoami=%s\ncommands=%s\nquestion.ask=%s\nnode=%s\n' "$who" "$list" "$ask" "$node" > "$w/codes.txt"
touch /aq/work/out/phase1
for i in $(seq 1 120); do
  r=$(call POST /api/v1/commands/whoami "$w/revoked.json" -H 'Content-Type: application/json' -d '{"params":{}}')
  case "$r" in 401*) break;; esac
  sleep 0.5
done
sleep 1
after=$(call POST /api/v1/commands/whoami /dev/null -H 'Content-Type: application/json' -d '{"params":{}}')
ls -A /aq/channel > "$w/channel-after.txt" 2>&1
printf 'revoked=%s\nafter=%s\n' "$r" "$after" >> "$w/codes.txt"
printf '{"type":"result","subtype":"success","is_error":false,"result":"Changes: out/; Checks: curl through AQ_CHANNEL; Limits: none"}\n'
"#;

struct Fixed(String);
impl CredentialIssuer for Fixed {
    fn issue(&self, expected: u64) -> IssueFuture<'_> {
        let token = self.0.clone();
        Box::pin(async move {
            Ok(Issued {
                token: token.into(),
                version: expected + 1,
                expires_at_ms: now_ms() + 600_000,
            })
        })
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} must be set"))
}

/// Stub aq-server: whoami answers only the attempt credential, until revoked.
async fn stub(token: String, attempt: uuid::Uuid, revoked: Arc<AtomicBool>) -> String {
    let refusal = |code: &str, grant: Option<&str>| {
        serde_json::json!({"code": code, "command": "question.ask", "grant": grant, "held": [],
            "message": "stub refusal", "effect": "not_applied", "request_id": null})
    };
    let expected = format!("Bearer {token}");
    let app = Router::new().fallback(move |uri: Uri, headers: HeaderMap| {
        let (expected, revoked) = (expected.clone(), revoked.clone());
        async move {
            let auth = headers
                .get("authorization")
                .map_or("", |v| v.to_str().unwrap());
            if auth != expected || revoked.load(Ordering::SeqCst) {
                let code = if auth == expected {
                    "session_revoked"
                } else {
                    "unauthorized"
                };
                return (StatusCode::UNAUTHORIZED, Json(refusal(code, None))).into_response();
            }
            match uri.path() {
                "/api/v1/commands/whoami" => Json(serde_json::json!({
                    "principal": "agent", "attempt_id": attempt}))
                .into_response(),
                "/api/v1/commands" => Json(serde_json::json!({"commands": [{"name": "whoami"}],
                    "query": uri.query()}))
                .into_response(),
                _ => (
                    StatusCode::FORBIDDEN,
                    Json(refusal("grant_missing", Some("question.ask"))),
                )
                    .into_response(),
            }
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    origin
}

#[tokio::test]
#[ignore = "live: real sandbox daemon; spec 0006 phase 3 acceptance"]
async fn container_calls_whoami_through_the_agent_channel() {
    let real = std::env::var("AQ_CHAN_PROOF_FILE")
        .ok()
        .map(std::path::PathBuf::from);
    let proof: Option<serde_json::Value> = real
        .as_ref()
        .map(|f| serde_json::from_slice(&std::fs::read(f).unwrap()).unwrap());
    let id = |k: &str| uuid::Uuid::parse_str(proof.as_ref().unwrap()[k].as_str().unwrap()).unwrap();
    let revoked = Arc::new(AtomicBool::new(false));
    let (owner, origin, issuer): (Ownership, String, Arc<dyn CredentialIssuer>) = match &proof {
        Some(p) => {
            let identity = AttemptIdentity {
                node_id: id("node_id"),
                boot_id: id("boot_id"),
                attempt_id: id("attempt_id"),
                session_id: id("session_id"),
                generation: p["generation"].as_u64().unwrap(),
            };
            let origin = p["origin"].as_str().unwrap().to_owned();
            let issuer = NodeIssuer::new(
                &origin,
                p["node_credential"].as_str().unwrap(),
                None,
                identity,
            )
            .unwrap();
            let owner = Ownership {
                node_id: identity.node_id,
                boot_id: identity.boot_id,
                attempt_id: identity.attempt_id,
            };
            (owner, origin, Arc::new(issuer))
        }
        None => {
            let attempt = uuid::Uuid::new_v4();
            let token = format!("aqa_{}", uuid::Uuid::new_v4().simple()) + "0123456789a";
            let owner = Ownership {
                node_id: uuid::Uuid::new_v4(),
                boot_id: uuid::Uuid::new_v4(),
                attempt_id: attempt,
            };
            (
                owner,
                stub(token.clone(), attempt, revoked.clone()).await,
                Arc::new(Fixed(token)),
            )
        }
    };
    let attempt = owner.attempt_id;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for d in ["work", "session", "private", "channels"] {
        std::fs::create_dir(root.join(d)).unwrap();
        std::fs::set_permissions(root.join(d), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let work = root.join("work");
    std::fs::write(work.join("agent.sh"), AGENT).unwrap();
    std::fs::set_permissions(
        work.join("agent.sh"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let claude = std::env::var("AQ_MS1_CLAUDE_TOKEN_FILE").ok();
    std::fs::write(
        root.join("session/context-0.bin"),
        "Run exactly this one command with your Bash tool, from the working directory: \
         `bash agent.sh`. It takes up to a minute. Do not inspect, edit or explain the script. \
         Then end your reply with the single line: Changes: out/; Checks: bash agent.sh; Limits: none",
    )
    .unwrap();
    let deadline = now_ms() + 600_000;
    let channel = AgentChannel::open(
        ChannelConfig {
            attempt_id: attempt,
            directory: root.join("channels").join(attempt.to_string()),
            origin,
            access: None,
            not_after_ms: deadline,
        },
        issuer,
    )
    .await
    .unwrap();
    let token_file = root.join("private/token");
    let login = match &claude {
        Some(file) => std::fs::read(file).unwrap(),
        None => b"fixture-login-0123456789abcdef".to_vec(),
    };
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&token_file)
        .and_then(|mut f| std::io::Write::write_all(&mut f, &login))
        .unwrap();
    let image = env("AQ_MS1_TEMPLATE_IMAGE");
    let digest = TemplateDigest::parse(image.rsplit_once('@').unwrap().1).unwrap();
    let mut docker = DockerLocalConfig::new(env("AQ_SANDBOX_SOCKET").into());
    docker.templates = vec![AllowedTemplate {
        image,
        digest: digest.clone(),
    }];
    docker.shared_root_fs = true;
    docker.channel_root = Some(root.join("channels"));
    let home = "/aq/home";
    let (executable, arguments) = match &claude {
        Some(_) => (
            "/opt/aq/bin/claude".to_string(),
            "-p --output-format stream-json --verbose --input-format text --permission-mode \
             bypassPermissions --setting-sources user --settings /aq/home/settings.json \
             --strict-mcp-config --no-session-persistence --disable-slash-commands --max-turns 4"
                .split(' ')
                .map(String::from)
                .collect(),
        ),
        None => (
            "/bin/bash".to_string(),
            vec!["/aq/work/agent.sh".to_string()],
        ),
    };
    let config = DriverConfig {
        docker_local: docker.clone(),
        template: digest,
        owner: owner.clone(),
        hard_deadline_ms: deadline,
        resources: ResourceLimits {
            cpu_millis: 2000,
            memory_bytes: 2 << 30,
            pids: 256,
            disk_bytes: None,
        },
        workspace: work.clone(),
        token_file,
        executable,
        arguments,
        environment: [
            ("HOME", home),
            ("CLAUDE_CONFIG_DIR", home),
            ("PATH", "/usr/local/bin:/usr/bin:/bin"),
            ("DISABLE_AUTOUPDATER", "1"),
            ("DISABLE_TELEMETRY", "1"),
            ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect(),
        settings_json: r#"{"permissions":{"defaultMode":"bypassPermissions"},"sandbox":{"enabled":false},"disableAllHooks":true,"enableAllProjectMcpServers":false,"cleanupPeriodDays":1,"forceLoginMethod":"claudeai"}"#.into(),
        result_dir: None,
        max_workspace_bytes: 64 * 1024 * 1024,
        agent_channel: true,
    };
    let config_file = root.join("private/driver.json");
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&config_file)
        .and_then(|mut f| std::io::Write::write_all(&mut f, &serde_json::to_vec(&config).unwrap()))
        .unwrap();
    // Revoke once the agent has finished phase 1 (read through the backend).
    let sandbox = SandboxIdentity {
        kind: BackendKind::DockerLocal,
        backend_id: owner.deterministic_name(),
        owner: owner.clone(),
    };
    let revoker = tokio::spawn({
        let (revoked, real) = (revoked.clone(), real.clone());
        async move {
            let mut backend = DockerLocalBackend::connect_unix(docker).await.unwrap();
            for _ in 0..1200 {
                if backend
                    .read_file(&sandbox, "/aq/work/out/phase1")
                    .await
                    .is_ok()
                {
                    match &real {
                        Some(f) => {
                            std::fs::write(f.with_extension("revoke"), "1").unwrap();
                            while !f.with_extension("revoked").exists() {
                                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                            }
                        }
                        None => revoked.store(true, Ordering::SeqCst),
                    }
                    return true;
                }
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
            false
        }
    });
    unsafe { std::env::set_var("AQ_SESSION_DIRECTORY", root.join("session")) };
    let code = run(&config_file).await.unwrap();
    assert!(revoker.await.unwrap(), "the agent never finished phase 1");

    let out = work.join("out");
    let read = |name: &str| std::fs::read_to_string(out.join(name)).unwrap_or_default();
    let codes = read("codes.txt");
    if let Ok(dest) = std::env::var("AQ_CHANNEL_LIVE_EVIDENCE_DIR") {
        let dest = std::path::Path::new(&dest);
        for entry in std::fs::read_dir(&out).unwrap() {
            let entry = entry.unwrap();
            std::fs::copy(entry.path(), dest.join(entry.file_name())).unwrap();
        }
        std::fs::copy(root.join("private/claude.jsonl"), dest.join("claude.jsonl")).unwrap();
    }
    assert_eq!(code, 0, "driver exit; codes: {codes}");
    assert!(codes.contains("whoami=200 rc=0"), "{codes}");
    let me: serde_json::Value = serde_json::from_str(&read("whoami.json")).unwrap();
    assert_eq!(
        (me["principal"].as_str(), me["attempt_id"].as_str()),
        (Some("agent"), Some(&*attempt.to_string()))
    );
    assert!(codes.contains("commands=200 rc=0"), "{codes}");
    assert!(codes.contains("question.ask=403 rc=0"), "{codes}");
    let missing: serde_json::Value = serde_json::from_str(&read("grant-missing.json")).unwrap();
    assert_eq!(missing["code"], "grant_missing");
    assert!(codes.contains("node=403 rc=0"), "{codes}");
    let refused: serde_json::Value = serde_json::from_str(&read("channel-refused.json")).unwrap();
    assert_eq!(
        (refused["code"].as_str(), refused["effect"].as_str()),
        (Some("channel_refused"), Some("not_applied"))
    );
    assert!(codes.contains("revoked=401 rc=0"), "{codes}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&read("revoked.json")).unwrap()["code"],
        "session_revoked"
    );
    assert!(
        codes.contains("after=000 rc=7"),
        "after revocation the socket is gone: {codes}"
    );
    assert_eq!(read("env-channel.txt").trim(), "1");
    assert_eq!(read("env-aqa.txt").trim(), "0");
    assert!(!read("channel-after.txt").contains("aq.sock"));
    assert!(
        channel.is_closed() && !channel.directory().exists(),
        "revocation closed the channel"
    );
    // No `aqa_` string at all in the transcript or anything copied back.
    let transcript = std::fs::read(root.join("private/claude.jsonl")).unwrap();
    assert!(!transcript.windows(4).any(|w| w == b"aqa_") && !holds_agent_credential(&transcript));
    for entry in std::fs::read_dir(&out).unwrap() {
        let data = std::fs::read(entry.unwrap().path()).unwrap();
        assert!(!data.windows(4).any(|w| w == b"aqa_"));
    }
}
