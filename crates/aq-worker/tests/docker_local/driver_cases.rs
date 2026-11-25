/// Node settings with a build cache for `project`: the cargo target and, the
/// same for Claude and Codex whatever their home, the cargo registry.
fn build_cache(
    root: &std::path::Path,
    project: Uuid,
    slot_max_bytes: u64,
) -> aq_sandbox_archive::build_cache::BuildCacheConfig {
    serde_json::from_value(json!({
        "root": root,
        "limits": {
            "max_bytes": 1 << 30, "slot_max_bytes": slot_max_bytes, "slot_max_age_ms": 60_000,
            "slot_watch_interval_ms": 0, "refresh_interval_ms": 60_000, "warm_timeout_ms": 60_000,
            "warm_cpu_millis": 2000, "warm_memory_bytes": 1 << 30, "warm_pids": 64,
        },
        "projects": { project.to_string(): { "paths": [
            { "target": "/tmp/aq-target", "env": "CARGO_TARGET_DIR" },
            { "target": "/aq/cache/cargo", "env": "CARGO_HOME" },
        ]}},
    }))
    .unwrap()
}

/// A project with a build cache: the sandbox gets its task's slot at each cache
/// path and the variables naming them, the upload is recorded in the slot's
/// ledger, and the cache is no part of what is copied back.
#[tokio::test]
async fn sandbox_driver_mounts_the_tasks_build_cache_outside_the_workspace() {
    let root = tempfile::tempdir().unwrap();
    let (project, task) = (Uuid::from_u128(7), Uuid::from_u128(8));
    let config = build_cache(root.path(), project, 1 << 30);
    assert!(
        config.slot(Uuid::from_u128(9), task, "aa").is_none(),
        "another project has no cache"
    );
    let slot = config.slot(project, task, "aa");
    let final_message = "Done.\nChanges: fixture-output; Checks: none; Limits: none";
    let run = run_driver(final_message, &[], (b"codex tui\r\n", b""), |c| {
        c.build_cache = slot
    })
    .await;
    assert_eq!(run.code, 0, "proven success");
    let names = run.docker.container_names();
    let container = run.docker.container(&names[0]).unwrap();
    let binds = container.body["HostConfig"]["Binds"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(binds.len(), 2, "{binds:?}");
    let (source, target) = binds[0].as_str().unwrap().split_once(':').unwrap();
    assert_eq!(target, "/tmp/aq-target");
    assert!(
        binds[1]
            .as_str()
            .unwrap()
            .ends_with("/p1/upper:/aq/cache/cargo")
    );
    let slot = std::path::Path::new(source).parent().unwrap();
    let slot = slot.parent().unwrap();
    assert!(
        slot.starts_with(root.path().join(project.to_string())) && slot.ends_with(task.to_string()),
        "{binds:?}"
    );
    let env = String::from_utf8(container.layer["/aq/in/env"].data.clone()).unwrap();
    assert!(
        env.ends_with("CARGO_TARGET_DIR=/tmp/aq-target\nCARGO_HOME=/aq/cache/cargo\n"),
        "{env}"
    );
    let ledger = std::fs::read_to_string(slot.join("ledger.json")).unwrap();
    assert!(ledger.contains("README.md"), "{ledger}");
    let work = run.dir.path().join("work");
    let mut copied: Vec<_> = std::fs::read_dir(&work)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    copied.sort();
    assert_eq!(
        copied,
        ["README.md", "fixture-output"],
        "only the workspace is copied back"
    );
}

/// SBX20 for the slot, which the daemon's disk samples do not cover: an attempt
/// whose slot is over `slot_max_bytes` is stopped like one over its transcript
/// limit (4, work copied back) with the `sandbox_disk_limit_exceeded` claim, and
/// the slot is deleted.
#[tokio::test]
async fn sandbox_driver_stops_an_attempt_whose_build_cache_slot_is_over_its_limit() {
    use aq_handoff_note::{DISK_LIMIT, Stop, handoff_note};
    let root = tempfile::tempdir().unwrap();
    let (project, task) = (Uuid::from_u128(7), Uuid::from_u128(8));
    // Even the slot's empty directories are over a one-byte limit.
    let slot = build_cache(root.path(), project, 1).slot(project, task, "aa");
    let run = run_driver(DONE, &[], (b"", b""), |c| c.build_cache = slot).await;
    assert_eq!(
        run.code, 4,
        "stopped with its work saved, not a finished turn"
    );
    let names = run.docker.container_names();
    let container = run.docker.container(&names[0]).unwrap();
    assert_eq!(container.status, "exited", "the sandbox is stopped");
    assert!(run.dir.path().join("work/fixture-output").exists());
    let slots: Vec<_> = (walk(root.path()).into_iter())
        .filter(|p| p.ends_with(task.to_string()))
        .collect();
    assert!(slots.is_empty(), "the oversized slot is deleted: {slots:?}");
    let note = handoff_note(Stop::BuildCacheBytes, &[], &[], &[]);
    assert!(
        note.starts_with(&format!("Stopped ({DISK_LIMIT}: build cache slot): ")),
        "{note}"
    );
}
fn walk(dir: &std::path::Path) -> Vec<PathBuf> {
    let children = std::fs::read_dir(dir).into_iter().flatten().flatten();
    let children = children.map(|c| c.path()).filter(|p| p.is_dir());
    children
        .flat_map(|p| [walk(&p), vec![p]].concat())
        .collect()
}

/// Epic 0 W1: the Codex launch path in the docker-local sandbox, against the Docker
/// double: the same sandbox, copy-in/copy-out and stop as Claude; the Codex runner and
/// the interactive `codex --sandbox danger-full-access` on the sandbox's terminal with
/// the offered model and effort; the ChatGPT login only as private material; the
/// attempt ends when the turn does, with the claim from Codex's final message.
fn task_author() -> aq_worker_protocol::git_author::GitAuthor {
    aq_worker_protocol::git_author::GitAuthor {
        name: "Zoë \"Task Author\" Example".into(),
        email: "Task+AQ@example.invalid".into(),
    }
}

fn assert_task_git_environment(environment: &str) {
    for (key, value) in task_author().tool_environment() {
        assert!(
            environment
                .lines()
                .any(|line| line == format!("{key}={value}")),
            "{key}"
        );
    }
}

#[tokio::test]
async fn sandbox_driver_runs_codex_in_the_docker_local_sandbox() {
    let run = run_driver(
        "Done.\nChanges: fixture-output; Checks: none; Limits: none",
        &[],
        (b"codex tui\r\n", b""),
        |config| config.apply_git_author(&task_author()).unwrap(),
    )
    .await;
    assert_eq!(run.code, 0, "proven success");
    let work = run.dir.path().join("work");
    assert_eq!(
        std::fs::read(work.join("fixture-output")).unwrap(),
        b"fixture-complete\n",
        "the runner's work was copied back"
    );
    let names = run.docker.container_names();
    assert_eq!(names.len(), 1);
    let container = run.docker.container(&names[0]).unwrap();
    assert_eq!(container.status, "exited", "the sandbox is stopped");
    assert_workspace_evidence(&container, &work);
    assert_driver_arguments(&container);
    assert_driver_pane(&run, &container, &names[0]);
    assert_private_material(&run, &container);
}

#[tokio::test]
async fn task_postgres_is_started_only_for_the_declared_sandbox() {
    let run = run_driver(
        "Done.\nChanges: fixture-output; Checks: none; Limits: none",
        &[],
        (b"codex tui\r\n", b""),
        |config| config.postgres = true,
    )
    .await;
    assert_eq!(run.code, 0);
    let name = run.docker.container_names().remove(0);
    let container = run.docker.container(&name).unwrap();
    let config = &container.body["HostConfig"];
    assert_eq!(
        config["Tmpfs"]["/run/aq-postgres"],
        aq_sandbox_archive::task_postgres::TMPFS
    );
    assert_eq!(config["CapDrop"], json!(["ALL"]));
    assert_eq!(config["PortBindings"], json!({}));
    assert!(
        container.body["Cmd"][2]
            .as_str()
            .unwrap()
            .contains("/run/aq-postgres/status")
    );
    let args = std::str::from_utf8(&container.layer["/aq/in/args"].data).unwrap();
    assert!(args.contains("shell_environment_policy.set.PGHOST="));
    let requests = run.docker.requests();
    let order = |suffix: &str| {
        requests
            .iter()
            .position(|(method, path)| *method == DockerMethod::Post && path.ends_with(suffix))
            .unwrap()
    };
    assert!(
        order(&format!("/containers/{name}/start")) < order(&format!("/containers/{name}/exec"))
    );
    assert!(
        order(&format!("/containers/{name}/exec"))
            < order(&format!("/exec/{}/start", "1".repeat(64)))
    );
}

fn assert_workspace_evidence(container: &Container, work: &std::path::Path) {
    // Of Codex's home only `sessions` is mounted; the transcript written there is not
    // part of the workspace that is copied back.
    assert!(container.body["HostConfig"]["Tmpfs"].is_null());
    let mounts = container.body["HostConfig"]["Mounts"].as_array().unwrap();
    let targets: Vec<_> = mounts
        .iter()
        .map(|m| m["Target"].as_str().unwrap())
        .collect();
    assert_eq!(targets, ["/aq/work", "/aq/home/codex/sessions"]);
    assert!(
        container
            .layer
            .contains_key("/aq/home/codex/sessions/2026/rollout.jsonl")
    );
    let mut copied: Vec<_> = (std::fs::read_dir(work).unwrap())
        .map(|entry| entry.unwrap().file_name())
        .collect();
    copied.sort();
    assert_eq!(copied, ["README.md", "fixture-output"]);
}

fn assert_driver_arguments(container: &Container) {
    let body = container.body.to_string();
    assert!(body.contains("CODEX_HOME/auth.json") && !body.contains("CLAUDE_CODE_OAUTH_TOKEN"));
    let text = |path: &str| String::from_utf8(container.layer[path].data.clone()).unwrap();
    let args: Vec<String> = text("/aq/in/args").lines().map(Into::into).collect();
    let at = |flag: &str| args[args.iter().position(|a| a == flag).unwrap() + 1].clone();
    assert_eq!(args[0], "--no-daemon");
    assert!(
        !args.contains(&"exec".to_string()),
        "the TUI, not `codex exec`"
    );
    assert_eq!(args.last().map(String::as_str), Some("--"));
    assert_eq!(at("--sandbox"), "danger-full-access");
    assert_eq!(at("-m"), "gpt-5.5");
    assert!(args.contains(&"model_reasoning_effort=\"high\"".to_string()));
    assert_eq!(text("/aq/in/executable"), "/opt/aq/bin/codex");
    let env = text("/aq/in/env");
    assert!(env.starts_with("CODEX_HOME=/aq/home/codex\n"));
    assert_task_git_environment(&env);
    for (key, value) in task_author().tool_environment() {
        let prefix = format!("shell_environment_policy.set.{key}=");
        let encoded = args
            .iter()
            .find_map(|arg| arg.strip_prefix(&prefix))
            .unwrap();
        let parsed: toml::Value = toml::from_str(&format!("value={encoded}")).unwrap();
        assert_eq!(parsed["value"].as_str(), Some(value));
    }
}

fn assert_driver_pane(run: &CodexDriverRun, container: &Container, id: &str) {
    // A terminal for the TUI, attached to the pane; the TUI never exits by itself.
    let create: Value = container.body.clone();
    for key in ["Tty", "OpenStdin", "AttachStdin"] {
        assert_eq!(create[key], true, "{key}");
    }
    assert!(
        run.attach.ends_with(&format!(
            "/containers/{}/attach?stream=1&stdin=1&stdout=1&stderr=1",
            id
        )),
        "{}",
        run.attach
    );
    assert_eq!(run.screen, b"codex tui\r\n");
}

fn assert_private_material(run: &CodexDriverRun, container: &Container) {
    let body = container.body.to_string();
    // The login reaches the sandbox only as private material, never argv, env or a path.
    assert_eq!(container.layer["/aq/private/material"].data, run.auth);
    let (_, refresh) = codex_login();
    assert!(!body.contains(&refresh));
    for (_, path) in run.docker.requests() {
        assert!(!path.contains(&refresh));
    }
    let private = run.dir.path().join("private");
    let exit: Value =
        serde_json::from_slice(&std::fs::read(private.join("exit.json")).unwrap()).unwrap();
    assert_eq!(exit["success"], true);
    assert!(private.join("codex.jsonl").exists() && !private.join("claude.jsonl").exists());
}

/// A login token that the agent writes into the workspace is refused: nothing is
/// copied back and the attempt is not a success.
#[tokio::test]
async fn sandbox_driver_refuses_a_codex_login_token_in_the_workspace() {
    let (_, refresh) = codex_login();
    let leak = format!("export T={refresh}\n").into_bytes();
    let run = run_codex_driver(
        "Done.\nChanges: leak.txt; Checks: none; Limits: none",
        &[("/aq/work/leak.txt", leak)],
    )
    .await;
    assert_ne!(run.code, 0);
    let work = run.dir.path().join("work");
    assert!(!work.join("leak.txt").exists() && !work.join("fixture-output").exists());
    let private = run.dir.path().join("private");
    assert!(
        !private.join("codex.jsonl").exists(),
        "no transcript when a secret leaked"
    );
}

/// A Claude driver configuration keeps its bytes (no `harness` key); a Codex one names
/// its harness and reads back the same.
#[test]
fn claude_driver_config_has_no_harness_key_and_codex_round_trips() {
    use aq_worker::sandbox_driver::{DriverConfig, DriverHarness};
    let claude = json!({
        "docker_local": DockerLocalConfig::new("/run/x.sock".into()),
        "template": format!("sha256:{HEX}"),
        "owner": {"node_id": Uuid::from_u128(1), "boot_id": Uuid::from_u128(2),
            "attempt_id": Uuid::from_u128(3)},
        "hard_deadline_ms": 1, "resources": {"cpu_millis": 1000, "memory_bytes": 1u64 << 30, "pids": 64},
        "workspace": "/w", "token_file": "/t", "executable": "/opt/aq/bin/claude", "arguments": ["-p"],
        "environment": {}, "settings_json": "{}", "result_dir": null, "max_workspace_bytes": 1,
    });
    let config: DriverConfig = serde_json::from_value(claude).unwrap();
    assert_eq!(config.harness, DriverHarness::Claude);
    let written = serde_json::to_value(&config).unwrap();
    assert!(written.get("harness").is_none(), "{written}");
    let mut codex = config;
    codex.harness = DriverHarness::Codex;
    let written = serde_json::to_value(&codex).unwrap();
    assert_eq!(written["harness"], "codex");
    let back: DriverConfig = serde_json::from_value(written).unwrap();
    assert_eq!(back.harness, DriverHarness::Codex);
}

/// What the owner types on the pane reaches the sandbox terminal as typed. A login
/// token the TUI prints is masked before it reaches the pane, and the attempt then
/// fails with nothing copied back and no transcript.
#[tokio::test]
async fn sandbox_driver_relays_typing_and_masks_a_login_token_on_the_pane() {
    let run = run_codex_driver_on(
        "Done.\nChanges: none; Checks: none; Limits: none",
        &[],
        b"ready\r\n",
        b"also update the README\r",
    )
    .await;
    assert_eq!(run.code, 0);
    assert_eq!(run.typed, b"also update the README\r");
    assert_eq!(run.screen, b"ready\r\n");

    let (_, refresh) = codex_login();
    let tui = format!("token: {refresh}\r\n");
    let run = run_codex_driver_on(
        "Done.\nChanges: none; Checks: none; Limits: none",
        &[],
        tui.as_bytes(),
        b"",
    )
    .await;
    assert_ne!(
        run.code, 0,
        "a login secret on the terminal fails the attempt"
    );
    let screen = String::from_utf8(run.screen).unwrap();
    assert_eq!(screen, format!("token: {}\r\n", "*".repeat(refresh.len())));
    let work = run.dir.path().join("work");
    assert!(!work.join("fixture-output").exists());
    assert!(!run.dir.path().join("private/codex.jsonl").exists());
}

const DONE: &str = "Done.\nChanges: fixture-output; Checks: none; Limits: none";

/// The transcript mount is outside the sandbox's disk limit, so the driver stops an
/// attempt whose mount passes the node's byte or entry limit: the sandbox is stopped,
/// the work is copied back and the driver exits as for a handoff (4), with the
/// `transcript_limit_exceeded` claim. A link out of the mount is not followed.
#[tokio::test]
async fn sandbox_driver_stops_an_attempt_whose_transcript_passes_its_limits() {
    use aq_handoff_note::{Stop, TRANSCRIPT_LIMIT, TranscriptBound, handoff_note};
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("big"), vec![1u8; 1 << 20]).unwrap();
    let bound = TranscriptBound {
        max_bytes: 64 * 1024,
        max_files: 3,
    };
    type Fill = fn(&std::path::Path, &std::path::Path);
    let cases: [(Fill, i32); 3] = [
        (
            |mount, _| std::fs::write(mount.join("s.jsonl"), vec![1u8; 1 << 18]).unwrap(),
            4,
        ),
        (
            |mount, _| {
                (0..4).for_each(|n| std::fs::write(mount.join(n.to_string()), b"x").unwrap())
            },
            4,
        ),
        (
            |mount, outside| std::os::unix::fs::symlink(outside, mount.join("out")).unwrap(),
            0,
        ),
    ];
    for (fill, code) in cases {
        let run = run_driver(DONE, &[], (b"", b""), |config| {
            config.transcript_bound = Some(bound);
            fill(
                &config.docker_local.transcript.as_ref().unwrap().0,
                outside.path(),
            );
        })
        .await;
        assert_eq!(run.code, code);
        let names = run.docker.container_names();
        assert_eq!(run.docker.container(&names[0]).unwrap().status, "exited");
        // Stopped or finished, the work is saved the same way.
        assert!(run.dir.path().join("work/fixture-output").exists());
    }
    for (stop, limit) in [
        (Stop::TranscriptBytes, "bytes"),
        (Stop::TranscriptFiles, "files"),
    ] {
        let note = handoff_note(stop, &[], &[], &[]);
        assert!(
            note.starts_with(&format!("Stopped ({TRANSCRIPT_LIMIT}: {limit}): ")),
            "{note}"
        );
    }
}

