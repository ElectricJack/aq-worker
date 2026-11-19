//! Against a local double of the server's `/v1/git` proxy: `git http-backend`
//! over one bare repository, recording each request's authentication.
use super::*;
use axum::{Router, body::Bytes, extract::Path as UrlPath, http::HeaderMap, routing::any};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

const GIT: &str = "/usr/bin/git";

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new(GIT)
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8(out.stdout).unwrap().trim().into()
}

struct Central {
    dir: tempfile::TempDir,
    url: String,
    /// `METHOD path?query authorization token` per request.
    seen: Arc<Mutex<Vec<String>>>,
    /// The next receive-pack is applied, then its reply is cut off.
    drop_push: Arc<AtomicBool>,
    _runtime: tokio::runtime::Runtime,
}

impl Central {
    fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let remote = dir.path().join("remote.git");
        git(
            dir.path(),
            &["init", "--quiet", "--bare", "-b", "main", "remote.git"],
        );
        git(&remote, &["config", "http.receivepack", "true"]);
        let seed = dir.path().join("seed");
        git(
            dir.path(),
            &["clone", "--quiet", remote.to_str().unwrap(), "seed"],
        );
        std::fs::write(seed.join("a.txt"), "one\n").unwrap();
        git(&seed, &["add", "a.txt"]);
        git(&seed, &["commit", "--quiet", "-m", "main"]);
        git(
            &seed,
            &["push", "--quiet", "origin", "HEAD:refs/heads/main"],
        );
        let (seen, drop_push) = (Arc::default(), Arc::new(AtomicBool::new(false)));
        let router = smart_http(
            dir.path().to_owned(),
            Arc::clone(&seen),
            Arc::clone(&drop_push),
        );
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        runtime.spawn(async move {
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            axum::serve(listener, router).await.unwrap();
        });
        Self {
            dir,
            url,
            seen,
            drop_push,
            _runtime: runtime,
        }
    }
    fn remote(&self) -> PathBuf {
        self.dir.path().join("remote.git")
    }
    fn head(&self, r: &str) -> Option<String> {
        let out = Command::new(GIT)
            .arg("-C")
            .arg(self.remote())
            .args(["rev-parse", "--verify", "--quiet", r])
            .output()
            .unwrap();
        out.status
            .success()
            .then(|| String::from_utf8(out.stdout).unwrap().trim().into())
    }
    fn grant(&self, task: &str, lease: Option<String>) -> Grant {
        Grant {
            repository_id: Uuid::from_u128(7),
            base: self.head("refs/heads/main").unwrap(),
            task_ref: format!("refs/heads/aq/{task}"),
            lease,
            main_ref: "refs/heads/main".into(),
        }
    }
    fn requests(&self, needle: &str) -> usize {
        let seen = self.seen.lock().unwrap();
        seen.iter().filter(|l| l.contains(needle)).count()
    }
}

