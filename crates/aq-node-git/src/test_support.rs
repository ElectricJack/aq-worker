//! A local double of the server's git grant route and `/v1/git` proxy:
//! `git http-backend` over one bare repository, recording each request's
//! authentication. For this crate's tests and its callers' (`test-support`).
use crate::*;
use axum::{
    Router, body::Bytes, extract::Path as UrlPath, http::HeaderMap, response::IntoResponse as _,
    routing::any,
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

/// Where the supported images keep Git, in preference order: the pinned sandbox OS
/// (`packaging/sandbox/Dockerfile.os`), the native runtime image
/// (`packaging/native-runtime/Dockerfile`), then a system Git.
pub const GIT_CANDIDATES: [&str; 3] = [
    "/opt/git/bin/git",
    "/opt/aq-git-runtime/bin/git",
    "/usr/bin/git",
];

/// The first candidate for which `usable` holds; `None` if there is none.
pub fn select_git<'a>(candidates: &[&'a str], usable: impl Fn(&Path) -> bool) -> Option<&'a str> {
    candidates.iter().copied().find(|c| usable(Path::new(c)))
}

fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// The Git the tests run; a missing Git fails the test, it is never skipped.
pub fn git_executable() -> &'static str {
    static SELECTED: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();
    SELECTED.get_or_init(|| {
        select_git(&GIT_CANDIDATES, is_executable_file)
            .unwrap_or_else(|| panic!("no Git found at any of {GIT_CANDIDATES:?}"))
    })
}

pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new(git_executable())
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

pub struct Central {
    pub dir: tempfile::TempDir,
    pub url: String,
    /// `METHOD path?query authorization attempt` per request.
    pub seen: Arc<Mutex<Vec<String>>>,
    /// What `GET /v1/git/grants/<attempt>` answers.
    pub grants: Arc<Mutex<std::collections::HashMap<Uuid, Grant>>>,
    /// The next receive-pack is applied, then its reply is cut off.
    pub drop_push: Arc<AtomicBool>,
    /// Every smart-HTTP request is refused with 403 (a lost grant or claim).
    pub refuse: Arc<AtomicBool>,
    _runtime: tokio::runtime::Runtime,
}

impl Central {
    pub fn start() -> Self {
        Self::start_with(Router::new())
    }
    /// Also serves `extra` (a caller's double of other server routes) on the same origin.
    pub fn start_with(extra: Router) -> Self {
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
        let refuse = Arc::new(AtomicBool::new(false));
        let grants: Arc<Mutex<std::collections::HashMap<Uuid, Grant>>> = Arc::default();
        let served = Arc::clone(&grants);
        let grant = move |UrlPath(attempt): UrlPath<Uuid>, h: HeaderMap| async move {
            let bearer = h
                .get("authorization")
                .is_some_and(|v| v == "Bearer node-secret");
            match (bearer, served.lock().unwrap().get(&attempt)) {
                (false, _) => axum::http::StatusCode::FORBIDDEN.into_response(),
                (true, Some(grant)) => axum::Json(grant.clone()).into_response(),
                (true, None) => axum::http::StatusCode::NOT_FOUND.into_response(),
            }
        };
        let router = smart_http(
            dir.path().to_owned(),
            Arc::clone(&seen),
            Arc::clone(&drop_push),
            Arc::clone(&refuse),
        )
        .route("/v1/git/grants/{attempt}", axum::routing::get(grant))
        .merge(extra);
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
            grants,
            drop_push,
            refuse,
            _runtime: runtime,
        }
    }
    pub fn remote(&self) -> PathBuf {
        self.dir.path().join("remote.git")
    }
    pub fn head(&self, r: &str) -> Option<String> {
        let out = Command::new(git_executable())
            .arg("-C")
            .arg(self.remote())
            .args(["rev-parse", "--verify", "--quiet", r])
            .output()
            .unwrap();
        out.status
            .success()
            .then(|| String::from_utf8(out.stdout).unwrap().trim().into())
    }
    pub fn grant(&self, task: &str, lease: Option<String>) -> Grant {
        Grant {
            endpoint: format!("/v1/git/{}", Uuid::from_u128(7)),
            repository_id: Uuid::from_u128(7),
            base: self.head("refs/heads/main").unwrap(),
            task_ref: format!("refs/heads/aq/{task}"),
            lease,
            update_from_main: false,
        }
    }
    /// Writes `file` on top of `parent` and moves `r` there.
    pub fn commit(&self, r: &str, parent: &str, file: &str, text: &str) -> String {
        let seed = self.dir.path().join("seed");
        git(
            &seed,
            &[
                "fetch",
                "--quiet",
                "origin",
                "+refs/heads/*:refs/remotes/origin/*",
            ],
        );
        git(&seed, &["checkout", "--quiet", "--detach", parent]);
        std::fs::write(seed.join(file), text).unwrap();
        git(&seed, &["add", file]);
        git(&seed, &["commit", "--quiet", "-m", file]);
        git(
            &seed,
            &["push", "--quiet", "--force", "origin", &format!("HEAD:{r}")],
        );
        git(&seed, &["rev-parse", "HEAD"])
    }
    pub fn requests(&self, needle: &str) -> usize {
        let seen = self.seen.lock().unwrap();
        seen.iter().filter(|l| l.contains(needle)).count()
    }
}

fn smart_http(
    root: PathBuf,
    seen: Arc<Mutex<Vec<String>>>,
    drop_push: Arc<AtomicBool>,
    refuse: Arc<AtomicBool>,
) -> Router {
    let handler = move |method: axum::http::Method,
                        UrlPath((_repository, rest)): UrlPath<(Uuid, String)>,
                        uri: axum::http::Uri,
                        h: HeaderMap,
                        body: Bytes| async move {
        let text = |name: &str| {
            let value = h.get(name).and_then(|v| v.to_str().ok());
            value.unwrap_or_default().to_string()
        };
        // The header aq-server spec 0017 names; a literal so the double needs no feature.
        let job = text("aq-environment-job");
        seen.lock().unwrap().push(format!(
            "{method} {rest}{} {} {}{}",
            uri.query().map(|q| format!("?{q}")).unwrap_or_default(),
            text("authorization"),
            text(transport::ATTEMPT_HEADER),
            if job.is_empty() {
                job
            } else {
                format!(" job={job}")
            }
        ));
        if refuse.load(Ordering::SeqCst) {
            return axum::http::StatusCode::FORBIDDEN.into_response();
        }
        let mut child = Command::new(git_executable())
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

pub fn config(root: &Path) -> Config {
    Config {
        repositories: root.join("repositories"),
        git: git_executable().into(),
        author_name: "AQ node".into(),
        author_email: "node@aq.invalid".into(),
        command_timeout_ms: 60_000,
        push_attempts: 3,
        retry_delay_ms: 20,
    }
}

pub fn node(root: &Path) -> NodeGit {
    let c = config(root);
    let limits = Limits {
        command_timeout: Duration::from_millis(c.command_timeout_ms),
        push_attempts: c.push_attempts,
        retry_delay: Duration::from_millis(c.retry_delay_ms),
    };
    NodeGit::new(
        c.git,
        c.repositories,
        (c.author_name, c.author_email),
        limits,
    )
}

pub fn remote(central: &Central) -> Remote {
    Remote::new(&central.url, "node-secret", None)
}

/// Where the worker puts an attempt's worktree (under its slot's state).
pub fn at(root: &Path, attempt: u128) -> PathBuf {
    root.join("slot/worktrees")
        .join(Uuid::from_u128(attempt).to_string())
}