/// A Claude launch names its session after the attempt (`--session-id`), with or
/// without a conversation outbox, so the node knows the transcript's file name.
#[tokio::test]
async fn sandbox_driver_names_the_claude_session_after_the_attempt() {
    use aq_worker::sandbox_driver::DriverHarness;
    let run = run_driver(DONE, &[], (b"", b""), |config| {
        config.harness = DriverHarness::Claude;
        config.arguments = vec!["--model".into(), "fixture".into()];
        config.environment.clear();
        config.settings_json = "{}".into();
        config.apply_git_author(&task_author()).unwrap();
        config.docker_local.transcript.as_mut().unwrap().1 =
            DriverHarness::Claude.transcript_dir().into();
        std::fs::write(&config.token_file, "fixture-login-0123456789").unwrap();
    })
    .await;
    let names = run.docker.container_names();
    let container = run.docker.container(&names[0]).unwrap();
    let args = String::from_utf8(container.layer["/aq/in/args"].data.clone()).unwrap();
    let attempt = Uuid::from_u128(3);
    assert_eq!(args, format!("--model\nfixture\n--session-id\n{attempt}\n"));
    assert_task_git_environment(std::str::from_utf8(&container.layer["/aq/in/env"].data).unwrap());
    let mounts = container.body["HostConfig"]["Mounts"].as_array().unwrap();
    assert_eq!(mounts[1]["Target"], "/aq/home/projects/-aq-work");
    assert_eq!(mounts.len(), 2);
}

