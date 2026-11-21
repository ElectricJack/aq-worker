use super::*;
use aq_node_git::{Remote, test_support as central};

#[test]
fn installed_source_router_prepares_both_attempt_types() {
    let remote = central::Central::start();
    let config = tempfile::tempdir().unwrap();
    let node = Arc::new(central::config(config.path()).node(central::remote(&remote)));
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let export = synthetic();
            let manifest = export.manifest.clone();
            let mut legacy = Inputs::new(export).await;
            let git = GitPeer::new(legacy.root.clone(), manifest);
            let spec = legacy.spec().await;
            legacy.mode.store(1, Ordering::SeqCst);
            let hydrate = |spec, local| {
                legacy.pipeline().hydrate_source(
                    legacy.id,
                    legacy.preparation().clone(),
                    spec,
                    local,
                )
            };
            assert!(hydrate(spec, git.client.clone()).await.is_err());
            let saved = legacy.pipeline().snapshot().await.unwrap();
            let pending = saved.attempts[&legacy.id]
                .source
                .as_ref()
                .unwrap()
                .pending
                .clone()
                .unwrap();
            let mut local = git.client.clone();
            local.node = Some(node.clone());
            assert!(hydrate(legacy.spec().await, local.clone()).await.is_err());
            let saved = legacy.pipeline().snapshot().await.unwrap();
            assert_eq!(
                saved.attempts[&legacy.id]
                    .source
                    .as_ref()
                    .unwrap()
                    .pending
                    .as_ref()
                    .unwrap()
                    .operation_id,
                pending.operation_id
            );
            legacy.mode.store(0, Ordering::SeqCst);
            hydrate(legacy.spec().await, local).await.unwrap();
            let saved = legacy.pipeline().snapshot().await.unwrap();
            let source = saved.attempts[&legacy.id].source.as_ref().unwrap();
            assert!(source.complete() && source.operations.contains_key("import"));
            assert!(source.worktree.is_none());
            drop(legacy.pipeline.take());
            let mut capacity = limits();
            capacity.disk_bytes = 64 * 1024 * 1024 * 1024;
            let reopened =
                Journal::open(&legacy.root, identity().node_id, capacity.clone()).unwrap();
            assert!(reopened.recovery_required());
            assert!(
                reopened.state().attempts[&legacy.id]
                    .source
                    .as_ref()
                    .unwrap()
                    .complete()
            );

            let mut export = synthetic();
            let grant = remote.grant(
                &offer().offered.offer.manifest.bindings.task_id.to_string(),
                None,
            );
            export.manifest.source.repository_id = grant.repository_id;
            export.manifest.source.base.hex.clone_from(&grant.base);
            export.manifest.checkpoint = export.manifest.source.base.clone();
            let manifest = export.manifest.clone();
            let mut central = Inputs::new(export).await;
            let git = GitPeer::new(central.root.clone(), manifest);
            let all = central.all_inputs().await;
            let mut local = git.client.clone();
            local.node = Some(node);
            let hydrate = |spec, local| {
                central.pipeline().hydrate_source(
                    central.id,
                    central.preparation().clone(),
                    spec,
                    local,
                )
            };
            let mut foreign = grant.clone();
            foreign.task_ref = "refs/heads/aq/other-task".into();
            remote.grants.lock().unwrap().insert(central.id, foreign);
            let error = hydrate(central.spec().await, local.clone())
                .await
                .err()
                .unwrap();
            assert!(
                error
                    .to_string()
                    .contains("git grant differs from source plan")
            );
            assert_eq!(remote.requests("git-upload-pack"), 0);
            remote
                .grants
                .lock()
                .unwrap()
                .insert(central.id, grant.clone());
            let proof = hydrate(central.spec().await, local.clone()).await.unwrap();
            let path = proof.workspace().path.clone();
            assert!(path.starts_with(git.client.root()) && path.join("a.txt").is_file());
            let saved = central.pipeline().snapshot().await.unwrap();
            let source = saved.attempts[&central.id].source.as_ref().unwrap();
            assert_eq!((source.ranges, source.worktree.as_ref()), (0, Some(&path)));
            assert!(
                git.calls
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|r| r["operation"] == "capabilities")
            );
            remote.grants.lock().unwrap().remove(&central.id);
            let error = hydrate(central.spec().await, local).await.err().unwrap();
            assert!(
                error
                    .to_string()
                    .contains("recorded node worktree requires a git grant")
            );
            central
                .pipeline()
                .prepare_inputs(central.id, central.preparation().clone(), all, proof)
                .await
                .unwrap();
            drop(central.pipeline.take());
            let reopened = Journal::open(&central.root, identity().node_id, capacity).unwrap();
            assert!(reopened.recovery_required());
            let source = reopened.state().attempts[&central.id]
                .source
                .as_ref()
                .unwrap();
            assert_eq!(source.worktree.as_ref(), Some(&path));
            remote
                .grants
                .lock()
                .unwrap()
                .insert(central.id, grant.clone());
            let restarted = central::config(config.path()).node(central::remote(&remote));
            fs::write(path.join("result.txt"), "retained after restart\n").unwrap();
            let (_, pushed) = restarted
                .finish(path.clone(), "result".into())
                .await
                .unwrap();
            let aq_node_git::Pushed::Pushed { commit, task_ref } = pushed else {
                panic!("expected a push");
            };
            assert_eq!(task_ref, grant.task_ref);
            assert_eq!(remote.head(&task_ref), Some(commit));
            restarted.end(path.clone()).await.unwrap();
            assert!(!path.exists());
        });
}

#[test]
fn source_router_keeps_bundle_untouched_when_grant_lookup_is_refused() {
    let remote = central::Central::start();
    let config = tempfile::tempdir().unwrap();
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let export = synthetic();
            let manifest = export.manifest.clone();
            let h = Inputs::new(export).await;
            let git = GitPeer::new(h.root.clone(), manifest);
            let mut local = git.client.clone();
            local.node = Some(Arc::new(central::config(config.path()).node(Remote::new(
                &remote.url,
                "other-node",
                None,
            ))));
            let refused = h
                .pipeline()
                .hydrate_source(h.id, h.preparation().clone(), h.spec().await, local)
                .await
                .err()
                .unwrap();
            assert!(refused.to_string().contains("403"));
            let saved = h.pipeline().snapshot().await.unwrap();
            let source = saved.attempts[&h.id].source.as_ref().unwrap();
            assert!(source.manifest.is_none() && source.operations.is_empty());
            assert_eq!(source.ranges, 0);
            assert!(
                git.calls
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|r| r["operation"] == "capabilities")
            );
        });
}
