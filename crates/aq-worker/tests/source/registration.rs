fn new_repository() -> SourceBinding {
    SourceBinding {
        repository_id: Uuid::new_v4(),
        binding_generation: 1,
        logical_identity: "owner/never-configured".into(),
        base: serde_json::from_value(json!({"algorithm":"sha1","hex":"a".repeat(40)})).unwrap(),
    }
}
/// Two slots meet a repository their aq-git has never seen at the same moment: each reads,
/// is refused, and registers it from the offer's binding. aq-git registers it once; the
/// other caller is told it exists and accepts it because it is the same binding.
fn register_twice_at_once(client: &GitClient, b: &SourceBinding) {
    let start = std::sync::Barrier::new(2);
    std::thread::scope(|s| {
        let run = || {
            start.wait();
            client.registering(b, || client.validate_binding(b))
        };
        let (one, two) = (s.spawn(run), s.spawn(run));
        one.join().unwrap().unwrap();
        two.join().unwrap().unwrap();
    });
    client.validate_binding(b).unwrap();
    // Seen before: nothing is registered again, and another identity for it is refused.
    client
        .registering(b, || client.validate_binding(b))
        .unwrap();
    let other = SourceBinding {
        logical_identity: "owner/another".into(),
        ..b.clone()
    };
    let refused = client.registering(&other, || client.validate_binding(&other));
    assert!(refused.is_err(), "an identity is never replaced");
}
#[test]
fn a_repository_aq_git_has_not_seen_is_registered_once_from_the_offer() {
    let temp = TempDir::new().unwrap();
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let socket = temp.path().join("git.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
    // (the registered binding, refused reads so far) and every request.
    let state = Arc::new((Mutex::new((Value::Null, 0)), std::sync::Condvar::new()));
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let (shared, recorded) = (state.clone(), requests.clone());
    spawn_registration_peer(listener, shared, recorded);
    let client = GitClient::new(&socket, temp.path()).unwrap();
    let b = new_repository();
    register_twice_at_once(&client, &b);
    let requests = requests.lock().unwrap();
    let registers: Vec<&Value> = (requests.iter())
        .filter(|r| r["operation"] == "repo.register")
        .collect();
    assert_eq!(
        registers.len(),
        3,
        "two at once, one for the other identity"
    );
    for r in &registers[..2] {
        assert_eq!(r["repository_id"], b.repository_id.to_string());
        assert_eq!(r["expected_binding_generation"], 0);
        assert_eq!(
            r["params"]["binding"],
            json!({"identity":"owner/never-configured","mode":"worker","binding_generation":1,"object_format":"sha1"})
        );
    }
    assert_eq!(
        state.0.lock().unwrap().0["identity"],
        "owner/never-configured"
    );
}
/// Recovery restarts aq-git on an attempt's state and registers the repository its recorded
/// offer named, since the worker lists none: an unseen one is registered from the offer's
/// binding, one the state holds is accepted without a second registration, and the same
/// repository under another binding generation is refused.
#[test]
fn recovery_registers_the_recorded_offer_and_refuses_a_changed_generation() {
    let temp = TempDir::new().unwrap();
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let socket = temp.path().join("git.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
    // Nothing registered; the peer does not wait for a second concurrent reader.
    let state = Arc::new((Mutex::new((Value::Null, 2)), std::sync::Condvar::new()));
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    spawn_registration_peer(listener, state.clone(), requests.clone());
    let client = GitClient::new(&socket, temp.path()).unwrap();
    let recorded = new_repository();
    let ensure = |b: &SourceBinding| client.registering(b, || client.validate_binding(b));
    ensure(&recorded).unwrap();
    ensure(&recorded).unwrap();
    let registers = |requests: &Mutex<Vec<Value>>| {
        let requests = requests.lock().unwrap();
        (requests.iter())
            .filter(|r| r["operation"] == "repo.register")
            .count()
    };
    assert_eq!(registers(&requests), 1, "the second call found it registered");
    assert_eq!(
        state.0.lock().unwrap().0["identity"],
        "owner/never-configured"
    );
    let moved = SourceBinding {
        binding_generation: 2,
        ..recorded
    };
    let refused = ensure(&moved).unwrap_err();
    assert!(
        format!("{refused:#}").contains("already registered"),
        "{refused:#}"
    );
}
/// The same against a real aq-git (any build the worker is installed beside).
#[test]
#[ignore = "requires AQ_GIT_TEST_BINARY (an aq-git build)"]
fn a_real_aq_git_registers_a_repository_it_has_not_seen_once() {
    use sha2::{Digest, Sha256};
    let exe = std::path::PathBuf::from(std::env::var_os("AQ_GIT_TEST_BINARY").unwrap());
    let pin = format!("{:x}", Sha256::digest(std::fs::read(&exe).unwrap()));
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("git");
    let service = aq_worker::source::GitService::start(&exe, &pin, &root).unwrap();
    let b = new_repository();
    let unseen = service.client.validate_binding(&b).unwrap_err();
    assert!(
        format!("{unseen:#}").contains("repository_not_found"),
        "{unseen:#}"
    );
    register_twice_at_once(&service.client, &b);
}

type RegistrationState = (Mutex<(Value, usize)>, std::sync::Condvar);
type GitReply = Result<Value, (&'static str, &'static str)>;
fn spawn_registration_peer(
    listener: UnixListener,
    shared: Arc<RegistrationState>,
    recorded: Arc<Mutex<Vec<Value>>>,
) {
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let (shared, recorded) = (shared.clone(), recorded.clone());
            let mut conn = conn.unwrap();
            std::thread::spawn(move || serve_registration(&mut conn, &shared, &recorded));
        }
    });
}

