use super::*;
#[test]
fn old_slots_key_is_node_count_and_new_files_name_nodes() {
    let config = valid();
    let current = toml::to_string(&config).unwrap();
    assert!(current.contains("nodes = 2"));
    let legacy = current.replace("nodes = 2", "slots = 2");
    assert_eq!(ServiceConfig::parse(&legacy).unwrap().nodes, 2);
    assert!(ServiceConfig::parse(&format!("nodes = 1\n{legacy}")).is_err());
}
fn valid() -> ServiceConfig {
    ServiceConfig {
        schema_version: 1,
        profile: "local".into(),
        machine_key: "desk".into(),
        nodes: 2,
        native_template: "/etc/aq/native.json".into(),
        state_root: "/var/aq".into(),
        runtime_root: Some("/run/user/1000/aq/local".into()),
        aq_git: AqGitConfig {
            binary: "/opt/aq-git".into(),
            git_executable: "/usr/bin/git".into(),
        },
        repositories: vec![],
        manual: None,
        shutdown_grace_ms: 120_000,
        retry: Default::default(),
        observations: Default::default(),
    }
}
fn manual(token: &str) -> ManualConfig {
    ManualConfig {
        podman_executable: "/usr/bin/podman".into(),
        max_slots: 1,
        claude_token_file: token.into(),
        source_slugs: Default::default(),
        codex: None,
    }
}
const MINIMAL: &str = r#"schema_version = 1
profile = "local"
machine_key = "desk"
slots = 1
native_template = "/n.json"
state_root = "/s"
runtime_root = "/run/user/1000/aq/local"
[aq_git]
binary = "/g"
git_executable = "/usr/bin/git"
"#;
fn load_text(text: &str) -> Result<ServiceConfig> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("service.toml");
    write_private(&path, text.as_bytes()).unwrap();
    ServiceConfig::load(&path)
}
const MANUAL: &str = "[manual]\npodman_executable = \"/usr/bin/podman\"\nmax_slots = 1\n";
mod migration;

/// Manual sessions name their Claude subscription token file in `[manual]`; it is
/// required there and must be absolute.
#[test]
fn manual_sessions_name_their_claude_token_file() {
    let manual = MANUAL;
    let c = load_text(&format!(
        "{MINIMAL}{manual}claude_token_file = \"/etc/aq/token\"\n"
    ))
    .unwrap();
    assert_eq!(
        c.manual.as_ref().unwrap().claude_token_file,
        Path::new("/etc/aq/token")
    );
    let saved = toml::to_string(&c).unwrap();
    assert!(
        saved.contains("claude_token_file = \"/etc/aq/token\""),
        "{saved}"
    );
    assert!(!saved.contains("measurement"), "{saved}");
    let e = format!(
        "{:#}",
        load_text(&format!("{MINIMAL}{manual}")).unwrap_err()
    );
    assert!(e.contains("missing field `claude_token_file`"), "{e}");
    let relative = format!("{MINIMAL}{manual}claude_token_file = \"token\"\n");
    let e = format!("{:#}", load_text(&relative).unwrap_err());
    assert!(
        e.contains("manual.claude_token_file must be an absolute path"),
        "{e}"
    );
}
#[test]
fn loads_valid_and_refuses_bounds() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("service.toml");
    valid().save(&path).unwrap();
    assert_eq!(ServiceConfig::load(&path).unwrap().nodes, 2);
    let bad: [fn(&mut ServiceConfig); 9] = [
        |c| c.nodes = 0,
        |c| c.nodes = 9,
        |c| c.native_template = "native.json".into(),
        |c| c.profile = "Bad_Name".into(),
        |c| c.machine_key = "Desk".into(),
        |c| c.manual = Some(manual("token")),
        |c| {
            c.manual = Some(ManualConfig {
                max_slots: 9,
                ..manual("/t")
            })
        },
        |c| c.runtime_root = Some("run/aq".into()),
        |c| {
            c.repositories = vec![RepositoryBinding {
                repository_id: Uuid::nil(),
                identity: "x".into(),
                binding_generation: 1,
                object_format: "sha256".into(),
            }]
        },
    ];
    for edit in bad {
        let mut c = valid();
        edit(&mut c);
        assert!(c.validate().is_err());
    }
}
#[test]
fn defaults_fill_the_grace() {
    let text = r#"schema_version = 1
profile = "local"
machine_key = "desk"
slots = 1
native_template = "/n.json"
state_root = "/s"
[aq_git]
binary = "/g"
git_executable = "/usr/bin/git"
"#;
    let c: ServiceConfig = toml::from_str(text).unwrap();
    c.validate().unwrap();
    assert_eq!(c.shutdown_grace_ms, 120_000);
    assert_retry_defaults(&c);
    assert_observation_overrides(text, &c);
    assert_retry_overrides(text);
    assert_eq!(c.stop_timeout_secs(), 150, "the template's default");
    let longest = ServiceConfig {
        shutdown_grace_ms: 600_000,
        ..c.clone()
    };
    assert_eq!(longest.stop_timeout_secs(), 630);
    assert!(c.repositories.is_empty() && c.manual.is_none());
    assert!(c.runtime_root.is_none());
    assert!(!toml::to_string(&c).unwrap().contains("runtime_root"));
}
/// D1: with a long home, the default state root holds no socket; the runtime root keeps
/// every socket path under the limit, and a runtime root whose paths cannot fit is refused
/// when the configuration is loaded, naming the path and its length.
#[test]
fn socket_paths_fit_or_the_config_is_refused() {
    let home = format!("/home/{}", "u".repeat(60));
    let mut c = valid();
    c.nodes = 8;
    c.state_root = format!("{home}/.local/state/agent-q/service/{}", c.profile).into();
    c.runtime_root = None;
    let layout = crate::Layout::for_service(Path::new("/c"), &c);
    for slot in 1..=8 {
        for socket in [
            layout.git_socket(slot),
            layout
                .host_root(slot)
                .join(format!("{}/tmux.sock", Uuid::nil())),
        ] {
            assert!(!socket.starts_with(&c.state_root), "{}", socket.display());
        }
    }
    c.validate().unwrap();
    // The longest uid and the longest profile each fallback root allows.
    c.runtime_root = Some(format!("/run/user/4294967294/aq/{}", "a".repeat(14)).into());
    c.validate().unwrap();
    c.runtime_root = Some(format!("/run/user/4294967294/aq/{}", "a".repeat(15)).into());
    assert!(c.validate().is_err());
    let fallback = PathBuf::from("/tmp/aq-1000").join("a".repeat(25));
    crate::layout::check_socket_paths(&fallback, 8).unwrap();
    // The live failure: the state root of a normal home cannot hold the sockets.
    let live = Path::new("/home/jkern/.local/state/agent-q/service/ms1/slots/1/current");
    assert!(crate::layout::check_socket_paths(live, 1).is_err());
    c.runtime_root = Some(format!("{home}/run").into());
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("service.toml");
    let text = toml::to_string(&c).unwrap();
    crate::enrollment_file::write_private(&path, text.as_bytes()).unwrap();
    let e = format!("{:#}", ServiceConfig::load(&path).unwrap_err());
    let long = format!("{home}/run/8/host/{}/terminal-", Uuid::nil());
    assert!(e.contains(&long), "{e}");
    assert!(
        e.contains("would be 135 bytes, over the 103-byte limit"),
        "{e}"
    );
}

