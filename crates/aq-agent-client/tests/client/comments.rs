use super::*;

async fn comment_client(mode: usize) -> (TempDir, Arc<Peer>, tokio::task::JoinHandle<()>) {
    let (url, peer, server) = peer(mode).await;
    let root = TempDir::new().unwrap();
    install(root.path(), &url, base()["context"].clone());
    (root, peer, server)
}
#[tokio::test]
async fn comment_add_replays_and_conflicts_by_operation_id() {
    let (root, peer, server) = comment_client(0).await;
    let client = AgentClient::open(root.path()).unwrap();
    let binding = &base()["context"]["binding"];
    let op = Uuid::from_u128(0xc1);
    let body = "Finding: the adapter drops `executions`.\n\n- café 世界";
    let added = client.comment_add(op, body).await.unwrap();
    assert_eq!(added.exit_code, 0);
    assert_bound_comment(&added.value["comment"], binding, op);
    // The request names no task: the credential binding decides the thread.
    let sent: Value = serde_json::from_slice(&peer.posts.lock().unwrap()[0]).unwrap();
    assert_eq!(sent, json!({"api_version":1,"operation_id":op,"body":body}));
    // Same ID and body: the same comment. A different body: id_conflict, exit 11.
    let replay = client.comment_add(op, body).await.unwrap();
    assert_eq!((replay.exit_code, &replay.value), (0, &added.value));
    let conflict = client.comment_add(op, "Something else").await.unwrap();
    assert_eq!(conflict.exit_code, 11);
    assert_eq!(conflict.value["code"], "id_conflict");
    assert_eq!(peer.comments.lock().unwrap().len(), 1);
    server.abort();
}
fn assert_bound_comment(c: &Value, binding: &Value, op: Uuid) {
    assert_eq!(c["comment_id"], json!(op));
    assert_eq!(c["task_id"], binding["task_id"]);
    assert_eq!(c["author"]["kind"], "agent");
    assert_eq!(c["author"]["attempt_id"], binding["attempt_id"]);
    assert!(aq_agent_client::comments::valid_comment(c));
}

