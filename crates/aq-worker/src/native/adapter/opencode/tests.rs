use super::*;
use aq_session_protocol::{ArtifactRef, Capability, Digest, Text};

fn settings(confinement: &str) -> (HarnessSettings, ModelSettings) {
    let artifact = ArtifactRef {
        id: Text::new("fixture").unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(b"fixture"),
    };
    let caps = ["aq.harness.startup-context", confinement, "aq.harness.stop"]
        .map(|id| Capability {
            id: Text::new(id).unwrap(),
            version: 1,
        })
        .to_vec();
    (
        HarnessSettings {
            harness: Text::new("opencode").unwrap(),
            cli_version: Text::new(OPENCODE_CLI_VERSION).unwrap(),
            adapter: artifact.clone(),
            environment: artifact,
            required_capabilities: caps,
        },
        ModelSettings {
            provider: Text::new("openai").unwrap(),
            model: Text::new("gpt-5.5").unwrap(),
            reasoning: Text::new("high").unwrap(),
            max_output_tokens: 4096,
            spending_budget: None,
        },
    )
}
fn paths() -> OpenCodePaths<'static> {
    OpenCodePaths {
        executable: Path::new("/opt/aq/bin/opencode"),
        workspace: Path::new("/work"),
        private_home: Path::new("/home/agent/.aq/opencode"),
    }
}
fn container() -> (HarnessSettings, ModelSettings) {
    settings(CONTAINER_CONFINEMENT_CAPABILITY)
}
fn config_json(recipe: &OpenCodeRecipe) -> serde_json::Value {
    serde_json::from_slice(&recipe.config_json).unwrap()
}
const PROXY: OpenCodeRoute = OpenCodeRoute::ResponsesProxy { port: 41234 };

#[test]
fn proxy_recipe_runs_headless_json_with_private_config_at_loopback_proxy() {
    let (harness, model) = container();
    let recipe = map(&harness, &model, paths(), ConfinementMode::Container, PROXY).unwrap();
    assert_eq!(recipe.executable, Path::new("/opt/aq/bin/opencode"));
    assert_eq!(recipe.working_directory, Path::new("/work"));
    assert_eq!(
        recipe.arguments,
        [
            "run",
            "--format",
            "json",
            "--pure",
            "--model",
            "openai/gpt-5.5"
        ]
    );
    let env = &recipe.environment;
    let home = "/home/agent/.aq/opencode";
    assert_eq!(env["HOME"], home);
    for (k, sub) in [
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_CACHE_HOME", "cache"),
        ("XDG_STATE_HOME", "state"),
        ("OPENCODE_CONFIG", CONFIG_FILE),
    ] {
        assert_eq!(env[k], format!("{home}/{sub}"));
    }
    assert_eq!(
        Path::new(&env["XDG_DATA_HOME"]).join("opencode/auth.json"),
        Path::new(home).join(AUTH_FILE)
    );
    for flag in [
        "OPENCODE_DISABLE_PROJECT_CONFIG",
        "OPENCODE_DISABLE_AUTOUPDATE",
        "OPENCODE_DISABLE_MODELS_FETCH",
        "OPENCODE_DISABLE_CLAUDE_CODE",
        "OPENCODE_DISABLE_EXTERNAL_SKILLS",
        "OPENCODE_DISABLE_DEFAULT_PLUGINS",
    ] {
        assert_eq!(env[flag], "1", "{flag}");
    }
    assert_eq!(env["npm_config_offline"], "true");
    for k in FORBIDDEN_ENVIRONMENT {
        assert!(!env.contains_key(*k), "{k} must not be set");
    }
    let c = config_json(&recipe);
    assert_eq!(
        c["provider"]["openai"]["options"]["baseURL"],
        "http://127.0.0.1:41234/v1"
    );
    assert_eq!(c["enabled_providers"], serde_json::json!(["openai"]));
    assert_eq!(c["model"], "openai/gpt-5.5");
    assert_eq!(c["permission"]["*"], "allow");
    assert_eq!(c["permission"]["external_directory"], "deny");
    assert_eq!(c["permission"]["question"], "deny");
    assert_eq!(c["autoupdate"], false);
    assert_eq!(c["share"], "disabled");
    // Every request is the selected turn the B6b policy admits: effort and cap
    // from ModelSettings, no title request, no automatic compaction, no web.
    assert_eq!(
        c["provider"]["openai"]["models"]["gpt-5.5"]["options"]["reasoningEffort"],
        "high"
    );
    assert_eq!(env["OPENCODE_EXPERIMENTAL_OUTPUT_TOKEN_MAX"], "4096");
    assert_eq!(c["agent"]["title"]["disable"], true);
    assert_eq!(c["compaction"]["auto"], false);
    assert_eq!(c["permission"]["webfetch"], "deny");
    assert_eq!(c["permission"]["websearch"], "deny");
}

#[test]
fn subscription_recipe_keeps_the_stock_endpoint() {
    let (harness, model) = container();
    let recipe = map(
        &harness,
        &model,
        paths(),
        ConfinementMode::Container,
        OpenCodeRoute::Subscription,
    )
    .unwrap();
    // The ChatGPT login is a built-in plugin, so built-ins stay on; npm stays offline.
    assert!(
        !recipe
            .environment
            .contains_key("OPENCODE_DISABLE_DEFAULT_PLUGINS")
    );
    assert_eq!(recipe.environment["npm_config_offline"], "true");
    assert_eq!(recipe.arguments[3], "--pure");
    let c = config_json(&recipe);
    assert!(c["provider"]["openai"].get("options").is_none());
    assert!(
        !String::from_utf8(recipe.config_json)
            .unwrap()
            .contains("127.0.0.1")
    );
}