fn serve_registration(
    conn: &mut std::os::unix::net::UnixStream,
    shared: &RegistrationState,
    recorded: &Mutex<Vec<Value>>,
) {
    let mut line = String::new();
    BufReader::new(conn.try_clone().unwrap())
        .read_line(&mut line)
        .unwrap();
    let req: Value = serde_json::from_str(&line).unwrap();
    recorded.lock().unwrap().push(req.clone());
    let result = registration_reply(shared, &req);
    let mut reply = json!({"protocol_version":1,"request_id":req["request_id"]});
    match result {
        Ok(result) => {
            reply["ok"] = json!(true);
            reply["result"] = result;
            if let Some(id) = req.get("operation_id") {
                reply["operation_id"] = id.clone();
            }
        }
        Err((code, message)) => {
            reply["ok"] = json!(false);
            reply["error"] = json!({"code":code,"message":message,"details":null});
        }
    }
    let _ = conn.write_all(format!("{reply}\n").as_bytes());
}

fn registration_reply(shared: &RegistrationState, req: &Value) -> GitReply {
    let (lock, both_refused) = shared;
    let mut state = lock.lock().unwrap();
    match req["operation"].as_str().unwrap() {
        "capabilities" => Ok(
            json!({"protocol_version":1,"artifact_format_version":1,"limits":{"artifact":{"max_expanded_bytes":536870912_u64,"max_bundle_bytes":67108864_u64}},"git":{"checkout_profile":"no_attributes_v1"},"operations":["repo.show","artifact.import","workspace.create","workspace.checkpoint","workspace.retire","artifact.export","retention.release","repo.cache-evict"]}),
        ),
        "repo.show" if state.0.is_null() => {
            state.1 += 1;
            both_refused.notify_all();
            Err(("repository_not_found", "Repository is not registered"))
        }
        "repo.show" => Ok(state.0.clone()),
        "repo.register" => {
            // Both callers were refused their read before either registers.
            state = both_refused.wait_while(state, |s| s.1 < 2).unwrap();
            if state.0.is_null() {
                let mut binding = req["params"]["binding"].clone();
                binding["repository_id"] = req["repository_id"].clone();
                state.0 = binding.clone();
                Ok(binding)
            } else {
                Err(("binding_conflict", "Repository already registered"))
            }
        }
        other => panic!("unexpected operation {other}"),
    }
}
