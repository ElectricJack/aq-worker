//! Against an axum double of the server's environment routes, on the same
//! origin as aq-node-git's double of the git routes.
use super::*;
use aq_node_git::test_support::{Central, config as git_config};
use axum::{Json, Router, http::StatusCode, response::IntoResponse as _, routing::post};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use uuid::Uuid;

#[derive(Default)]
struct Double {
    jobs: VecDeque<Value>,
    forbidden: bool,
    claims: usize,
    completed: Vec<Value>,
}

fn server(double: Arc<Mutex<Double>>) -> Router {
    let claimed = Arc::clone(&double);
    let claim = move |h: axum::http::HeaderMap| async move {
        assert_eq!(h["authorization"], "Bearer node-secret");
        let mut d = claimed.lock().unwrap();
        d.claims += 1;
        if d.forbidden {
            return StatusCode::FORBIDDEN.into_response();
        }
        match d.jobs.pop_front() {
            Some(job) => Json(job).into_response(),
            None => StatusCode::NO_CONTENT.into_response(),
        }
    };
    let complete = move |h: axum::http::HeaderMap, Json(result): Json<Value>| async move {
        assert_eq!(h["authorization"], "Bearer node-secret");
        let job_id = result["job_id"].clone();
        double.lock().unwrap().completed.push(result);
        Json(json!({"job_id": job_id, "recorded": true}))
    };
    Router::new()
        .route("/v1/worker/environments/claim", post(claim))
        .route("/v1/worker/environments/complete", post(complete))
}

/// Stands in for `template_tool.py`: records what it got, then acts out `mode`.
const FAKE_TOOL: &str = r#"
import json, pathlib, sys, time
capture, mode, args = pathlib.Path(sys.argv[1]), sys.argv[2], sys.argv[3:]
opts = dict(zip(args[1::2], args[2::2]))
job = json.loads(pathlib.Path(opts['--job']).read_text())
(capture / 'args.json').write_text(json.dumps(args))
(capture / 'job.json').write_text(json.dumps(job))
(capture / 'tree').write_text((pathlib.Path(opts['--repo']) / 'b.txt').read_text())
if mode == 'crash':
    sys.stderr.write('Traceback (most recent call last):\nValueError: API_TOKEN=supersecretvalue123\n')
    sys.exit(1)
if mode == 'hang':
    sys.stderr.write('building\n')
    sys.stderr.flush()
    time.sleep(60)
if mode == 'slow':
    (capture / 'started').write_text('')
    time.sleep(1)
long = 'a' * 5000 + 'END'
print(json.dumps({'job_id': job['job_id'], 'outcome': 'built', 'failure': None, 'run': None,
    'builds': [{'name': 'web', 'digest': 'sha256:' + '1' * 64, 'image': 'r@sha256:' + '1' * 64,
            'recipe_sha256': '2' * 64, 'provides': ['node'], 'warm': [], 'harnesses': [],
            'toolchain': [{'command': 'node --version', 'output': long},
                          {'command': 'pnpm --version', 'output': 'b' * 6000 + 'TAIL'},
                          {'command': 'true', 'output': 'v1'}]}]}))
"#;

struct Harness {
    central: Central,
    double: Arc<Mutex<Double>>,
    state: tempfile::TempDir,
    capture: tempfile::TempDir,
    commit: String,
}

