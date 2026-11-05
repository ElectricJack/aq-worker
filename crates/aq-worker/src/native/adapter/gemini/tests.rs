use super::*;
use aq_session_protocol::{ArtifactRef, Capability, Digest, Text};

const CAP: &str = "aq-gemini-0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

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
            harness: Text::new("gemini").unwrap(),
            cli_version: Text::new(GEMINI_CLI_VERSION).unwrap(),
            adapter: artifact.clone(),
            environment: artifact,
            required_capabilities: caps,
        },
        ModelSettings {
            provider: Text::new("google").unwrap(),
            model: Text::new("gemini-2.5-flash").unwrap(),
            reasoning: Text::new("high").unwrap(),
            max_output_tokens: 4096,
            spending_budget: None,
        },
    )
}
fn paths() -> GeminiPaths<'static> {
    GeminiPaths {
        node: Path::new("/opt/aq/node/bin/node"),
        script: Path::new("/opt/aq/harnesses/gemini/bundle/gemini.js"),
        workspace: Path::new("/work"),
        private_home: Path::new("/home/agent/.aq/gemini"),
    }
}
fn proxy() -> GeminiAuth<'static> {
    GeminiAuth::ApiKeyProxy {
        origin: "http://127.0.0.1:4242",
        capability: CAP,
    }
}
fn container() -> (HarnessSettings, ModelSettings) {
    settings(aq_session_protocol::CONTAINER_CONFINEMENT_CAPABILITY)
}
fn flag<'a>(recipe: &'a GeminiRecipe, name: &str) -> &'a str {
    let i = recipe.arguments.iter().position(|a| a == name).unwrap();
    assert_eq!(recipe.arguments.iter().filter(|a| *a == name).count(), 1);
    &recipe.arguments[i + 1]
}
fn system(recipe: &GeminiRecipe) -> serde_json::Value {
    serde_json::from_slice(&recipe.system_settings_json).unwrap()
}

#[test]
fn proxy_recipe_runs_bundle_node_headless_with_loopback_key_route() {
    let (h, m) = container();
    let r = map(&h, &m, paths(), ConfinementMode::Container, proxy()).unwrap();
    assert_eq!(r.executable, Path::new("/opt/aq/node/bin/node"));
    assert_eq!(r.arguments[0], "/opt/aq/harnesses/gemini/bundle/gemini.js");
    assert_eq!(r.working_directory, Path::new("/work"));
    assert_eq!(flag(&r, "-p"), "");
    assert_eq!(flag(&r, "--output-format"), "stream-json");
    assert_eq!(flag(&r, "--approval-mode"), "yolo");
    assert_eq!(flag(&r, "--model"), "gemini-2.5-flash");
    assert!(r.arguments.iter().any(|a| a == "--skip-trust"));
    assert!(!r.arguments.iter().any(|a| a == "--sandbox" || a == "-s"));
    let e = &r.environment;
    assert_eq!(e["HOME"], "/home/agent/.aq/gemini");
    assert_eq!(e["GEMINI_CLI_HOME"], "/home/agent/.aq/gemini");
    assert_eq!(
        e["GEMINI_CLI_SYSTEM_SETTINGS_PATH"],
        "/home/agent/.aq/gemini/system-settings.json"
    );
    assert_eq!(e["GEMINI_CLI_NO_RELAUNCH"], "true");
    assert_eq!(e["GOOGLE_GEMINI_BASE_URL"], "http://127.0.0.1:4242");
    assert_eq!(e["GEMINI_API_KEY"], CAP);
    for name in FORBIDDEN_ENVIRONMENT {
        if !matches!(*name, "GEMINI_API_KEY" | "GOOGLE_GEMINI_BASE_URL") {
            assert!(!e.contains_key(*name), "{name} set");
        }
    }
    let s = system(&r);
    assert_eq!(s["security"]["auth"]["selectedType"], "gemini-api-key");
    assert_eq!(s["security"]["auth"]["enforcedType"], "gemini-api-key");
    assert_eq!(s["hooksConfig"]["enabled"], false);
    assert_eq!(s["telemetry"]["enabled"], false);
    assert_eq!(s["privacy"]["usageStatisticsEnabled"], false);
    assert_eq!(s["general"]["enableAutoUpdate"], false);
    assert_eq!(s["admin"]["mcp"]["enabled"], false);
    assert_eq!(s["admin"]["extensions"]["enabled"], false);
    assert_eq!(s["tools"]["sandbox"], false);
}

#[test]
fn subscription_recipe_carries_no_key_variable() {
    let (h, m) = container();
    let r = map(
        &h,
        &m,
        paths(),
        ConfinementMode::Container,
        GeminiAuth::Subscription,
    )
    .unwrap();
    for name in FORBIDDEN_ENVIRONMENT {
        assert!(!r.environment.contains_key(*name), "{name} set");
    }
    assert_eq!(
        system(&r)["security"]["auth"]["enforcedType"],
        "oauth-personal"
    );
    assert_eq!(r.environment["GEMINI_FORCE_FILE_STORAGE"], "true");
}

#[test]
fn native_mode_is_refused_even_when_declared() {
    let (h, m) = settings(aq_session_protocol::NATIVE_CONFINEMENT_CAPABILITY);
    assert!(map(&h, &m, paths(), ConfinementMode::Native, proxy()).is_err());
    let (h, m) = container();
    assert!(map(&h, &m, paths(), ConfinementMode::Native, proxy()).is_err());
}

#[test]
fn refuses_wrong_settings_capability_origin_and_paths() {
    let (h, m) = container();
    let mut bad = h.clone();
    bad.cli_version = Text::new("0.66.0").unwrap();
    assert!(map(&bad, &m, paths(), ConfinementMode::Container, proxy()).is_err());
    let mut bad = m.clone();
    bad.provider = Text::new("anthropic").unwrap();
    assert!(map(&h, &bad, paths(), ConfinementMode::Container, proxy()).is_err());
    let mut bad = m.clone();
    bad.model = Text::new("gemini 2.5; rm").unwrap();
    assert!(map(&h, &bad, paths(), ConfinementMode::Container, proxy()).is_err());
    for (origin, capability) in [
        ("http://127.0.0.1:4242", "AIzaSyRealLookingKey"),
        ("https://generativelanguage.googleapis.com", CAP),
        ("http://localhost:4242", CAP),
        ("http://127.0.0.1:4242/v1beta", CAP),
        ("http://127.0.0.1", CAP),
    ] {
        let auth = GeminiAuth::ApiKeyProxy { origin, capability };
        assert!(
            map(&h, &m, paths(), ConfinementMode::Container, auth).is_err(),
            "{origin} {capability}"
        );
    }
    let mut p = paths();
    p.private_home = Path::new("/work/.gemini-home");
    assert!(map(&h, &m, p, ConfinementMode::Container, proxy()).is_err());
    let mut p = paths();
    p.script = Path::new("/opt/aq/../tmp/gemini.js");
    assert!(map(&h, &m, p, ConfinementMode::Container, proxy()).is_err());
}
