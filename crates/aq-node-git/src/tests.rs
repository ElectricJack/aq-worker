//! Against the local double of the server's git routes (`test_support`).
use crate::test_support::*;
use crate::*;
use std::sync::atomic::Ordering;

#[test]
fn first_attempt_clones_later_attempts_fetch_and_push_under_the_lease() {
    let central = Central::start();
    let state = tempfile::tempdir().unwrap();
    let node = node(&state.path().join("repositories"));
    let remote = remote(&central);
    let path = at(state.path(), 1);
    let first = node
        .start(
            &remote,
            &central.grant("t1", None),
            Uuid::from_u128(1),
            &path,
        )
        .unwrap();
    assert_eq!(first.synced, Synced::Cloned);
    assert_eq!(first.start, central.head("refs/heads/main").unwrap());
    std::fs::write(path.join("b.txt"), "two\n").unwrap();
    let Pushed::Pushed { task_ref, commit } = node.finish(&remote, &first, "attempt 1").unwrap()
    else {
        panic!("first push conflicted")
    };
    assert_eq!(task_ref, "refs/heads/aq/t1");
    assert_eq!(central.head("refs/heads/aq/t1").as_ref(), Some(&commit));
    // A handoff successor can start before the predecessor's local cleanup.
    // It reuses the repository and starts at the task branch's leased head.
    let repo = node.repository(Uuid::from_u128(7));
    std::fs::write(repo.join("kept"), "").unwrap();
    let returned = central.grant("t1", Some(commit.clone()));
    let path = at(state.path(), 2);
    let second = node
        .start(&remote, &returned, Uuid::from_u128(2), &path)
        .unwrap();
    node.end(&NodeGit::open(&first.path).unwrap()).unwrap();
    assert!(!first.path.exists() && !record(&first.path).exists());
    assert_eq!(git(&path, &["rev-parse", "HEAD"]), commit);
    assert_eq!(second.synced, Synced::Fetched);
    assert!(repo.join("kept").exists());
    assert_eq!(second.start, commit);
    assert_eq!(
        std::fs::read_to_string(path.join("b.txt")).unwrap(),
        "two\n"
    );
    std::fs::remove_file(path.join("a.txt")).unwrap();
    let Pushed::Pushed { commit: next, .. } = node.finish(&remote, &second, "attempt 2").unwrap()
    else {
        panic!("second push conflicted")
    };
    assert_eq!(
        git(&central.remote(), &["rev-parse", &format!("{next}^")]),
        commit
    );
    node.end(&second).unwrap();

    // Every request carried the node credential and named its attempt.
    let seen = central.seen.lock().unwrap().clone();
    let attempts = [Uuid::from_u128(1), Uuid::from_u128(2)];
    assert!(
        seen.iter().all(|l| attempts
            .iter()
            .any(|a| l.ends_with(&format!("Bearer node-secret {a}")))),
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
                // Each slot is its own process with its own NodeGit on the node's repositories.
                let node = node(&root.join("repositories"));
                let path = at(&root.join(format!("slot{n}")), 10 + n);
                let wt = node
                    .start(&remote, &grant, Uuid::from_u128(10 + n), &path)
                    .unwrap();
                std::fs::write(path.join(format!("slot{n}.txt")), "x").unwrap();
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
    let node = node(&state.path().join("repositories"));
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
        git(repo, &["for-each-ref", "--format=%(refname)", "refs/heads"]),
        ""
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
    let path = at(state.path(), 3);
    let wt = node
        .start(
            &remote,
            &central.grant("r1", None),
            Uuid::from_u128(3),
            &path,
        )
        .unwrap();
    std::fs::write(path.join("c.txt"), "three\n").unwrap();
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
        let path = at(state.path(), 20 + n as u128);
        let wt = node
            .start(&remote, &grant, Uuid::from_u128(20 + n as u128), &path)
            .unwrap();
        std::fs::write(path.join("d.txt"), "four\n").unwrap();
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
    grant.validate().unwrap();
    for bad in [
        "refs/heads/main",
        "refs/heads/aq/",
        "refs/heads/aq/a/../b",
        "refs/heads/aq/-x y",
    ] {
        grant.task_ref = bad.into();
        assert!(grant.validate().is_err(), "{bad}");
    }
    let mut grant = central.grant("v1", None);
    grant.endpoint = "/v1/git/other".into();
    assert!(grant.validate().is_err());
    let mut grant = central.grant("v1", None);
    grant.base = "--upload-pack=touch".into();
    assert!(grant.validate().is_err());
}

fn creator() -> GitAuthor {
    GitAuthor {
        name: "Zoë Creator".into(),
        email: "Zoë+AQ@example.invalid".into(),
    }
}

fn identities(dir: &Path, commit: &str) -> String {
    git(
        dir,
        &["show", "--no-patch", "--format=%an <%ae>|%cn <%ce>", commit],
    )
}

/// The async face: the grant comes from the server, and a later call (a later
/// boot) finishes and ends the worktree from its record alone.
#[test]
fn a_node_reads_its_grant_then_finishes_and_ends_from_the_worktree_record() {
    let central = Central::start();
    let state = tempfile::tempdir().unwrap();
    let attempt = Uuid::from_u128(40);
    central
        .grants
        .lock()
        .unwrap()
        .insert(attempt, central.grant("n1", None));
    let node = config(state.path()).node(remote(&central));
    let path = at(state.path(), 40);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let grant = runtime
        .block_on(node.grant(attempt, Duration::from_secs(5)))
        .unwrap()
        .unwrap();
    let wt = runtime
        .block_on(node.start(attempt, path.clone(), grant))
        .unwrap();
    assert_eq!(wt.grant.task_ref, "refs/heads/aq/n1");
    git(&path, &["commit", "--allow-empty", "-m", "pre-authored"]);
    let preauthored = git(&path, &["rev-parse", "HEAD"]);
    let verify =
        |id, head: &str| runtime.block_on(node.verify_checkout(id, path.clone(), head.to_owned()));
    verify(attempt, &preauthored).unwrap();
    assert!(verify(Uuid::new_v4(), &preauthored).is_err());
    assert!(verify(attempt, &wt.start).is_err());
    let preauthored_identity = identities(&path, &preauthored);
    std::fs::write(path.join("e.txt"), "five\n").unwrap();
    assert!(verify(attempt, &preauthored).is_err(), "untracked file");
    let again = config(state.path()).node(remote(&central));
    let (_, pushed) = runtime
        .block_on(again.finish_as(path.clone(), "done".into(), Some(creator())))
        .unwrap();
    let Pushed::Pushed { commit, .. } = pushed else {
        panic!("conflict")
    };
    assert_eq!(central.head("refs/heads/aq/n1"), Some(commit.clone()));
    let expected = format!(
        "{} <{}>|{} <{}>",
        creator().name,
        creator().email,
        config(state.path()).author_name,
        config(state.path()).author_email
    );
    assert_eq!(identities(&path, &commit), expected);
    assert_eq!(git(&path, &["rev-parse", "HEAD^"]), preauthored);
    assert_eq!(identities(&path, &preauthored), preauthored_identity);
    let (_, retry) = runtime
        .block_on(again.finish_as(path.clone(), "done".into(), Some(creator())))
        .unwrap();
    assert!(matches!(retry, Pushed::Pushed { commit: oid, .. } if oid == commit));
    runtime.block_on(again.end(path.clone())).unwrap();
    assert!(!path.exists() && !record(&path).exists());
    runtime.block_on(again.end(path.clone())).unwrap();
    let absent = runtime.block_on(node.grant(Uuid::from_u128(41), Duration::from_secs(5)));
    assert_eq!(absent.unwrap(), None);
}

#[tokio::test]
async fn only_not_found_means_the_attempt_has_no_grant() {
    use axum::{Router, http::StatusCode, routing::get};

    let app = Router::new().route(
        "/v1/git/grants/{status}",
        get(
            async |axum::extract::Path(status): axum::extract::Path<Uuid>| {
                (StatusCode::from_u16(status.as_u128() as u16).unwrap(), "{}")
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async { axum::serve(listener, app).await.unwrap() });
    let remote = Remote::new(&origin, "node-secret", None);
    let lookup = |code| remote.grant(Uuid::from_u128(code), Duration::from_secs(2));
    assert_eq!(lookup(404).await.unwrap(), None);
    for status in [302, 401, 403, 429, 500, 503] {
        assert!(
            lookup(status)
                .await
                .unwrap_err()
                .to_string()
                .contains(&status.to_string())
        );
    }
    assert!(
        lookup(200)
            .await
            .unwrap_err()
            .to_string()
            .contains("git grant shape")
    );
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
    assert!(
        lookup(404)
            .await
            .unwrap_err()
            .to_string()
            .contains("git grant request failed")
    );
}

/// A returned task: its branch has `b.txt` (and, when `clash`, its own `a.txt`);
/// main has moved on with a new `a.txt` since. The grant's base stays the
/// replaced attempt's, so a merge of `base` would bring in nothing.
fn returned_task(central: &Central, task: &str, clash: bool, update: bool) -> Grant {
    let base = central.head("refs/heads/main").unwrap();
    let task_ref = format!("refs/heads/aq/{task}");
    let mut head = central.commit(&task_ref, &base, "b.txt", "branch\n");
    if clash {
        head = central.commit(&task_ref, &head, "a.txt", "branch\n");
    }
    central.commit("refs/heads/main", &base, "a.txt", "main\n");
    let mut grant = central.grant(task, Some(head));
    (grant.base, grant.update_from_main) = (base, update);
    grant
}

#[test]
fn a_returned_task_starts_from_its_branch_head_without_main() {
    let central = Central::start();
    let state = tempfile::tempdir().unwrap();
    let (node, remote) = (node(state.path()), remote(&central));
    let grant = returned_task(&central, "w1", false, false);
    let path = at(state.path(), 30);
    let wt = node
        .start(&remote, &grant, Uuid::from_u128(30), &path)
        .unwrap();
    assert_eq!(
        git(&path, &["rev-parse", "HEAD"]),
        grant.lease.clone().unwrap()
    );
    assert_eq!(
        std::fs::read_to_string(path.join("a.txt")).unwrap(),
        "one\n"
    );
    assert!(wt.conflicts.is_empty() && conflict_note(&path).is_none());
    node.end(&wt).unwrap();
}

#[test]
fn update_from_main_commits_a_clean_merge_of_the_main_head() {
    let central = Central::start();
    let state = tempfile::tempdir().unwrap();
    let (node, remote) = (node(state.path()), remote(&central));
    let grant = returned_task(&central, "w2", false, true);
    let path = at(state.path(), 31);
    let author = creator();
    let wt = node
        .start_as(
            &remote,
            &grant,
            Uuid::from_u128(31),
            &path,
            Some(author.identity()),
        )
        .unwrap();
    let worker = config(state.path());
    assert_eq!(
        identities(&path, "HEAD"),
        format!(
            "{} <{}>|{} <{}>",
            author.name, author.email, worker.author_name, worker.author_email
        )
    );
    let parents = git(&path, &["rev-list", "--parents", "-n1", "HEAD"]);
    let (lease, main) = (
        grant.lease.clone().unwrap(),
        central.head("refs/heads/main").unwrap(),
    );
    assert_eq!(
        parents.split(' ').skip(1).collect::<Vec<_>>(),
        [lease.as_str(), &main]
    );
    assert!(wt.conflicts.is_empty());
    let note = conflict_note(&path).unwrap();
    assert!(note.contains("The merge was clean") && note.contains("AQ handles Git publication"));
    assert_eq!(
        std::fs::read_to_string(path.join("a.txt")).unwrap(),
        "main\n"
    );
    assert_eq!(
        std::fs::read_to_string(path.join("b.txt")).unwrap(),
        "branch\n"
    );
    assert_eq!(git(&path, &["status", "--porcelain"]), "");
    let Pushed::Pushed { commit, .. } = node.finish(&remote, &wt, "repair").unwrap() else {
        panic!("conflict")
    };
    assert_eq!(central.head("refs/heads/aq/w2"), Some(commit));
    node.end(&wt).unwrap();
}

#[test]
fn update_from_main_leaves_conflict_markers_committed_and_lists_the_paths() {
    let central = Central::start();
    let state = tempfile::tempdir().unwrap();
    let (node, remote) = (node(state.path()), remote(&central));
    let grant = returned_task(&central, "w3", true, true);
    let path = at(state.path(), 32);
    let wt = node
        .start(&remote, &grant, Uuid::from_u128(32), &path)
        .unwrap();
    assert_eq!(wt.conflicts, ["a.txt"]);
    let text = std::fs::read_to_string(path.join("a.txt")).unwrap();
    assert!(
        text.contains("<<<<<<<") && text.contains(">>>>>>>"),
        "{text}"
    );
    let parents = git(&path, &["rev-list", "--parents", "-n1", "HEAD"]);
    assert_eq!(parents.split(' ').count(), 3, "the merge is committed");
    assert_eq!(git(&path, &["status", "--porcelain"]), "");
    assert!(conflict_note(&path).unwrap().contains("a.txt"));
    node.end(&wt).unwrap();
}

#[cfg(feature = "environment-job")]
#[test]
fn an_environment_job_fetches_its_commit_into_the_existing_repository_with_the_job_header() {
    let central = Central::start();
    let state = tempfile::tempdir().unwrap();
    let node = node(&state.path().join("repositories"));
    let remote = remote(&central);
    let id = Uuid::from_u128(7);
    let attempt = at(state.path(), 1);
    let first = node
        .start(
            &remote,
            &central.grant("t1", None),
            Uuid::from_u128(1),
            &attempt,
        )
        .unwrap();
    node.end(&first).unwrap();
    let repo = node.repository(id);
    std::fs::write(repo.join("kept"), "").unwrap();
    // Pushed after the node's last fetch.
    let main = central.head("refs/heads/main").unwrap();
    let commit = central.commit("refs/heads/aq/t2", &main, "b.txt", "two\n");
    let job = Uuid::from_u128(9);
    let path = state.path().join("jobs").join(job.to_string());

    let synced = node.checkout(&remote, job, id, &commit, &path).unwrap();
    assert_eq!(synced, Synced::Fetched);
    assert!(repo.join("kept").exists(), "never re-cloned");
    assert_eq!(git(&path, &["rev-parse", "HEAD"]), commit);
    assert_eq!(
        std::fs::read_to_string(path.join("b.txt")).unwrap(),
        "two\n"
    );
    let seen = central.seen.lock().unwrap().clone();
    let tagged: Vec<_> = seen
        .iter()
        .filter(|l| l.contains(&format!("job={job}")))
        .collect();
    assert!(!tagged.is_empty(), "{seen:?}");
    // The job header alone, no attempt, and only fetches.
    assert!(
        tagged
            .iter()
            .all(|l| l.contains("Bearer node-secret  job=") && !l.contains("receive-pack")),
        "{seen:?}"
    );
    node.discard(id, &path).unwrap();
    assert!(!path.exists());
    assert!(git(&repo, &["worktree", "list"]).lines().count() == 1);

    // Anything but a full object id never reaches git.
    let before = central.seen.lock().unwrap().len();
    assert!(node.checkout(&remote, job, id, "HEAD", &path).is_err());
    assert_eq!(central.seen.lock().unwrap().len(), before);
    // A refused fetch (claim lost) adds no worktree.
    central.refuse.store(true, Ordering::SeqCst);
    assert!(node.checkout(&remote, job, id, &commit, &path).is_err());
    assert!(!path.exists());

    // A worktree whose directory a crash removed is pruned from every repository.
    central.refuse.store(false, Ordering::SeqCst);
    node.checkout(&remote, job, id, &commit, &path).unwrap();
    std::fs::remove_dir_all(&path).unwrap();
    assert_eq!(git(&repo, &["worktree", "list"]).lines().count(), 2);
    node.prune().unwrap();
    assert_eq!(git(&repo, &["worktree", "list"]).lines().count(), 1);
}

/// Spec 0017 §6.2: a checkpoint commits the agent's live tree the way the
/// result is committed and force-pushes it to the attempt's try ref. It never
/// moves the task ref or touches the worktree, so the final push still holds
/// its lease.
#[cfg(feature = "environment-try")]
#[test]
fn the_final_result_push_still_succeeds_after_a_checkpoint() {
    let central = Central::start();
    let state = tempfile::tempdir().unwrap();
    let node = node(&state.path().join("repositories"));
    let remote = remote(&central);
    let (attempt, path) = (Uuid::from_u128(50), at(state.path(), 50));
    let wt = node
        .start(&remote, &central.grant("c1", None), attempt, &path)
        .unwrap();
    let head = git(&path, &["rev-parse", "HEAD"]);
    let author = creator();
    let who = Some((author.name.as_str(), author.email.as_str()));
    // The agent's live tree, as the copy-back would write it.
    let live = |text: &'static str| {
        move |tree: &Path| -> Result<()> {
            std::fs::write(tree.join("a.txt"), text)?;
            std::fs::write(tree.join("b.txt"), "two\n")?;
            std::fs::write(tree.join(".gitignore"), "*.log\n")?;
            std::fs::write(tree.join("x.log"), "noise\n")?;
            Ok(())
        }
    };
    let try_ref = format!("refs/heads/aq-try/{attempt}");
    assert_eq!(crate::try_ref(attempt), try_ref);

    let first = node
        .checkpoint(&remote, &wt, "AQ attempt", who, live("first\n"))
        .unwrap();
    assert_eq!(central.head(&try_ref), Some(first.clone()));
    let at_remote = |args: &[&str]| git(&central.remote(), args);
    assert_eq!(at_remote(&["rev-parse", &format!("{first}^")]), head);
    assert_eq!(at_remote(&["show", &format!("{first}:a.txt")]), "first");
    assert!(!at_remote(&["ls-tree", "--name-only", &first]).contains("x.log"));
    let expected = format!(
        "{} <{}>|AQ node <node@aq.invalid>",
        author.name, author.email
    );
    assert_eq!(identities(&central.remote(), &first), expected);
    // The task ref, the worktree, its index and HEAD are untouched.
    assert_eq!(central.head("refs/heads/aq/c1"), None);
    assert_eq!(git(&path, &["rev-parse", "HEAD"]), head);
    assert_eq!(git(&path, &["status", "--porcelain"]), "");
    assert_eq!(
        std::fs::read_to_string(path.join("a.txt")).unwrap(),
        "one\n"
    );
    let worktrees = || git(&wt.repository, &["worktree", "list"]).lines().count();
    assert_eq!(worktrees(), 2, "the checkpoint's own worktree is removed");

    // The next try force-updates the try ref; a failed fill pushes nothing.
    let second = node
        .checkpoint(&remote, &wt, "AQ attempt", who, live("second\n"))
        .unwrap();
    assert_ne!(second, first);
    assert_eq!(central.head(&try_ref), Some(second.clone()));
    let failed = |_: &Path| -> Result<()> { anyhow::bail!("copy refused") };
    assert!(node.checkpoint(&remote, &wt, "x", who, failed).is_err());
    assert_eq!(central.head(&try_ref), Some(second.clone()));
    assert_eq!(worktrees(), 2);

    // The copy-back writes the same tree: the result is the same commit
    // content on the attempt's HEAD, pushed under the original lease.
    live("second\n")(&path).unwrap();
    let Pushed::Pushed { task_ref, commit } =
        node.finish_as(&remote, &wt, "AQ attempt", who).unwrap()
    else {
        panic!("the checkpoint moved the task ref")
    };
    assert_eq!(task_ref, "refs/heads/aq/c1");
    assert_eq!(central.head(&task_ref), Some(commit.clone()));
    assert_eq!(at_remote(&["rev-parse", &format!("{commit}^")]), head);
    let tree = |c: &str| at_remote(&["rev-parse", &format!("{c}^{{tree}}")]);
    assert_eq!(tree(&commit), tree(&second));
    node.end(&wt).unwrap();
}

#[test]
fn sibling_grants_checkout_exact_revisions_and_replay_only_writable_outputs() {
    use axum::{
        Json, Router,
        http::HeaderMap,
        routing::{get, put},
    };
    use serde_json::{Value, json};
    use std::sync::{Arc, Mutex};
    use workspaces::{Access, Project, Workspaces, checkout_path, project_endpoint};

    let plan = Arc::new(Mutex::new(Workspaces::default()));
    let receipts = Arc::new(Mutex::new(Vec::<Value>::new()));
    let grants = plan.clone();
    let received = receipts.clone();
    let router = Router::new()
        .route(
            "/v1/git/grants/{id}/workspaces",
            get(move |headers: HeaderMap| async move {
                assert_eq!(headers["authorization"], "Bearer node-secret");
                Json(grants.lock().unwrap().clone())
            }),
        )
        .route(
            "/v1/git/grants/{id}/workspaces/result",
            put(
                move |headers: HeaderMap, Json(value): Json<Value>| async move {
                    assert_eq!(headers["authorization"], "Bearer node-secret");
                    received.lock().unwrap().push(value);
                    axum::http::StatusCode::NO_CONTENT
                },
            ),
        );
    let central = Central::start_with(router);
    let base = central.head("refs/heads/main").unwrap();
    let (read, write, unchanged) = (
        Uuid::from_u128(12),
        Uuid::from_u128(13),
        Uuid::from_u128(14),
    );
    let project = |id, access, name| {
        let mut grant = central.grant(name, None);
        grant.repository_id = id;
        grant.endpoint = project_endpoint(id, id);
        Project {
            project_id: id,
            project_revision: 5,
            binding_generation: 2,
            access,
            checkout_head: grant.base.clone(),
            git: grant,
        }
    };
    plan.lock().unwrap().workspaces = vec![
        project(read, Access::Read, "source"),
        project(write, Access::Write, "output"),
        project(unchanged, Access::Write, "unchanged"),
    ];
    let state = tempfile::tempdir().unwrap();
    let node = config(state.path()).node(remote(&central));
    let primary = state.path().join("primary");
    let attempt = Uuid::from_u128(77);
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        node.start_siblings(attempt, &primary, Duration::from_secs(10), true).await.unwrap();
        for id in [read, write, unchanged] {
            let path = checkout_path(&primary, id);
            assert_eq!(git(&path, &["rev-parse", "HEAD"]), base);
        }
        std::fs::write(checkout_path(&primary, read).join("a.txt"), "cannot publish").unwrap();
        std::fs::write(checkout_path(&primary, write).join("a.txt"), "write-only update").unwrap();
        node.finish_siblings(attempt, &primary, None).await.unwrap();
        let head = central.head("refs/heads/aq/output").unwrap();
        assert!(central.head("refs/heads/aq/source").is_none());
        assert!(central.head("refs/heads/aq/unchanged").is_none());
        std::fs::write(checkout_path(&primary, write).join("a.txt"), "late edit").unwrap();
        node.finish_siblings(attempt, &primary, None).await.unwrap();
        assert_eq!(central.head("refs/heads/aq/output"), Some(head.clone()));
        let payload = json!({"workspaces":[
            {"project_id":write,"repository_id":write,"result":{"status":"pushed","head":head}},
            {"project_id":unchanged,"repository_id":unchanged,"result":{"status":"unchanged","head":base}}
        ]});
        assert_eq!(*receipts.lock().unwrap(), [payload.clone(), payload]);
        node.end_siblings(&primary).await.unwrap();
        assert_eq!(workspaces::load(&primary).unwrap(), Workspaces::default());
        let head = central.head("refs/heads/aq/output").unwrap();
        let mut resumed_grant = project(write, Access::Write, "output");
        resumed_grant.git.lease = Some(head.clone());
        resumed_grant.checkout_head = head.clone();
        plan.lock().unwrap().workspaces = vec![resumed_grant];
        let successor = Uuid::from_u128(78);
        node.start_siblings(successor, &primary, Duration::from_secs(10), true).await.unwrap();
        let resumed = checkout_path(&primary, write);
        assert_eq!(git(&resumed, &["rev-parse", "HEAD"]), head);
        assert_eq!(std::fs::read(resumed.join("a.txt")).unwrap(), b"write-only update");
        node.finish_siblings(successor, &primary, None).await.unwrap();
        assert_eq!(receipts.lock().unwrap().last().unwrap(), &json!({"workspaces":[{
            "project_id":write,"repository_id":write,"result":{"status":"unchanged","head":head}
        }]}));
        node.end_siblings(&primary).await.unwrap();
    });
}