#[tokio::test]
async fn comment_bodies_are_checked_before_any_request() {
    let (root, peer, server) = comment_client(0).await;
    let client = AgentClient::open(root.path()).unwrap();
    for bad in [
        String::new(),
        " \n\t".into(),
        "bell\u{7}".into(),
        "x".repeat(16_385),
    ] {
        let error = client
            .comment_add(Uuid::new_v4(), &bad)
            .await
            .err()
            .unwrap();
        let failure = error.downcast_ref::<Failure>().unwrap();
        assert_eq!(
            (failure.code, failure.effect),
            ("invalid_comment_body", "not_applied")
        );
    }
    assert!(client.comment_add(Uuid::nil(), "ok").await.is_err());
    for (cursor, limit) in [
        (None, Some(0)),
        (None, Some(101)),
        (Some(""), None),
        (Some("a b"), None),
    ] {
        assert!(client.comment_list(cursor, limit).await.is_err());
    }
    assert_eq!(peer.calls.load(Ordering::SeqCst), 0);
    // Tab, LF, CR and a full 16 KiB of multibyte text are accepted.
    let ok = client
        .comment_add(Uuid::new_v4(), "a\tb\r\nc")
        .await
        .unwrap();
    assert_eq!(ok.exit_code, 0);
    let full = client
        .comment_add(Uuid::new_v4(), &"é".repeat(8192))
        .await
        .unwrap();
    assert_eq!(full.exit_code, 0);
    server.abort();
}
#[tokio::test]
async fn comment_list_pages_newest_first_and_checks_the_thread() {
    let (root, _peer, server) = comment_client(0).await;
    let client = AgentClient::open(root.path()).unwrap();
    for n in 1..=3u128 {
        let added = client
            .comment_add(Uuid::from_u128(n), &format!("note {n}"))
            .await;
        assert_eq!(added.unwrap().exit_code, 0);
    }
    let first = client.comment_list(None, Some(2)).await.unwrap();
    assert_eq!(first.exit_code, 0);
    let bodies = |v: &Value| -> Vec<String> {
        v["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["body"].as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(bodies(&first.value), ["note 3", "note 2"]);
    let cursor = first.value["next_cursor"].as_str().unwrap().to_owned();
    let text = aq_agent_client::comments::render_page(&first.value);
    assert!(text.contains("2 comments (newest first)"), "{text}");
    assert!(text.contains("    note 3"), "{text}");
    assert!(
        text.contains(&format!("aq-agent comment list --cursor {cursor}")),
        "{text}"
    );
    let older = client.comment_list(Some(&cursor), Some(2)).await.unwrap();
    assert_eq!(bodies(&older.value), ["note 1"]);
    assert!(older.value["next_cursor"].is_null());
    server.abort();
}
#[tokio::test]
async fn comment_responses_outside_the_attempt_or_lost_stay_uncertain() {
    // Another attempt's author, or another task's comment, is never accepted.
    for mode in [30, 33] {
        let (root, _peer, server) = comment_client(mode).await;
        let client = AgentClient::open(root.path()).unwrap();
        let op = Uuid::new_v4();
        if mode == 30 {
            let error = client.comment_add(op, "x").await.err().unwrap();
            let failure = error.downcast_ref::<Failure>().unwrap();
            assert_eq!(
                (failure.operation_id, failure.effect),
                (Some(op), "unknown")
            );
        } else {
            client.comment_add(op, "x").await.unwrap();
            let error = client.comment_list(None, None).await.err().unwrap();
            assert_eq!(error.downcast_ref::<Failure>().unwrap().effect, "read_only");
        }
        server.abort();
    }
    // A lost response reports the ID as safe to retry; the retry returns the
    // comment recorded by the first send.
    let (root, peer, server) = comment_client(31).await;
    let client = AgentClient::open(root.path()).unwrap();
    let op = Uuid::new_v4();
    let error = client.comment_add(op, "lost once").await.err().unwrap();
    let failure = error.downcast_ref::<Failure>().unwrap();
    assert_eq!(failure.code, "remote_response_unknown");
    assert_eq!((failure.operation_id, failure.retryable), (Some(op), true));
    let retry = client.comment_add(op, "lost once").await.unwrap();
    assert_eq!(retry.value["comment"]["comment_id"], json!(op));
    assert_eq!(peer.comments.lock().unwrap().len(), 1);
    server.abort();
    // A revoked credential is a definite refusal, exit 14.
    let (root, _peer, server) = comment_client(32).await;
    let client = AgentClient::open(root.path()).unwrap();
    let refused = client.comment_list(None, None).await.unwrap();
    assert_eq!(
        (refused.exit_code, refused.value["code"].as_str()),
        (14, Some("session_revoked"))
    );
    server.abort();
}
#[tokio::test]
async fn actual_cli_comment_add_and_list() {
    let (root, peer, server) = comment_client(0).await;
    let op = Uuid::from_u128(0xa1).to_string();
    assert_cli_adds(&root, &op).await;
    assert_cli_refusals_and_list(&root, &peer, &op).await;
    server.abort();
}

fn cli(root: &TempDir, args: &[&str]) -> tokio::process::Command {
    use std::process::Stdio;
    use tokio::process::Command;
    let mut command = Command::new(env!("CARGO_BIN_EXE_aq-agent"));
    command
        .arg("--state-dir")
        .arg(root.path())
        .arg("comment")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}
async fn assert_cli_adds(root: &TempDir, op: &str) {
    use tokio::io::AsyncWriteExt;
    let output = cli(root, &["add", "Finding: **one**", "--operation-id", op])
        .output()
        .await
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(
        text.starts_with(&format!("Comment {op} recorded on task ")),
        "{text}"
    );
    // Body from stdin; the trailing newline is dropped. --json prints the response.
    let mut child = cli(root, &["add", "--json"]).spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all("Second\nline\n".as_bytes()).await.unwrap();
    drop(stdin);
    let output = child.wait_with_output().await.unwrap();
    assert_eq!(output.status.code(), Some(0));
    let added: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(added["comment"]["body"], "Second\nline");
    // Body from a file.
    let file = root.path().join("note.md");
    std::fs::write(&file, "From a file\n").unwrap();
    let output = cli(root, &["add", "--file", file.to_str().unwrap()])
        .output()
        .await
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
}

async fn assert_cli_refusals_and_list(root: &TempDir, peer: &Peer, op: &str) {
    // Same ID, different body: conflict on stderr, exit 11.
    let output = cli(root, &["add", "changed", "--operation-id", op])
        .output()
        .await
        .unwrap();
    assert_eq!(output.status.code(), Some(11));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("id_conflict"));
    // A blank body never reaches the server: exit 15.
    let posts = peer.posts.lock().unwrap().len();
    let output = cli(root, &["add", "  "]).output().await.unwrap();
    assert_eq!(output.status.code(), Some(15));
    assert_eq!(peer.posts.lock().unwrap().len(), posts);
    let output = cli(root, &["list", "--limit", "101"])
        .output()
        .await
        .unwrap();
    assert_eq!(output.status.code(), Some(2), "clap rejects the limit");
    // Text and JSON listings.
    let output = cli(root, &["list", "--limit", "2"]).output().await.unwrap();
    assert_eq!(output.status.code(), Some(0));
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("2 comments (newest first)"), "{text}");
    assert!(text.contains("    From a file"), "{text}");
    assert!(text.contains("agent (attempt "), "{text}");
    assert!(
        text.contains("aq-agent comment list --cursor tc1.2"),
        "{text}"
    );
    let output = cli(root, &["list", "--json", "--cursor", "tc1.2"])
        .output()
        .await
        .unwrap();
    let page: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(page["items"][0]["body"], "Finding: **one**");
    for stream in [&output.stdout, &output.stderr] {
        assert!(!String::from_utf8_lossy(stream).contains("aqa_"));
    }
}
