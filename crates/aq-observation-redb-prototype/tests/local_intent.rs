#[allow(dead_code)]
mod support;
use anyhow::{Result, ensure};
use aq_observation_admission::{self as a, Owner, Receipt, Reply, Request, Status};
use aq_observation_redb_prototype::{
    AuthenticatedOwnerPeer, Limits, LocalIntent, LocalStage, Store,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};
use uuid::Uuid;
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct Peer {
    owner: Option<Owner>,
    receipts: BTreeMap<Uuid, Receipt>,
    boots: BTreeSet<Uuid>,
    boot: u128,
    lose: bool,
    before: bool,
    calls: Vec<Request>,
    #[serde(skip)]
    file: Option<std::path::PathBuf>,
}
impl Peer {
    fn persist(&self) -> Result<()> {
        if let Some(path) = &self.file {
            let tmp = path.with_extension("pending");
            let mut f = std::fs::File::create(&tmp)?;
            use std::io::Write;
            f.write_all(&serde_json::to_vec(self)?)?;
            f.sync_all()?;
            std::fs::rename(&tmp, path)?;
            std::fs::File::open(path.parent().unwrap())?.sync_all()?;
        }
        Ok(())
    }
    fn load(path: &Path) -> Self {
        let mut p: Self = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        p.file = Some(path.into());
        p
    }
    fn new() -> Self {
        Self {
            boot: 6,
            ..Default::default()
        }
    }
}
impl AuthenticatedOwnerPeer for Peer {
    fn mutate(&mut self, r: &Request) -> Result<Reply> {
        self.calls.push(r.clone());
        if self.before {
            self.before = false;
            anyhow::bail!("synthetic before-commit loss")
        }
        let new = match r.operation {
            a::Operation::AdvanceBoot { new_boot_id, .. } => self.boots.contains(&new_boot_id),
            _ => false,
        };
        let access = a::Access {
            node_id: Uuid::from_u128(5),
            boot_id: Uuid::from_u128(self.boot),
            accepted_through: 0,
            max_pending_distance: a::MAX_PENDING_DISTANCE,
            new_boot_previously_owned: new,
        };
        let t = a::apply(
            self.owner.as_ref(),
            self.receipts.get(&r.operation_id),
            r,
            &access,
            Some(Uuid::from_u128(400)),
        )?;
        self.owner = Some(t.reply.current_owner.clone());
        self.boots.insert(t.reply.current_owner.current_boot_id);
        self.receipts
            .insert(r.operation_id, t.reply.receipt.clone());
        let lose = self.lose;
        self.lose = false;
        self.persist()?;
        if lose {
            anyhow::bail!("synthetic after-commit loss")
        };
        Ok(t.reply)
    }
    fn status(&mut self, node: Uuid) -> Result<Status> {
        Ok(Status {
            version: 1,
            node_id: node,
            admitted_node_boot_id: Uuid::from_u128(self.boot),
            owner: self.owner.clone(),
            accepted_through: 0,
        })
    }
}
fn prepared(path: &Path, peer: &mut Peer) -> LocalIntent {
    let mut l = LocalIntent::create(
        path,
        Limits::fixture(),
        Uuid::from_u128(401),
        Uuid::from_u128(402),
    )
    .unwrap();
    l.prepare(peer).unwrap();
    l
}
fn active(path: &Path, peer: &mut Peer) -> LocalIntent {
    let mut l = prepared(path, peer);
    l.create_store(peer).unwrap();
    l.activate(peer, Uuid::from_u128(403)).unwrap();
    l
}
#[test]
fn real_prepare_create_activate_exact_marker_and_fresh_resume_gate() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("owner");
    let mut peer = Peer::new();
    let mut l = prepared(&p, &mut peer);
    assert_eq!(l.stage(), LocalStage::Prepared);
    assert!(l.with_active(|_| Ok(())).is_err());
    l.create_store(&mut peer).unwrap();
    assert_eq!(l.stage(), LocalStage::StoreCreated);
    assert!(l.with_active(|s| s.attach(support::source(8))).is_err());
    assert!(Store::open(&p.join("store"), Limits::fixture()).is_err());
    l.activate(&mut peer, Uuid::from_u128(403)).unwrap();
    l.with_active(|s| s.attach(support::source(8))).unwrap();
    drop(l);
    let mut l = LocalIntent::open(&p, Uuid::from_u128(6)).unwrap();
    assert!(l.with_active(|_| Ok(())).is_err());
    l.resume(&mut peer).unwrap();
    l.with_active(|s| {
        ensure!(s.source(Uuid::from_u128(8))?.cursor == 0, "cursor");
        Ok(())
    })
    .unwrap();
}
#[test]
fn lost_prepare_and_activation_replies_recover_only_existing_store() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("owner");
    let mut peer = Peer::new();
    let mut l = LocalIntent::create(
        &p,
        Limits::fixture(),
        Uuid::from_u128(401),
        Uuid::from_u128(402),
    )
    .unwrap();
    peer.lose = true;
    assert!(l.prepare(&mut peer).is_err());
    assert_eq!(l.stage(), LocalStage::PreparePending);
    drop(l);
    let mut l = LocalIntent::open(&p, Uuid::from_u128(6)).unwrap();
    l.prepare(&mut peer).unwrap();
    l.create_store(&mut peer).unwrap();
    peer.lose = true;
    assert!(l.activate(&mut peer, Uuid::from_u128(403)).is_err());
    drop(l);
    let mut l = LocalIntent::open(&p, Uuid::from_u128(6)).unwrap();
    assert_eq!(l.stage(), LocalStage::ActivationPending);
    assert!(l.with_active(|_| Ok(())).is_err());
    l.resume(&mut peer).unwrap();
    assert_eq!(l.stage(), LocalStage::Active);
    drop(l);
    std::fs::remove_file(p.join("store/store.redb")).unwrap();
    assert!(LocalIntent::open(&p, Uuid::from_u128(6)).is_err());
    assert!(!p.join("store/store.redb").exists());
    assert_eq!(peer.owner.as_ref().unwrap().revision, 2);
}
#[test]
fn prepare_receipt_replay_after_remote_activation_never_creates_at_zero_cursor() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("owner");
    let mut peer = Peer::new();
    let mut l = prepared(&p, &mut peer);
    let marker = peer.owner.as_ref().unwrap().marker.clone();
    peer.mutate(&Request {
        version: 1,
        operation_id: Uuid::from_u128(999),
        node_id: Uuid::from_u128(5),
        operation: a::Operation::Activate { marker },
    })
    .unwrap();
    assert!(l.create_store(&mut peer).is_err());
    assert!(!p.join("store").exists());
    assert_eq!(l.stage(), LocalStage::Prepared);
}
#[test]
fn activation_operation_immutable_and_changed_binding_status_refused() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("owner");
    let mut peer = Peer::new();
    let mut l = prepared(&p, &mut peer);
    l.create_store(&mut peer).unwrap();
    peer.before = true;
    assert!(l.activate(&mut peer, Uuid::from_u128(403)).is_err());
    assert!(l.activate(&mut peer, Uuid::from_u128(404)).is_err());
    l.activate(&mut peer, Uuid::from_u128(403)).unwrap();
    drop(l);
    let mut l = LocalIntent::open(&p, Uuid::from_u128(6)).unwrap();
    peer.owner.as_mut().unwrap().marker.store_id = Uuid::from_u128(999);
    assert!(l.resume(&mut peer).is_err());
    assert!(l.with_active(|_| Ok(())).is_err());
}
#[test]
fn remote_boot_commit_loss_replays_exact_request_and_fences_old_sources_once() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("owner");
    let mut peer = Peer::new();
    let l = active(&p, &mut peer);
    l.with_active(|s| {
        s.attach(support::source(8))?;
        support::append(s, 1);
        Ok(())
    })
    .unwrap();
    drop(l);
    peer.boot = 66;
    let mut l = LocalIntent::open(&p, Uuid::from_u128(66)).unwrap();
    assert!(l.resume(&mut peer).is_err());
    peer.lose = true;
    assert!(
        l.advance_boot(&mut peer, Uuid::from_u128(404), Uuid::from_u128(66), 10)
            .is_err()
    );
    assert_eq!(l.stage(), LocalStage::BootPending);
    assert!(l.with_active(|_| Ok(())).is_err());
    drop(l);
    let mut l = LocalIntent::open(&p, Uuid::from_u128(66)).unwrap();
    l.recover_boot(&mut peer, 10).unwrap();
    l.with_active(|s| {
        let view = s.snapshot()?;
        ensure!(
            view.active_boot == Uuid::from_u128(66) && view.next == 3,
            "global sequence/fence"
        );
        ensure!(
            s.source(Uuid::from_u128(8))?.hold.is_some(),
            "old source not fenced"
        );
        Ok(())
    })
    .unwrap();
    let requests: Vec<_> = peer
        .calls
        .iter()
        .filter(|r| r.operation_id == Uuid::from_u128(404))
        .collect();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0], requests[1]);
}
#[test]
fn owner_lock_replacement_refuses_original_owner_mutation_and_second_open() {
    use std::os::unix::fs::PermissionsExt;
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("owner");
    let mut peer = Peer::new();
    let l = active(&p, &mut peer);
    let old = std::fs::read(p.join("store/store.redb")).unwrap();
    std::fs::rename(p.join("owner.lock"), p.join("stolen-lock")).unwrap();
    std::fs::write(p.join("owner.lock"), []).unwrap();
    std::fs::set_permissions(p.join("owner.lock"), std::fs::Permissions::from_mode(0o600)).unwrap();
    assert!(l.with_active(|s| s.attach(support::source(8))).is_err());
    assert!(LocalIntent::open(&p, Uuid::from_u128(6)).is_err());
    assert_eq!(old, std::fs::read(p.join("store/store.redb")).unwrap());
}
#[test]
fn owner_process_child() {
    let Ok(root) = std::env::var("AQ_OBSERVATION_OWNER_CHILD_ROOT") else {
        return;
    };
    let path = Path::new(&root);
    let result = if std::env::var("AQ_OBSERVATION_OWNER_CHILD_CREATE").is_ok() {
        LocalIntent::create(
            path,
            Limits::fixture(),
            Uuid::from_u128(801),
            Uuid::from_u128(802),
        )
    } else {
        LocalIntent::open(path, Uuid::from_u128(6))
    };
    assert!(
        result.is_err(),
        "competing process unexpectedly acquired managed store"
    );
    if std::env::var("AQ_OWNER_EXPECT_DATABASE_LOCK").is_ok() {
        let error = format!("{:?}", result.err().unwrap()).to_ascii_lowercase();
        assert!(
            error.contains("lock") || error.contains("already open"),
            "unexpected failure before database lock: {error}"
        );
    }
}
#[test]
fn two_process_open_and_create_refuse_and_redb_lock_survives_replaced_owner_locks() {
    use std::os::unix::fs::PermissionsExt;
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("owner");
    let mut peer = Peer::new();
    let l = active(&p, &mut peer);
    let before = std::fs::read(p.join("store/store.redb")).unwrap();
    let database_only = std::cell::Cell::new(false);
    let child = |create: bool| {
        let mut c = std::process::Command::new(std::env::current_exe().unwrap());
        c.args(["--exact", "owner_process_child", "--nocapture"])
            .env("AQ_OBSERVATION_OWNER_CHILD_ROOT", &p);
        if create {
            c.env("AQ_OBSERVATION_OWNER_CHILD_CREATE", "1");
        }
        if database_only.get() {
            c.env("AQ_OWNER_EXPECT_DATABASE_LOCK", "1");
        }
        let o = c.output().unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    };
    child(false);
    child(true);
    // Move held locks outside the owner roots so the second process gets fresh
    // private lock inodes and reaches redb's whole-database lock. It must still
    // fail without changing the database, even though both advisory owner locks
    // have been replaced. The original owner also refuses further operations.
    for (from, to) in [
        (p.join("owner.lock"), t.path().join("held-outer")),
        (p.join("store/owner.lock"), t.path().join("held-store")),
    ] {
        std::fs::rename(&from, to).unwrap();
        std::fs::write(&from, []).unwrap();
        std::fs::set_permissions(&from, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    database_only.set(true);
    child(false);
    assert!(l.with_active(|s| s.attach(support::source(8))).is_err());
    assert_eq!(before, std::fs::read(p.join("store/store.redb")).unwrap());
}
fn boot_for(stage: &str) -> Uuid {
    Uuid::from_u128(if stage == "boot" { 66 } else { 6 })
}
fn setup_owner_case(root: &Path, peer_path: &Path, stage: &str) {
    let mut p = Peer::new();
    let l = match stage {
        "prepare" => LocalIntent::create(
            root,
            Limits::fixture(),
            Uuid::from_u128(401),
            Uuid::from_u128(402),
        )
        .unwrap(),
        "create" => prepared(root, &mut p),
        "activate" => {
            let mut l = prepared(root, &mut p);
            l.create_store(&mut p).unwrap();
            l
        }
        "boot" => {
            let l = active(root, &mut p);
            l.with_active(|s| {
                s.attach(support::source(8))?;
                support::append(s, 1);
                s.prepare()?;
                Ok(())
            })
            .unwrap();
            p.boot = 66;
            l
        }
        _ => panic!("stage"),
    };
    drop(l);
    p.file = Some(peer_path.into());
    p.persist().unwrap();
}
fn perform_owner_case(l: &mut LocalIntent, p: &mut Peer, stage: &str) -> Result<()> {
    match stage {
        "prepare" => l.prepare(p),
        "create" => l.create_store(p),
        "activate" => l.activate(p, Uuid::from_u128(403)),
        "boot" => l.advance_boot(p, Uuid::from_u128(404), Uuid::from_u128(66), 10),
        _ => panic!("stage"),
    }
}
fn copy_owner(from: &Path, to: &Path) {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().mode(0o700).create(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        if e.file_type().unwrap().is_dir() {
            copy_owner(&e.path(), &to.join(e.file_name()));
        } else {
            std::fs::copy(e.path(), to.join(e.file_name())).unwrap();
        }
    }
}
#[test]
fn owner_crash_boundary_child() {
    let Ok(root) = std::env::var("AQ_OWNER_FAULT_ROOT") else {
        return;
    };
    let peer = std::env::var("AQ_OWNER_FAULT_PEER").unwrap();
    let stage = std::env::var("AQ_OWNER_FAULT_STAGE").unwrap();
    let n = std::env::var("AQ_OWNER_FAULT_N").unwrap().parse().unwrap();
    let after = std::env::var("AQ_OWNER_FAULT_AFTER").unwrap() == "1";
    let mut l = LocalIntent::open(Path::new(&root), boot_for(&stage)).unwrap();
    let mut p = Peer::load(Path::new(&peer));
    let monitor = if std::env::var("AQ_OWNER_FAULT_BACKEND").is_ok() {
        l.store_monitor().unwrap()
    } else {
        l.intent_monitor()
    };
    monitor.fault(aq_observation_redb_prototype::backend::Fault {
        operation: n,
        after,
        action: aq_observation_redb_prototype::backend::Action::Exit,
    });
    perform_owner_case(&mut l, &mut p, &stage).unwrap();
    panic!("fault boundary was not reached");
}
fn assert_owner_recovery(root: &Path, peer_path: &Path, stage: &str) {
    let existed = root.join("store/store.redb").exists();
    let mut peer = Peer::load(peer_path);
    match LocalIntent::open(root, boot_for(stage)) {
        Err(_) => {
            // Retained temp or uncertain creation can require explicit recovery, but no
            // fresh constructor may erase/reseed it, even if central accepted-through0.
            assert!(
                LocalIntent::create(
                    root,
                    Limits::fixture(),
                    Uuid::from_u128(901),
                    Uuid::from_u128(902)
                )
                .is_err()
            );
            assert_eq!(existed, root.join("store/store.redb").exists());
        }
        Ok(mut l) => {
            match l.stage() {
                LocalStage::PreparePending | LocalStage::Prepared => {
                    l.prepare(&mut peer).unwrap();
                    l.create_store(&mut peer).unwrap();
                    l.activate(&mut peer, Uuid::from_u128(403)).unwrap();
                }
                LocalStage::StoreCreated | LocalStage::ActivationPending => {
                    l.activate(&mut peer, Uuid::from_u128(403)).unwrap()
                }
                LocalStage::Active => {
                    if stage == "boot"
                        && peer.owner.as_ref().unwrap().current_boot_id == Uuid::from_u128(6)
                    {
                        l.advance_boot(&mut peer, Uuid::from_u128(404), Uuid::from_u128(66), 10)
                            .unwrap();
                    } else {
                        l.resume(&mut peer).unwrap();
                    }
                }
                LocalStage::BootPending => l.recover_boot(&mut peer, 10).unwrap(),
                LocalStage::CreationStarted => {
                    panic!("open must reconcile existing marker or hold")
                }
            }
            l.with_active(|s| {
                let projection = s.snapshot()?;
                if stage == "boot" {
                    ensure!(
                        projection.next == 3
                            && projection.queued == 2
                            && projection.active_boot == Uuid::from_u128(66),
                        "boot facts/sequence changed"
                    );
                    let source = s.source(Uuid::from_u128(8))?;
                    ensure!(
                        source.cursor == support::row(1).len() as u64 && source.hold.is_some(),
                        "source cursor/fence changed"
                    );
                    let packet = s.prepare()?.unwrap();
                    ensure!(
                        packet.body.message
                            == serde_json::from_slice::<aq_lifecycle_protocol::Packet>(
                                &std::fs::read(root.parent().unwrap().join("before-packet.json"))?
                            )?
                            .body
                            .message,
                        "frozen old packet changed"
                    );
                } else {
                    ensure!(
                        projection.next == 1 && projection.queued == 0 && projection.seen == 0,
                        "bootstrap invented facts"
                    );
                }
                Ok(())
            })
            .unwrap();
        }
    }
}
fn walk_owner_boundaries(exit: bool) -> u64 {
    let temp = tempfile::tempdir().unwrap();
    let mut cuts = 0;
    for stage in ["prepare", "create", "activate", "boot"] {
        let base = temp.path().join(format!("base-{stage}"));
        let peer_path = temp.path().join(format!("base-{stage}-peer.json"));
        setup_owner_case(&base, &peer_path, stage);
        if stage == "boot" {
            let mut p = Peer::load(&peer_path);
            let mut l = LocalIntent::open(&base, Uuid::from_u128(6)).unwrap();
            p.boot = 6;
            l.resume(&mut p).unwrap();
            let packet = l.with_active(|s| Ok(s.prepare()?.unwrap())).unwrap();
            std::fs::write(
                temp.path().join("before-packet.json"),
                serde_json::to_vec(&packet).unwrap(),
            )
            .unwrap();
        }
        let measure = temp.path().join(format!("measure-{stage}"));
        copy_owner(&base, &measure);
        let mut l = LocalIntent::open(&measure, boot_for(stage)).unwrap();
        let mut p = Peer::load(&peer_path);
        p.file = None;
        l.intent_monitor().reset();
        perform_owner_case(&mut l, &mut p, stage).unwrap();
        let count = l.intent_monitor().stats().operations;
        drop(l);
        for n in 1..=count {
            for after in [false, true] {
                let root = temp.path().join(format!("cut-{stage}-{n}-{after}"));
                copy_owner(&base, &root);
                let peer = temp.path().join(format!("peer-{stage}-{n}-{after}.json"));
                std::fs::copy(&peer_path, &peer).unwrap();
                if exit {
                    let output = std::process::Command::new(std::env::current_exe().unwrap())
                        .args(["--exact", "owner_crash_boundary_child", "--nocapture"])
                        .env("AQ_OWNER_FAULT_ROOT", &root)
                        .env("AQ_OWNER_FAULT_PEER", &peer)
                        .env("AQ_OWNER_FAULT_STAGE", stage)
                        .env("AQ_OWNER_FAULT_N", n.to_string())
                        .env("AQ_OWNER_FAULT_AFTER", if after { "1" } else { "0" })
                        .output()
                        .unwrap();
                    assert_eq!(
                        output.status.code(),
                        Some(77),
                        "{} {}",
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    );
                } else {
                    let mut l = LocalIntent::open(&root, boot_for(stage)).unwrap();
                    let mut p = Peer::load(&peer);
                    l.intent_monitor()
                        .fault(aq_observation_redb_prototype::backend::Fault {
                            operation: n,
                            after,
                            action: aq_observation_redb_prototype::backend::Action::Error,
                        });
                    assert!(perform_owner_case(&mut l, &mut p, stage).is_err());
                }
                assert_owner_recovery(&root, &peer, stage);
                cuts += 1;
            }
        }
    }
    cuts
}
#[test]
fn owner_io_boundaries_reopen_consistently_or_hold_without_reseed() {
    eprintln!(
        "owner IO/transaction/network boundary cuts:{}",
        walk_owner_boundaries(false)
    );
}
#[test]
fn owner_real_process_exit_boundaries_reopen_consistently_or_hold_without_reseed() {
    eprintln!(
        "owner real process-exit boundary cuts:{}",
        walk_owner_boundaries(true)
    );
}
#[test]
fn corrupt_empty_or_missing_managed_store_and_corrupt_intent_are_preserved() {
    for kind in ["empty", "truncated", "missing", "intent"] {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("owner");
        let mut peer = Peer::new();
        drop(active(&p, &mut peer));
        let db = p.join("store/store.redb");
        let altered = match kind {
            "empty" => {
                std::fs::write(&db, []).unwrap();
                db
            }
            "truncated" => {
                std::fs::write(&db, [0u8; 511]).unwrap();
                db
            }
            "missing" => {
                std::fs::remove_file(&db).unwrap();
                db
            }
            _ => {
                let path = p.join("intent.json");
                std::fs::write(&path, b"{broken").unwrap();
                path
            }
        };
        let before = std::fs::read(&altered).ok();
        assert!(LocalIntent::open(&p, Uuid::from_u128(6)).is_err());
        assert_eq!(before, std::fs::read(&altered).ok());
        assert!(
            LocalIntent::create(
                &p,
                Limits::fixture(),
                Uuid::from_u128(901),
                Uuid::from_u128(902)
            )
            .is_err()
        );
        assert_eq!(before, std::fs::read(&altered).ok());
    }
}
#[test]
fn held_owner_root_or_intent_replacement_refuses_mutation() {
    for replace_intent in [false, true] {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("owner");
        let mut peer = Peer::new();
        let l = active(&p, &mut peer);
        let before = std::fs::read(p.join("store/store.redb")).unwrap();
        if replace_intent {
            let bytes = std::fs::read(p.join("intent.json")).unwrap();
            std::fs::rename(p.join("intent.json"), t.path().join("held-intent")).unwrap();
            std::fs::write(p.join("intent.json"), bytes).unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                p.join("intent.json"),
                std::fs::Permissions::from_mode(0o600),
            )
            .unwrap();
            assert!(l.with_active(|s| s.attach(support::source(8))).is_err());
            assert_eq!(before, std::fs::read(p.join("store/store.redb")).unwrap());
        } else {
            let moved = t.path().join("moved");
            std::fs::rename(&p, &moved).unwrap();
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new().mode(0o700).create(&p).unwrap();
            assert!(l.with_active(|s| s.attach(support::source(8))).is_err());
            assert_eq!(
                before,
                std::fs::read(moved.join("store/store.redb")).unwrap()
            );
        }
    }
}
fn walk_owner_backend_boundaries(exit: bool) -> u64 {
    let temp = tempfile::tempdir().unwrap();
    let mut cuts = 0;
    for stage in ["activate", "boot"] {
        let base = temp.path().join(format!("base-{stage}"));
        let peer_path = temp.path().join(format!("base-{stage}-peer.json"));
        setup_owner_case(&base, &peer_path, stage);
        if stage == "boot" {
            let mut p = Peer::load(&peer_path);
            let mut l = LocalIntent::open(&base, Uuid::from_u128(6)).unwrap();
            p.boot = 6;
            l.resume(&mut p).unwrap();
            let packet = l.with_active(|s| Ok(s.prepare()?.unwrap())).unwrap();
            std::fs::write(
                temp.path().join("before-packet.json"),
                serde_json::to_vec(&packet).unwrap(),
            )
            .unwrap();
        }
        let measure = temp.path().join(format!("measure-{stage}"));
        copy_owner(&base, &measure);
        let mut l = LocalIntent::open(&measure, boot_for(stage)).unwrap();
        let mut p = Peer::load(&peer_path);
        p.file = None;
        l.store_monitor().unwrap().reset();
        perform_owner_case(&mut l, &mut p, stage).unwrap();
        let count = l.store_monitor().unwrap().stats().operations;
        drop(l);
        for n in 1..=count {
            for after in [false, true] {
                let root = temp.path().join(format!("cut-{stage}-{n}-{after}"));
                copy_owner(&base, &root);
                let peer = temp.path().join(format!("peer-{stage}-{n}-{after}.json"));
                std::fs::copy(&peer_path, &peer).unwrap();
                if exit {
                    let output = std::process::Command::new(std::env::current_exe().unwrap())
                        .args(["--exact", "owner_crash_boundary_child", "--nocapture"])
                        .env("AQ_OWNER_FAULT_ROOT", &root)
                        .env("AQ_OWNER_FAULT_PEER", &peer)
                        .env("AQ_OWNER_FAULT_STAGE", stage)
                        .env("AQ_OWNER_FAULT_N", n.to_string())
                        .env("AQ_OWNER_FAULT_AFTER", if after { "1" } else { "0" })
                        .env("AQ_OWNER_FAULT_BACKEND", "1")
                        .output()
                        .unwrap();
                    assert_eq!(
                        output.status.code(),
                        Some(77),
                        "{} {}",
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    );
                } else {
                    let mut l = LocalIntent::open(&root, boot_for(stage)).unwrap();
                    let mut p = Peer::load(&peer);
                    l.store_monitor().unwrap().fault(
                        aq_observation_redb_prototype::backend::Fault {
                            operation: n,
                            after,
                            action: aq_observation_redb_prototype::backend::Action::Error,
                        },
                    );
                    assert!(perform_owner_case(&mut l, &mut p, stage).is_err());
                }
                assert_owner_recovery(&root, &peer, stage);
                cuts += 1;
            }
        }
    }
    cuts
}
#[test]
fn owner_database_error_cuts_preserve_activation_and_boot_fence_atomicity() {
    eprintln!(
        "owner database IO-error boundary cuts:{}",
        walk_owner_backend_boundaries(false)
    );
}
#[test]
fn owner_database_process_exits_preserve_activation_and_boot_fence_atomicity() {
    eprintln!(
        "owner database process-exit boundary cuts:{}",
        walk_owner_backend_boundaries(true)
    );
}
