use super::*;
use tokio::net::UnixListener;
/// A stand-in aq-git: `serve` listens on `--listen` (python3), `recovery clear-fence`
/// saves its request beside the script and answers ok; `AQ_FAKE_EXIT` exits at once.
pub(crate) fn fake_aq_git(dir: &Path, exit: Option<u8>) -> AqGitConfig {
    use std::os::unix::fs::PermissionsExt;
    let script = dir.join("aq-git");
    let body = match exit {
        Some(code) => format!("#!/bin/sh\nexit {code}\n"),
        None => r#"#!/bin/sh
for a; do [ "$prev" = --listen ] && listen=$a; prev=$a; done
case " $* " in
*" clear-fence "*) cat > "$(dirname "$0")/clear-fence.json"; echo '{"ok":true,"result":{}}' ;;
*" serve "*) sleep 60 & echo $! > "$(dirname "$0")/engine.pid"
  exec python3 -c 'import socket,sys,time
s=socket.socket(socket.AF_UNIX); s.bind(sys.argv[1]); s.listen(8); time.sleep(60)' "$listen" ;;
esac
"#
        .to_owned(),
    };
    std::fs::write(&script, body).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    // A thread forking elsewhere in the test process can briefly hold the write
    // descriptor (ETXTBSY on exec): wait until the script runs.
    for _ in 0..200 {
        match std::process::Command::new(&script).arg("--probe").status() {
            Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) => {
                std::thread::sleep(Duration::from_millis(10))
            }
            _ => break,
        }
    }
    AqGitConfig {
        binary: script,
        git_executable: "/usr/bin/git".into(),
    }
}
/// The state of an aq-git that was SIGKILLed: a socket nobody listens on and an active
/// incarnation whose owner lock is free.
fn killed(state: &Path, socket: &Path) {
    std::fs::create_dir_all(state.join("git")).unwrap();
    std::fs::write(state.join("git/owner.lock"), b"").unwrap();
    let record = serde_json::json!({"active": true, "id": "inc-1", "guard_pid": 1,
            "boot_id": "b", "version": 1});
    std::fs::write(state.join("git/incarnation.json"), record.to_string()).unwrap();
    drop(std::os::unix::net::UnixListener::bind(socket).unwrap());
}
/// D2 (b): after a SIGKILL the stale socket is removed and the fence cleared (the owner
/// lock is free), and start waits for the new aq-git to accept a connection.
#[tokio::test]
async fn start_reclaims_the_socket_and_fence_of_a_killed_aq_git() {
    let dir = tempfile::tempdir().unwrap();
    let (state, socket) = (dir.path().join("current"), dir.path().join("git.sock"));
    killed(&state, &socket);
    let config = fake_aq_git(dir.path(), None);
    let git = AqGit::start(&config, &state, &socket).await.unwrap();
    UnixStream::connect(&socket)
        .await
        .expect("the new aq-git listens");
    let request: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("clear-fence.json")).unwrap())
            .unwrap();
    assert_eq!(request["operation"], "recovery.clear-fence");
    assert_eq!(request["params"]["expected_incarnation"], "inc-1");
    assert_eq!(request["params"]["old_environment_stopped"], true);
    assert_eq!(request["params"]["evidence"]["owner_lock"], "free");
    git.stop().await.unwrap();
}
/// Nothing is removed while a process holds the owner lock or listens on the socket, and
/// an aq-git that exits before it listens is reported at once.
#[tokio::test]
async fn start_keeps_what_a_live_owner_holds_and_reports_an_early_exit() {
    let dir = tempfile::tempdir().unwrap();
    let (state, socket) = (dir.path().join("current"), dir.path().join("git.sock"));
    killed(&state, &socket);
    let config = fake_aq_git(dir.path(), None);
    let held = std::fs::File::open(state.join("git/owner.lock")).unwrap();
    // SAFETY: flock on our own descriptor.
    assert_eq!(unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX) }, 0);
    let e = format!(
        "{:#}",
        AqGit::start(&config, &state, &socket).await.err().unwrap()
    );
    assert!(e.contains("still owned by a live process"), "{e}");
    assert!(socket.exists());
    drop(held);
    std::fs::remove_file(&socket).unwrap();
    let live = UnixListener::bind(&socket).unwrap();
    let e = format!(
        "{:#}",
        AqGit::start(&config, &state, &socket).await.err().unwrap()
    );
    assert!(e.contains("a live aq-git still listens"), "{e}");
    drop(live);
    assert!(
        !dir.path().join("clear-fence.json").exists(),
        "the fence is kept"
    );
    std::fs::remove_file(state.join("git/incarnation.json")).unwrap();
    let exits = fake_aq_git(dir.path(), Some(3));
    let e = format!(
        "{:#}",
        AqGit::start(&exits, &state, &socket).await.err().unwrap()
    );
    assert!(
        e.contains("exited (exit status: 3) before it listened"),
        "{e}"
    );
}
/// D2 against the real aq-git: register, SIGKILL its process group (as `systemctl kill`
/// does), start again on the same state and register again.
#[tokio::test]
#[ignore = "requires AQ_GIT_TEST_BINARY (an aq-git build)"]
async fn real_aq_git_restarts_after_a_sigkill() {
    let binary = std::env::var_os("AQ_GIT_TEST_BINARY").expect("set AQ_GIT_TEST_BINARY");
    let dir = tempfile::tempdir().unwrap();
    let (state, socket) = (dir.path().join("current"), dir.path().join("git.sock"));
    std::fs::create_dir_all(state.join("git")).unwrap();
    let config = AqGitConfig {
        binary: binary.into(),
        git_executable: "/usr/bin/git".into(),
    };
    let mut git = AqGit::start(&config, &state, &socket).await.unwrap();
    register_e0(&git).await;
    let pgid = git.child.as_ref().unwrap().id().unwrap() as i32;
    // SAFETY: the test's own aq-git process group.
    unsafe { libc::kill(-pgid, libc::SIGKILL) };
    git.child.take().unwrap().wait().await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(socket.exists(), "the killed aq-git left its socket");
    let git = AqGit::start(&config, &state, &socket).await.unwrap();
    register_e0(&git).await;
    git.stop().await.unwrap();
    let git = AqGit::start(&config, &state, &socket).await.unwrap();
    register_e0(&git).await;
    git.stop().await.unwrap();
}
/// #51 review M3: a stopped or dropped aq-git takes its whole process group (the forked
/// engine holds the owner lock), not only its leader.
#[tokio::test]
async fn stop_and_drop_kill_the_whole_process_group() {
    for drop_it in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let (state, socket) = (dir.path().join("current"), dir.path().join("git.sock"));
        std::fs::create_dir_all(state.join("git")).unwrap();
        let config = fake_aq_git(dir.path(), None);
        let git = AqGit::start(&config, &state, &socket).await.unwrap();
        let engine: i32 = std::fs::read_to_string(dir.path().join("engine.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // SAFETY: probing a process the test started.
        assert_eq!(unsafe { libc::kill(engine, 0) }, 0, "the engine runs");
        if drop_it {
            drop(git);
        } else {
            git.stop().await.unwrap();
        }
        let gone = (0..200).any(|_| {
            std::thread::sleep(Duration::from_millis(10));
            // A killed child of the shell is a zombie until reaped: check its state.
            std::fs::read_to_string(format!("/proc/{engine}/stat")).map_or(true, |s| {
                s.rsplit_once(") ").is_some_and(|(_, t)| t.starts_with('Z'))
            })
        });
        assert!(
            gone,
            "the engine of a {} aq-git is gone",
            if drop_it { "dropped" } else { "stopped" }
        );
    }
}
/// #51 re-review: the group kill uses the leader pid taken at spawn, so it still fires
/// when the leader exited and was reaped before the stop (its engine still running).
#[tokio::test]
async fn the_group_kill_fires_after_the_leader_was_reaped() {
    let dir = tempfile::tempdir().unwrap();
    let (state, socket) = (dir.path().join("current"), dir.path().join("git.sock"));
    std::fs::create_dir_all(state.join("git")).unwrap();
    let config = fake_aq_git(dir.path(), None);
    let mut git = AqGit::start(&config, &state, &socket).await.unwrap();
    let engine: i32 = std::fs::read_to_string(dir.path().join("engine.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // The leader exits on its own and is reaped: its `Child::id` is gone.
    let child = git.child.as_mut().unwrap();
    child.start_kill().unwrap();
    child.wait().await.unwrap();
    assert!(child.id().is_none());
    git.stop().await.unwrap();
    let gone = (0..200).any(|_| {
        std::thread::sleep(Duration::from_millis(10));
        std::fs::read_to_string(format!("/proc/{engine}/stat")).map_or(true, |s| {
            s.rsplit_once(") ").is_some_and(|(_, t)| t.starts_with('Z'))
        })
    });
    assert!(gone, "the engine is gone after the stop");
}

/// Registers (or re-registers: aq-git answers `binding_conflict` for a repository its state
/// already holds) the repository the restart test keeps in its state.
async fn register_e0(git: &AqGit) {
    let id = Uuid::new_v4().to_string();
    let reply = git
        .exchange(&serde_json::json!({
            "protocol_version": 1, "request_id": id, "operation_id": id,
            "operation": "repo.register", "repository_id": Uuid::from_u128(9),
            "expected_binding_generation": 0,
            "params": {"binding": {"identity": "e0-development", "mode": "worker",
                "binding_generation": 1, "object_format": "sha1"}}}))
        .await
        .unwrap();
    assert!(
        reply["ok"] == true || reply["error"]["code"] == "binding_conflict",
        "{reply}"
    );
}
