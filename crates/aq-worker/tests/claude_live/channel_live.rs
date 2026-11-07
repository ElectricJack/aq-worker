//! Spec 0006 phase 3 acceptance, live: a container calls `whoami` through the
//! per-attempt agent channel, holds no `aqa_` string anywhere that leaves the
//! sandbox, and server revocation closes the channel. The real node-side
//! driver runs a real docker-local sandbox; the agent is a curl script in
//! place of Claude; aq-server is a stub until the `whoami` command lands.
//! Ignored by default. Inputs: `AQ_SANDBOX_SOCKET`, `AQ_MS1_TEMPLATE_IMAGE`
//! (a pullable `...@sha256:...` image with bash and curl), optional
//! `AQ_CHANNEL_LIVE_EVIDENCE_DIR`.
use aq_agent_channel::{
    AgentChannel, ChannelConfig, CredentialIssuer, IssueFuture, Issued, holds_agent_credential,
};
use aq_worker::backend::docker_local::{AllowedTemplate, DockerLocalConfig};
use aq_worker::backend::{Ownership, ResourceLimits, TemplateDigest};
use aq_worker::sandbox_driver::{DriverConfig, run};
use axum::{
    Json, Router,
    http::{HeaderMap, StatusCode, Uri},
    response::IntoResponse,
};
use std::{
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    sync::{Arc, Mutex},
};

/// The agent: whoami (with a spoofed bearer), a node route, then a call the
/// server answers `session_revoked`, then whoami again.
const AGENT: &str = r#"set -u
s=/aq/channel/aq.sock
call() { curl -sS --unix-socket "$s" -o "$2" -w '%{http_code}' -X POST -H 'Content-Type: application/json' "${@:3}" -d '{}' "http://aq$1" 2>/dev/null; echo " rc=$?"; }
who=$(call /api/v1/commands/whoami /aq/work/whoami.json -H 'Authorization: Bearer spoofed-by-the-agent')
node=$(call /v2/worker/agent-session /aq/work/refusal.json)
revoke=$(call /api/v1/commands/revoked /dev/null)
sleep 1
after=$(call /api/v1/commands/whoami /dev/null)
env | grep -c '^AQ_CHANNEL=unix:/aq/channel/aq.sock$' > /aq/work/env-channel.txt
ls -A /aq/channel > /aq/work/channel-after.txt 2>&1
printf 'whoami=%s\nnode=%s\nrevoke=%s\nafter=%s\n' "$who" "$node" "$revoke" "$after" > /aq/work/codes.txt
printf '{"type":"result","subtype":"success","is_error":false,"result":"Changes: whoami.json codes.txt; Checks: curl through AQ_CHANNEL; Limits: stub server"}\n'
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