impl Harness {
    fn new() -> Self {
        let double = Arc::new(Mutex::new(Double::default()));
        let central = Central::start_with(server(Arc::clone(&double)));
        let main = central.head("refs/heads/main").unwrap();
        let commit = central.commit("refs/heads/aq/t1", &main, "b.txt", "tree at job\n");
        let state = tempfile::tempdir().unwrap();
        std::fs::write(
            state.path().join("builder.json"),
            r#"{"target_repository": "registry.example/aq/environments"}"#,
        )
        .unwrap();
        Self {
            central,
            double,
            state,
            capture: tempfile::tempdir().unwrap(),
            commit,
        }
    }
    fn job(&self, n: u128, kind: &str, log_bytes: u64, build_wall_ms: u64) -> Value {
        json!({
            "job_id": Uuid::from_u128(n), "kind": kind,
            "project_id": Uuid::from_u128(5), "project": "web",
            "repository_id": Uuid::from_u128(7), "commit": self.commit,
            "recipes": ["web"], "run": if kind == "try" { json!("pnpm test") } else { Value::Null },
            "base_template": format!("ghcr.io/aq/base@sha256:{}", "3".repeat(64)),
            "target_repository": "chosen-by-the-server.example/ignored",
            "limits": {"max_install_steps": 64, "max_step_args": 32, "max_arg_bytes": 4096,
                "max_prefetch": 16, "max_recipe_bytes": 1048576, "build_wall_ms": build_wall_ms,
                "build_cpu_millis": 4000, "build_memory_bytes": 8589934592u64,
                "log_bytes": log_bytes, "try_concurrency": 2, "keep_versions": 3},
        })
    }
    fn queue(&self, job: Value) {
        self.double.lock().unwrap().jobs.push_back(job);
    }
    fn config(&self) -> EnvironmentsConfig {
        EnvironmentsConfig {
            build_config: self.state.path().join("builder.json"),
            work_dir: self.state.path().join("jobs"),
            node_git: git_config(self.state.path()),
            claim_interval_ms: 0,
        }
    }
    fn server(&self) -> Server {
        Server {
            url: self.central.url.clone(),
            credential: "node-secret".into(),
            cf_access: None,
        }
    }
    /// The fake tool for jobs; the real one redacts logs.
    fn tool(
        &self,
        mode: &'static str,
    ) -> impl Fn(&[OsString]) -> anyhow::Result<Tool> + Send + Sync + use<> {
        let capture = self.capture.path().to_owned();
        move |args: &[OsString]| {
            let dir = tempfile::tempdir()?;
            if args[0] == "redact-log" {
                let real = Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../packaging/sandbox/template_tool.py");
                let mut command = std::process::Command::new("python3");
                command.arg("-I").arg(real).args(args);
                return Ok((dir, command));
            }
            let script = dir.path().join("fake_tool.py");
            std::fs::write(&script, FAKE_TOOL)?;
            let mut command = std::process::Command::new("python3");
            command
                .arg("-I")
                .arg(script)
                .arg(&capture)
                .arg(mode)
                .args(args);
            Ok((dir, command))
        }
    }
    fn captured(&self, name: &str) -> PathBuf {
        self.capture.path().join(name)
    }
    fn completed(&self) -> Vec<Value> {
        self.double.lock().unwrap().completed.clone()
    }
    /// Runs the runner until `done` holds, then shuts it down and waits for it.
    fn run(&self, mode: &'static str, done: impl Fn(&Self) -> bool) {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let (tx, rx) = tokio::sync::watch::channel(false);
            let (server, config, tool) = (self.server(), self.config(), self.tool(mode));
            let runner = tokio::spawn(async move { run(&server, config, rx, tool).await });
            let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
            while !done(self) {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "runner never got there"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            tx.send(true).unwrap();
            let finished = tokio::time::timeout(Duration::from_secs(60), runner).await;
            finished.expect("runner stops").unwrap().unwrap();
        });
    }
}

fn free_text(result: &Value) -> usize {
    let toolchain = result["builds"].as_array().into_iter().flatten();
    let outputs = toolchain.flat_map(|b| b["toolchain"].as_array().unwrap().iter());
    outputs
        .map(|t| t["output"].as_str().unwrap().len())
        .sum::<usize>()
        + result["failure"]["log"].as_str().map_or(0, str::len)
        + result["run"]["output"].as_str().map_or(0, str::len)
}

fn no_job_dirs_left(state: &Path) {
    let left: Vec<_> = std::fs::read_dir(state.join("jobs")).unwrap().collect();
    assert!(left.is_empty(), "{left:?}");
    let repo = state
        .join("repositories")
        .join(format!("{}.git", Uuid::from_u128(7)));
    let list = aq_node_git::test_support::git(&repo, &["worktree", "list"]);
    assert_eq!(list.lines().count(), 1, "{list}");
}

#[test]
fn claims_a_build_job_runs_the_tool_and_completes_it() {
    let h = Harness::new();
    h.queue(h.job(1, "build", 1000, 600_000));
    h.run("ok", |h| !h.completed().is_empty());

    let args: Vec<String> =
        serde_json::from_str(&std::fs::read_to_string(h.captured("args.json")).unwrap()).unwrap();
    assert_eq!(args[0], "environment-build");
    for flag in ["--job", "--repo", "--build-config", "--output"] {
        assert!(args.contains(&flag.to_string()), "{args:?}");
    }
    // The tool ran on a worktree of the node's repository at the job's commit.
    assert_eq!(
        std::fs::read_to_string(h.captured("tree")).unwrap(),
        "tree at job\n"
    );
    let seen = h.central.seen.lock().unwrap().clone();
    let job = Uuid::from_u128(1);
    assert!(
        seen.iter().any(|l| l.contains(&format!("job={job}"))),
        "{seen:?}"
    );
    // The Job the tool expects: the node's target repository, the job's base template.
    let written: Value =
        serde_json::from_str(&std::fs::read_to_string(h.captured("job.json")).unwrap()).unwrap();
    assert_eq!(
        written["target_repository"],
        "registry.example/aq/environments"
    );
    assert_eq!(
        written["base_template"],
        h.job(1, "build", 1000, 600_000)["base_template"]
    );
    assert!(written.get("repository_id").is_none());

    let completed = h.completed();
    assert_eq!(completed.len(), 1);
    let result = &completed[0];
    assert_eq!(result["job_id"], json!(job));
    assert_eq!(result["outcome"], "built");
    // Free text fits log_bytes in total; each output keeps its tail.
    assert!(free_text(result) <= 1000, "{}", free_text(result));
    let outputs = &result["builds"][0]["toolchain"];
    assert!(outputs[0]["output"].as_str().unwrap().ends_with("END"));
    assert!(outputs[1]["output"].as_str().unwrap().ends_with("TAIL"));
    assert_eq!(outputs[2]["output"], "v1");
    no_job_dirs_left(h.state.path());
}