#[test]
fn opencode_recipe_refuses_native_mode_and_unsupported_settings() {
    let (harness, model) = settings(NATIVE_CONFINEMENT_CAPABILITY);
    assert!(map(&harness, &model, paths(), ConfinementMode::Native, PROXY).is_err());
    assert!(map(&harness, &model, paths(), ConfinementMode::Container, PROXY).is_err());
    let (harness, model) = container();
    assert!(map(&harness, &model, paths(), ConfinementMode::Native, PROXY).is_err());
    let port0 = OpenCodeRoute::ResponsesProxy { port: 0 };
    assert!(map(&harness, &model, paths(), ConfinementMode::Container, port0).is_err());
    let mut h = harness.clone();
    h.cli_version = Text::new("1.18.36").unwrap();
    assert!(map(&h, &model, paths(), ConfinementMode::Container, PROXY).is_err());
    let mut h = harness.clone();
    h.harness = Text::new("codex").unwrap();
    assert!(map(&h, &model, paths(), ConfinementMode::Container, PROXY).is_err());
    let mut m = model.clone();
    m.provider = Text::new("anthropic").unwrap();
    assert!(map(&harness, &m, paths(), ConfinementMode::Container, PROXY).is_err());
    let bad = [
        OpenCodePaths {
            workspace: Path::new("/home/agent/.aq"),
            ..paths()
        },
        OpenCodePaths {
            private_home: Path::new("/work/.opencode"),
            ..paths()
        },
        OpenCodePaths {
            executable: Path::new("opencode"),
            ..paths()
        },
        OpenCodePaths {
            private_home: Path::new("/home/agent/../x"),
            ..paths()
        },
    ];
    for p in bad {
        assert!(map(&harness, &model, p, ConfinementMode::Container, PROXY).is_err());
    }
}

fn login(expires: i64) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({"openai": {
        "type": "oauth", "refresh": "rt-fixture", "access": "at-fixture",
        "expires": expires, "accountId": "acct-fixture"}}))
    .unwrap()
}

#[test]
fn chatgpt_login_validation_accepts_only_one_openai_oauth_entry() {
    assert_eq!(
        validate_chatgpt_auth_json(&login(1_800_000_000_000)).unwrap(),
        1_800_000_000_000
    );
    let refused = [
        serde_json::json!({"openai": {"type": "api", "key": "sk-fixture"}}),
        serde_json::json!({"openai": {"type": "oauth", "refresh": "", "access": "a", "expires": 1}}),
        serde_json::json!({"openai": {"type": "oauth", "refresh": "r", "access": "a", "expires": 0}}),
        serde_json::json!({"openai": {"type": "oauth", "refresh": "r", "access": "a", "expires": 1},
            "anthropic": {"type": "api", "key": "sk-fixture"}}),
        serde_json::json!({"openai": {"type": "oauth", "refresh": "r", "access": "a", "expires": 1,
            "extra": true}}),
        serde_json::json!({"openai": {"type": "oauth", "refresh": "r", "access": "a", "expires": 1,
            "enterpriseUrl": "https://example.invalid"}}),
    ];
    for v in refused {
        assert!(
            validate_chatgpt_auth_json(&serde_json::to_vec(&v).unwrap()).is_err(),
            "{v}"
        );
    }
    assert!(validate_chatgpt_auth_json(b"").is_err());
    assert!(validate_chatgpt_auth_json(&vec![b' '; MAX_AUTH_BYTES + 1]).is_err());
}

#[test]
fn ollama_recipe_enables_only_the_local_server_and_no_credential() {
    let (harness, mut model) = container();
    model.provider = Text::new("ollama").unwrap();
    model.model = Text::new("qwen3.5:9b").unwrap();
    let host = std::net::Ipv4Addr::new(169, 254, 1, 2);
    let route = OpenCodeRoute::Ollama { host, port: 18434 };
    let recipe = map(&harness, &model, paths(), ConfinementMode::Container, route).unwrap();
    assert_eq!(recipe.arguments[4..], ["--model", "ollama/qwen3.5:9b"]);
    assert_eq!(recipe.environment["OPENCODE_DISABLE_DEFAULT_PLUGINS"], "1");
    assert_eq!(recipe.environment["npm_config_offline"], "true");
    let c = config_json(&recipe);
    assert_eq!(c["enabled_providers"], serde_json::json!(["ollama"]));
    assert_eq!(c["model"], "ollama/qwen3.5:9b");
    assert!(c["provider"].get("openai").is_none());
    assert_eq!(
        c["provider"]["ollama"]["options"],
        serde_json::json!({"baseURL": "http://169.254.1.2:18434/v1"})
    );
    assert_eq!(c["provider"]["ollama"]["npm"], "@ai-sdk/openai-compatible");
    // The provider must match the route, and the origin must stay local.
    for (provider, host, port) in [
        ("openai", host, 18434),
        ("ollama", std::net::Ipv4Addr::new(10, 0, 0, 1), 18434),
        ("ollama", host, 0),
    ] {
        model.provider = Text::new(provider).unwrap();
        assert!(
            map(
                &harness,
                &model,
                paths(),
                ConfinementMode::Container,
                OpenCodeRoute::Ollama { host, port },
            )
            .is_err(),
            "{provider} {host}:{port}"
        );
    }
    model.provider = Text::new("ollama").unwrap();
    assert!(map(&harness, &model, paths(), ConfinementMode::Container, PROXY).is_err());
}