#[tokio::test]
#[ignore = "live: real sandbox daemon; spec 0006 phase 3 acceptance"]
async fn container_calls_whoami_through_the_agent_channel() {
    let attempt = uuid::Uuid::new_v4();
    let token = format!("aqa_{}", uuid::Uuid::new_v4().simple()) + "0123456789a";
    // Stub aq-server: whoami answers only the attempt credential.
    let seen: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
    let (log, expected) = (seen.clone(), format!("Bearer {token}"));
    let app = Router::new().fallback(move |uri: Uri, headers: HeaderMap| {
        let (log, expected) = (log.clone(), expected.clone());
        async move {
            let auth = headers
                .get("authorization")
                .map_or("", |v| v.to_str().unwrap())
                .to_owned();
            log.lock()
                .unwrap()
                .push((uri.path().to_owned(), auth.clone()));
            match (uri.path(), auth == expected) {
                ("/api/v1/commands/whoami", true) => Json(serde_json::json!({
                    "principal": {"kind": "agent", "attempt_id": attempt}}))
                .into_response(),
                ("/api/v1/commands/revoked", true) => (
                    StatusCode::UNAUTHORIZED,
                    Json(serde_json::json!({"code": "session_revoked"})),
                )
                    .into_response(),
                _ => StatusCode::UNAUTHORIZED.into_response(),
            }
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for d in ["work", "session", "private", "channels"] {
        std::fs::create_dir(root.join(d)).unwrap();
        std::fs::set_permissions(root.join(d), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let work = root.join("work");
    std::fs::write(work.join("README.md"), "# scratch\n").unwrap();
    std::fs::write(root.join("session/context-0.bin"), "Call whoami.").unwrap();
    let deadline = now_ms() + 600_000;
    let channel = AgentChannel::open(
        ChannelConfig {
            attempt_id: attempt,
            directory: root.join("channels").join(attempt.to_string()),
            origin,
            access: None,
            not_after_ms: deadline,
        },
        Arc::new(Fixed(token.clone())),
    )
    .await
    .unwrap();
    let token_file = root.join("private/token");
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&token_file)
        .and_then(|mut f| std::io::Write::write_all(&mut f, b"fixture-login-0123456789abcdef"))
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
    let config = DriverConfig {
        docker_local: docker,
        template: digest,
        owner: Ownership {
            node_id: uuid::Uuid::new_v4(),
            boot_id: uuid::Uuid::new_v4(),
            attempt_id: attempt,
        },
        hard_deadline_ms: deadline,
        resources: ResourceLimits {
            cpu_millis: 1000,
            memory_bytes: 1 << 30,
            pids: 128,
            disk_bytes: None,
        },
        workspace: work.clone(),
        token_file,
        executable: "/bin/bash".into(),
        arguments: vec!["-c".into(), AGENT.replace('\n', "; ").replace("; ; ", "; ")],
        environment: [
            ("HOME", "/aq/home"),
            ("PATH", "/usr/local/bin:/usr/bin:/bin"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect(),
        settings_json: "{}".into(),
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
    unsafe { std::env::set_var("AQ_SESSION_DIRECTORY", root.join("session")) };
    let code = run(&config_file).await.unwrap();

    let read = |name: &str| std::fs::read_to_string(work.join(name)).unwrap_or_default();
    let codes = read("codes.txt");
    if let Ok(out) = std::env::var("AQ_CHANNEL_LIVE_EVIDENCE_DIR") {
        for name in [
            "codes.txt",
            "whoami.json",
            "refusal.json",
            "env-channel.txt",
            "channel-after.txt",
        ] {
            let _ = std::fs::copy(work.join(name), std::path::Path::new(&out).join(name));
        }
        let _ = std::fs::copy(
            root.join("private/claude.jsonl"),
            std::path::Path::new(&out).join("claude.jsonl"),
        );
    }
    assert_eq!(code, 0, "driver exit; codes: {codes}");
    assert!(codes.contains("whoami=200 rc=0"), "{codes}");
    assert!(read("whoami.json").contains(&attempt.to_string()));
    assert!(
        codes.contains("node=403 rc=0") && read("refusal.json").contains("channel_refused"),
        "{codes}"
    );
    assert!(codes.contains("revoke=401 rc=0"), "{codes}");
    assert!(
        codes.contains("after=000 rc=7"),
        "after revocation the socket is gone: {codes}"
    );
    assert_eq!(read("env-channel.txt").trim(), "1");
    assert!(!read("channel-after.txt").contains("aq.sock"));
    assert!(
        channel.is_closed() && !channel.directory().exists(),
        "revocation closed the channel"
    );
    // The server saw only the attempt credential; the agent's bearer was replaced.
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2, "{seen:?}");
    assert!(
        seen.iter()
            .all(|(_, auth)| *auth == format!("Bearer {token}"))
    );
    // No `aqa_` string at all in the transcript or anything copied back.
    let transcript = std::fs::read(root.join("private/claude.jsonl")).unwrap();
    assert!(!transcript.windows(4).any(|w| w == b"aqa_") && !holds_agent_credential(&transcript));
    for entry in std::fs::read_dir(&work).unwrap() {
        let data = std::fs::read(entry.unwrap().path()).unwrap();
        assert!(!data.windows(4).any(|w| w == b"aqa_"));
    }
}