#[test]
fn old_git_wildcard_400_is_absence_but_other_400s_are_refused() {
    use axum::{Router, extract::Path, http::StatusCode, routing::get};
    let old = Router::new().route(
        "/v1/git/grants/{attempt}/workspaces",
        get(|| async {
            (
                StatusCode::BAD_REQUEST,
                "Invalid URL: Cannot parse `repository` with value `grants`: UUID parsing failed: invalid character",
            )
        }),
    );
    let new = Router::new().route(
        "/v1/git/grants/{attempt}/workspaces",
        get(|Path(_): Path<Uuid>| async { (StatusCode::BAD_REQUEST, "invalid sealed grant") }),
    );
    let old = Central::start_with(old);
    let new = Central::start_with(new);
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let attempt = Uuid::from_u128(90);
            let timeout = Duration::from_secs(10);
            assert_eq!(
                remote(&old).workspaces(attempt, timeout).await.unwrap(),
                workspaces::Workspaces::default()
            );
            assert!(remote(&new).workspaces(attempt, timeout).await.is_err());
        });
}

#[test]
fn a_bundle_home_supports_sealed_projects_and_service_only_grants() {
    use axum::{Json, Router, routing::get};
    use std::sync::{Arc, Mutex};
    use workspaces::{Access, Project, Service, Workspaces};

    let plan = Arc::new(Mutex::new(Workspaces::default()));
    let grants = plan.clone();
    let central = Central::start_with(Router::new().route(
        "/v1/git/grants/{attempt}/workspaces",
        get(move || async move { Json(grants.lock().unwrap().clone()) }),
    ));
    let state = tempfile::tempdir().unwrap();
    let node = config(state.path()).node(remote(&central));
    let attempt = Uuid::from_u128(81);
    let timeout = Duration::from_secs(10);
    let primary = state.path().join("archive-home");
    std::fs::create_dir(&primary).unwrap();
    std::fs::write(primary.join("original"), "bundle source").unwrap();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            assert!(node.grant(attempt, timeout).await.unwrap().is_none());
            assert!(
                node.start_siblings(attempt, &primary, timeout, true)
                    .await
                    .is_err()
            );
            node.start_siblings(attempt, &primary, timeout, false)
                .await
                .unwrap();

            plan.lock().unwrap().services.push(Service::Postgres);
            assert!(
                node.start_siblings(attempt, &primary, timeout, false)
                    .await
                    .is_err()
            );
            node.start_siblings(attempt, &primary, timeout, true)
                .await
                .unwrap();
            assert_eq!(
                workspaces::load(&primary).unwrap().services,
                [Service::Postgres]
            );
            node.end_siblings(&primary).await.unwrap();
            assert!(primary.join("original").is_file());

            let project = Uuid::from_u128(19);
            let mut git = central.grant("related", None);
            git.endpoint = workspaces::project_endpoint(project, git.repository_id);
            let checkout_head = git.base.clone();
            *plan.lock().unwrap() = Workspaces {
                workspaces: vec![Project {
                    project_id: project,
                    project_revision: 1,
                    binding_generation: 1,
                    access: Access::Read,
                    checkout_head: checkout_head.clone(),
                    git,
                }],
                services: vec![],
            };
            assert!(node.grant(attempt, timeout).await.unwrap().is_none());
            node.start_siblings(attempt, &primary, timeout, true)
                .await
                .unwrap();
            assert_eq!(
                test_support::git(
                    &workspaces::checkout_path(&primary, project),
                    &["rev-parse", "HEAD"]
                ),
                checkout_head,
            );
            node.end_siblings(&primary).await.unwrap();
            assert_eq!(
                std::fs::read(primary.join("original")).unwrap(),
                b"bundle source"
            );
        });
}