fn assert_retry_defaults(c: &ServiceConfig) {
    // No repositories list: every repository is registered from its offer. No [retry]:
    // the defaults, which are not written back.
    assert!(c.repositories.is_empty());
    let r = &c.retry;
    assert_eq!(
        (r.offer_pause_ms, r.offer_pause_max_ms),
        (3_000, 30_000),
        "the short pause and its ceiling"
    );
    assert_eq!(
        (r.fault_backoff_start_ms, r.fault_backoff_max_ms),
        (30_000, 300_000)
    );
    assert!(!toml::to_string(c).unwrap().contains("retry"));
}

fn assert_observation_overrides(text: &str, c: &ServiceConfig) {
    // No [observations]: the defaults, and one key overrides only itself.
    assert_eq!(c.observations, ObservationConfig::default());
    assert!(!toml::to_string(c).unwrap().contains("observations"));
    let tuned = format!("{text}[observations]\npoll_ms = 1000\n");
    let tuned: ServiceConfig = toml::from_str(&tuned).unwrap();
    tuned.validate().unwrap();
    assert_eq!(tuned.observations.poll_ms, 1_000);
    assert_eq!(tuned.observations.store_file_bytes, 64 * 1024 * 1024);
    let mut small = tuned.clone();
    small.observations.store_data_bytes = small.observations.store_file_bytes;
    assert!(small.validate().is_err());
}

fn assert_retry_overrides(text: &str) {
    let set = format!("{text}[retry]\noffer_pause_max_ms = 10000\n");
    let set = ServiceConfig::parse(&set).unwrap();
    set.validate().unwrap();
    assert_eq!(set.retry.offer_pause_max_ms, 10_000);
    assert_eq!(set.retry.offer_pause_ms, 3_000, "each key is optional");
    let again = ServiceConfig::parse(&toml::to_string(&set).unwrap()).unwrap();
    assert_eq!(again.retry, set.retry);
    let mut bad = set.clone();
    bad.retry.offer_pause_max_ms = 1_000;
    assert!(bad.validate().is_err(), "a ceiling below the pause");
}