fn smart_http(root: PathBuf, seen: Arc<Mutex<Vec<String>>>, drop_push: Arc<AtomicBool>) -> Router {
    let handler = move |method: axum::http::Method,
                        UrlPath((_repository, rest)): UrlPath<(Uuid, String)>,
                        uri: axum::http::Uri,
                        h: HeaderMap,
                        body: Bytes| async move {
        let text = |name: &str| {
            let value = h.get(name).and_then(|v| v.to_str().ok());
            value.unwrap_or_default().to_string()
        };
        seen.lock().unwrap().push(format!(
            "{method} {rest}{} {} {}",
            uri.query().map(|q| format!("?{q}")).unwrap_or_default(),
            text("authorization"),
            text(transport::TOKEN_HEADER)
        ));
        let mut child = Command::new(GIT)
            .arg("http-backend")
            .env("GIT_PROJECT_ROOT", &root)
            .env("GIT_HTTP_EXPORT_ALL", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("PATH_INFO", format!("/remote.git/{rest}"))
            .env("REQUEST_METHOD", method.as_str())
            .env("QUERY_STRING", uri.query().unwrap_or_default())
            .env("CONTENT_TYPE", text("content-type"))
            .env("CONTENT_LENGTH", body.len().to_string())
            .env("HTTP_CONTENT_ENCODING", text("content-encoding"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let writer = std::thread::spawn(move || {
            let _ = std::io::Write::write_all(&mut stdin, &body);
        });
        let out = child.wait_with_output().unwrap();
        writer.join().unwrap();
        let cut = out
            .stdout
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .unwrap();
        let mut response = axum::http::Response::builder();
        for line in std::str::from_utf8(&out.stdout[..cut]).unwrap().lines() {
            let (name, value) = line.split_once(": ").unwrap();
            response = if name == "Status" {
                response.status(value[..3].parse::<u16>().unwrap())
            } else {
                response.header(name, value)
            };
        }
        let body = out.stdout[cut + 4..].to_vec();
        if rest == "git-receive-pack" && drop_push.swap(false, Ordering::SeqCst) {
            // Applied, but the connection ends before the client reads its status.
            let cut: std::io::Result<Bytes> = Err(std::io::ErrorKind::ConnectionReset.into());
            let stream = futures_util::stream::iter([cut]);
            return response
                .body(axum::body::Body::from_stream(stream))
                .unwrap();
        }
        response.body(axum::body::Body::from(body)).unwrap()
    };
    Router::new().route("/v1/git/{repository}/{*rest}", any(handler))
}

fn node(root: &Path) -> NodeGit {
    NodeGit::new(
        GIT.into(),
        root.to_owned(),
        ("AQ node".into(), "node@aq.invalid".into()),
        Limits {
            command_timeout: Duration::from_secs(60),
            push_attempts: 3,
            retry_delay: Duration::from_millis(20),
        },
    )
}

fn remote(central: &Central) -> Remote {
    Remote::new(&central.url, "node-secret", None, "attempt-token")
}

#[test]
fn first_attempt_clones_later_attempts_fetch_and_push_under_the_lease() {
    let central = Central::start();
    let state = tempfile::tempdir().unwrap();
    let node = node(state.path());
    let remote = remote(&central);
    let first = node
        .start(
            &remote,
            &central.grant("t1", None),
            Uuid::from_u128(1),
            Start::Base,
        )
        .unwrap();
    assert_eq!(first.synced, Synced::Cloned);
    assert_eq!(first.start, central.head("refs/heads/main").unwrap());
    std::fs::write(first.path.join("b.txt"), "two\n").unwrap();
    let Pushed::Pushed { task_ref, commit } = node.finish(&remote, &first, "attempt 1").unwrap()
    else {
        panic!("first push conflicted")
    };
    assert_eq!(task_ref, "refs/heads/aq/t1");
    assert_eq!(central.head("refs/heads/aq/t1").as_ref(), Some(&commit));
    node.end(&first).unwrap();
    assert!(!first.path.exists());

    // The repository is kept: a marker in it survives the next attempt.
    let repo = node.repository(Uuid::from_u128(7));
    std::fs::write(repo.join("kept"), "").unwrap();
    let returned = central.grant("t1", Some(commit.clone()));
    let second = node
        .start(&remote, &returned, Uuid::from_u128(2), Start::TaskBranch)
        .unwrap();
    assert_eq!(second.synced, Synced::Fetched);
    assert!(repo.join("kept").exists());
    assert_eq!(second.start, commit);
    assert_eq!(
        std::fs::read_to_string(second.path.join("b.txt")).unwrap(),
        "two\n"
    );
    std::fs::remove_file(second.path.join("a.txt")).unwrap();
    let Pushed::Pushed { commit: next, .. } = node.finish(&remote, &second, "attempt 2").unwrap()
    else {
        panic!("second push conflicted")
    };
    assert_eq!(
        git(&central.remote(), &["rev-parse", &format!("{next}^")]),
        commit
    );
    node.end(&second).unwrap();

    // Every request carried the node credential and the attempt's token.
    let seen = central.seen.lock().unwrap().clone();
    assert!(
        seen.iter()
            .all(|l| l.ends_with("Bearer node-secret attempt-token")),
        "{seen:?}"
    );
    assert!(seen.iter().any(|l| l.starts_with("POST git-upload-pack")));
}

#[test]
fn two_slots_work_in_separate_worktrees_of_one_repository_concurrently() {
    let central = Central::start();
    let state = tempfile::tempdir().unwrap();
    let tasks: Vec<_> = (0..2)
        .map(|n| {
            let (root, grant) = (
                state.path().to_owned(),
                central.grant(&format!("s{n}"), None),
            );
            let remote = remote(&central);
            std::thread::spawn(move || {
                // Each slot is its own process with its own NodeGit on the shared root.
                let node = node(&root);
                let wt = node
                    .start(&remote, &grant, Uuid::from_u128(10 + n), Start::Base)
                    .unwrap();
                std::fs::write(wt.path.join(format!("slot{n}.txt")), "x").unwrap();
                let pushed = node.finish(&remote, &wt, "slot").unwrap();
                (wt, pushed)
            })
        })
        .collect();
    let done: Vec<_> = tasks.into_iter().map(|t| t.join().unwrap()).collect();
    assert_ne!(done[0].0.path, done[1].0.path);
    assert_eq!(done[0].0.repository, done[1].0.repository);
    let synced: Vec<_> = done.iter().map(|(w, _)| &w.synced).collect();
    assert!(synced.contains(&&Synced::Cloned) && synced.contains(&&Synced::Fetched));
    for (n, (_, pushed)) in done.iter().enumerate() {
        let Pushed::Pushed { commit, .. } = pushed else {
            panic!("conflict")
        };
        assert_eq!(
            central.head(&format!("refs/heads/aq/s{n}")).as_ref(),
            Some(commit)
        );
    }
    let node = node(state.path());
    for (wt, _) in &done {
        node.end(wt).unwrap();
        assert!(!wt.path.exists());
    }
    let repo = &done[0].0.repository;
    assert_eq!(
        git(repo, &["worktree", "list", "--porcelain"])
            .matches("worktree ")
            .count(),
        1
    );
    assert_eq!(
        git(repo, &["for-each-ref", "--format=%(refname)"]),
        "refs/remotes/central/main"
    );
    let repositories = std::fs::read_dir(state.path().join("repositories")).unwrap();
    assert_eq!(
        repositories
            .filter(|e| e.as_ref().unwrap().path().is_dir())
            .count(),
        1
    );
}

#[test]
fn a_push_whose_connection_dropped_is_retried_with_the_same_lease() {
    let central = Central::start();
    let state = tempfile::tempdir().unwrap();
    let (node, remote) = (node(state.path()), remote(&central));
    let wt = node
        .start(
            &remote,
            &central.grant("r1", None),
            Uuid::from_u128(3),
            Start::Base,
        )
        .unwrap();
    std::fs::write(wt.path.join("c.txt"), "three\n").unwrap();
    central.drop_push.store(true, Ordering::SeqCst);
    let Pushed::Pushed { commit, .. } = node.finish(&remote, &wt, "retried").unwrap() else {
        panic!("a retry after a lost reply must see its own push, not a conflict")
    };
    assert_eq!(central.head("refs/heads/aq/r1").as_ref(), Some(&commit));
    // Two push runs; the second found the ref already at its commit.
    assert_eq!(
        central.requests("GET info/refs?service=git-receive-pack"),
        2
    );
    node.end(&wt).unwrap();
}

#[test]
fn a_task_ref_moved_since_the_offer_is_a_push_conflict_and_never_forced() {
    let central = Central::start();
    let state = tempfile::tempdir().unwrap();
    let (node, remote) = (node(state.path()), remote(&central));
    let other = git(
        &central.remote(),
        &["commit-tree", "main^{tree}", "-p", "main", "-m", "other"],
    );
    git(
        &central.remote(),
        &["update-ref", "refs/heads/aq/c1", &other],
    );
    // No lease (the offer saw no branch), and a stale lease.
    let stale = central.head("refs/heads/main");
    for (n, lease) in [None, stale].into_iter().enumerate() {
        let grant = central.grant("c1", lease);
        let wt = node
            .start(
                &remote,
                &grant,
                Uuid::from_u128(20 + n as u128),
                Start::Base,
            )
            .unwrap();
        std::fs::write(wt.path.join("d.txt"), "four\n").unwrap();
        assert_eq!(
            node.finish(&remote, &wt, "conflict").unwrap(),
            Pushed::Conflict
        );
        assert_eq!(central.head("refs/heads/aq/c1"), Some(other.clone()));
        node.end(&wt).unwrap();
    }
}

#[test]
fn a_grant_that_is_not_a_task_branch_is_refused_before_git_runs() {
    let central = Central::start();
    let mut grant = central.grant("v1", None);
    grant.task_ref = "refs/heads/main".into();
    assert!(grant.validate().is_err());
    grant.task_ref = "refs/heads/aq/v1".into();
    grant.validate().unwrap();
    grant.base = "--upload-pack=touch".into();
    assert!(grant.validate().is_err());
}