#[test]
fn sibling_hydrates_retained_commit_absent_from_advertised_branches() {
    use axum::{Router, http::HeaderMap, routing::get};
    use workspaces::project_endpoint;

    let state = tempfile::tempdir().unwrap();
    let source = state.path().join("source");
    std::fs::create_dir(&source).unwrap();
    git(&source, &["init", "-b", "retained"]);
    std::fs::write(source.join("proof.txt"), "retained sibling contents").unwrap();
    git(&source, &["add", "proof.txt"]);
    git(
        &source,
        &[
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-m",
            "retained",
        ],
    );
    let oid = git(&source, &["rev-parse", "HEAD"]);
    let file = state.path().join("retained.bundle");
    git(
        &source,
        &["bundle", "create", file.to_str().unwrap(), "retained"],
    );
    let bytes = std::fs::read(file).unwrap();
    let attempt = Uuid::from_u128(25);
    let project = Uuid::from_u128(55);
    let central = Central::start_with(Router::new().route(
        "/v1/git/workspaces/{project}/{repo}/aq-source-bundle",
        get(move |headers: HeaderMap| {
            let bytes = bytes.clone();
            async move {
                assert_eq!(headers["authorization"], "Bearer node-secret");
                assert_eq!(headers["aq-attempt"], attempt.to_string());
                use sha2::{Digest, Sha256};
                let digest = format!("{:x}", Sha256::digest(&bytes));
                (
                    [
                        ("content-type", "application/x-git-bundle".into()),
                        ("aq-workspace-bundle-sha256", digest),
                        ("aq-workspace-bundle-size", bytes.len().to_string()),
                    ],
                    bytes,
                )
            }
        }),
    ));
    let mut grant = central.grant("retained", None);
    assert_ne!(grant.base, oid);
    grant.base = oid.clone();
    grant.repository_id = project;
    grant.endpoint = project_endpoint(project, project);
    let node = node(&state.path().join("repositories"));
    let remote = remote(&central);
    let checkout = state.path().join("checkout");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let checked = checkout.clone();
    runtime.block_on(async {
        tokio::task::spawn_blocking(move || node.start(&remote, &grant, attempt, &checked))
            .await
            .unwrap()
            .unwrap()
    });
    assert_eq!(git(&checkout, &["rev-parse", "HEAD"]), oid);
    assert_eq!(
        std::fs::read_to_string(checkout.join("proof.txt")).unwrap(),
        "retained sibling contents"
    );
}