#[tokio::test]
async fn sandbox_driver_isolates_related_projects_and_copies_only_writable_outputs() {
    use aq_node_git::workspaces::{checkout_path, readonly_path, sandbox_path};
    let read = Uuid::from_u128(41);
    let write = Uuid::from_u128(42);
    let target = |project| format!("{}/a.txt", sandbox_path(project));
    let read_target = target(read);
    let write_target = target(write);
    let run = run_driver(
        DONE,
        &[(&read_target, b"ignore this".to_vec()), (&write_target, b"returned write".to_vec())],
        (b"", b""),
        |config| {
            let root = config.workspace.parent().unwrap().to_path_buf();
            config.docker_local.workspace_bind_paths = Some(
                aq_sandbox_archive::docker_engine::WorkspaceBindPaths {
                    worker_root: root.clone(),
                    daemon_root: root,
                },
            );
            let grant = |project: Uuid, access: &str| json!({
                "project_id": project, "project_revision": 4, "binding_generation": 2,
                "repository_id": project, "endpoint": format!("/v1/git/workspaces/{project}/{project}"),
                "access": access, "base": "a".repeat(40), "checkout_head":"a".repeat(40),
                "task_ref":"refs/heads/aq/fixture",
                "lease":null, "update_from_main":false,
            });
            let plan = json!({"workspaces":[grant(read, "read"), grant(write, "write")]});
            std::fs::write(config.workspace.with_extension("workspaces.json"), serde_json::to_vec(&plan).unwrap()).unwrap();
            for project in [read, write] {
                let worktree = checkout_path(&config.workspace, project);
                std::fs::create_dir_all(&worktree).unwrap();
                std::fs::write(worktree.join("a.txt"), "starting source").unwrap();
                std::fs::write(worktree.join(".git"), "gitdir: node-private").unwrap();
            }
        },
    ).await;
    assert_eq!(run.code, 0);
    let work = run.dir.path().join("work");
    let names = run.docker.container_names();
    let container = run.docker.container(&names[0]).unwrap();
    let mounts = container.body["HostConfig"]["Mounts"].as_array().unwrap();
    assert!(mounts.iter().any(|mount| mount["Target"] == sandbox_path(read) && mount["ReadOnly"] == true));
    assert_eq!(std::fs::read(checkout_path(&work, read).join("a.txt")).unwrap(), b"starting source");
    assert!(!readonly_path(&work, read).join(".git").exists());
    assert_eq!(std::fs::read(checkout_path(&work, write).join("a.txt")).unwrap(), b"returned write");
    assert!(!container.layer.contains_key(&format!("{}/.git", sandbox_path(write))));
}