#[test]
fn a_tool_crash_completes_the_job_as_failed_with_a_redacted_log() {
    let h = Harness::new();
    h.queue(h.job(2, "try", 4096, 600_000));
    h.run("crash", |h| !h.completed().is_empty());

    let args: Vec<String> =
        serde_json::from_str(&std::fs::read_to_string(h.captured("args.json")).unwrap()).unwrap();
    assert_eq!(args[0], "environment-try");
    assert!(!args.contains(&"--output".to_string()));
    let result = &h.completed()[0];
    assert_eq!(result["outcome"], "failed");
    assert_eq!(result["builds"], json!([]));
    assert_eq!(result["run"], Value::Null);
    assert_eq!(result["failure"]["recipe"], "web");
    assert_eq!(result["failure"]["step"], "install");
    let log = result["failure"]["log"].as_str().unwrap();
    assert!(
        log.contains("API_TOKEN=") && log.contains("ValueError"),
        "{log}"
    );
    assert!(!log.contains("supersecretvalue123"), "{log}");
    no_job_dirs_left(h.state.path());
}

#[test]
fn a_tool_past_build_wall_ms_is_killed_and_the_job_completed_as_failed() {
    let h = Harness::new();
    h.queue(h.job(3, "build", 4096, 1500));
    let started = std::time::Instant::now();
    h.run("hang", |h| !h.completed().is_empty());
    assert!(started.elapsed() < Duration::from_secs(30));
    let result = &h.completed()[0];
    assert_eq!(result["outcome"], "failed");
    assert_eq!(result["failure"]["step"], "install");
    let log = result["failure"]["log"].as_str().unwrap();
    assert!(
        log.contains("building") && log.contains("build_wall_ms"),
        "{log}"
    );
    no_job_dirs_left(h.state.path());
}

#[test]
fn shutdown_stops_claiming_and_finishes_the_running_job() {
    let h = Harness::new();
    h.queue(h.job(4, "build", 65536, 600_000));
    h.queue(h.job(5, "build", 65536, 600_000));
    h.run("slow", |h| h.captured("started").exists());

    let completed = h.completed();
    assert_eq!(completed.len(), 1, "{completed:?}");
    assert_eq!(completed[0]["job_id"], json!(Uuid::from_u128(4)));
    assert_eq!(completed[0]["outcome"], "built");
    let double = h.double.lock().unwrap();
    assert_eq!((double.claims, double.jobs.len()), (1, 1));
    drop(double);
    no_job_dirs_left(h.state.path());
}

#[test]
fn a_refused_fetch_completes_nothing_and_runs_no_tool() {
    let h = Harness::new();
    h.central.refuse.store(true, Ordering::SeqCst);
    h.queue(h.job(6, "build", 65536, 600_000));
    h.run("ok", |h| h.double.lock().unwrap().claims >= 2);
    assert!(h.completed().is_empty());
    assert!(!h.captured("args.json").exists());
    assert!(
        std::fs::read_dir(h.state.path().join("jobs"))
            .unwrap()
            .next()
            .is_none()
    );
}

#[test]
fn a_node_outside_the_builder_group_backs_off() {
    let h = Harness::new();
    h.double.lock().unwrap().forbidden = true;
    let started = std::time::Instant::now();
    h.run("ok", |_| started.elapsed() > Duration::from_millis(1500));
    let claims = h.double.lock().unwrap().claims;
    assert!((1..=2).contains(&claims), "{claims} claims in 1.5 s");
}

#[test]
fn free_text_is_trimmed_to_log_bytes_in_total_keeping_each_tail() {
    let mut result = json!({
        "failure": {"log": format!("{}LOG", "x".repeat(100))},
        "run": {"output": "short"},
        "builds": [{"toolchain": [{"output": format!("{}é1", "y".repeat(50))}]}],
    });
    fit(&mut result, 60);
    assert!(free_text(&result) <= 60, "{result}");
    assert_eq!(result["run"]["output"], "short");
    assert!(result["failure"]["log"].as_str().unwrap().ends_with("LOG"));
    assert!(
        result["builds"][0]["toolchain"][0]["output"]
            .as_str()
            .unwrap()
            .ends_with("é1")
    );
    fit(&mut result, 0);
    assert_eq!(free_text(&result), 0);
}
