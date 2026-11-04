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
            harness: Text::new("claude").unwrap(),
            cli_version: Text::new(CLAUDE_CLI_VERSION).unwrap(),
            adapter: artifact.clone(),
            environment: artifact,
            required_capabilities: caps,
        },
        ModelSettings {
            provider: Text::new("anthropic").unwrap(),
            model: Text::new("claude-opus-5").unwrap(),
            reasoning: Text::new("high").unwrap(),
            max_output_tokens: 4096,
            spending_budget: None,
        },
    )
}
fn paths() -> ClaudePaths<'static> {
    ClaudePaths {
        executable: Path::new("/opt/aq/bin/claude"),
        workspace: Path::new("/work"),
        private_home: Path::new("/home/agent/.aq/claude"),
    }
}
fn flag<'a>(recipe: &'a ClaudeRecipe, name: &str) -> &'a str {
    let i = recipe.arguments.iter().position(|a| a == name).unwrap();
    assert_eq!(recipe.arguments.iter().filter(|a| *a == name).count(), 1);
    &recipe.arguments[i + 1]
}
fn settings_json(recipe: &ClaudeRecipe) -> serde_json::Value {
    serde_json::from_slice(&recipe.settings_json).unwrap()
}

#[test]
fn native_claude_recipe_uses_harness_sandbox_and_accept_edits() {
    let (harness, model) = settings(aq_session_protocol::NATIVE_CONFINEMENT_CAPABILITY);
    let recipe = map(&harness, &model, paths(), ConfinementMode::Native).unwrap();
    assert_eq!(recipe.executable, Path::new("/opt/aq/bin/claude"));
    assert_eq!(recipe.working_directory, Path::new("/work"));
    assert_eq!(recipe.arguments[0], "-p");
    assert_eq!(flag(&recipe, "--output-format"), "stream-json");
    assert_eq!(flag(&recipe, "--input-format"), "text");
    assert_eq!(flag(&recipe, "--model"), "claude-opus-5");
    assert_eq!(flag(&recipe, "--permission-mode"), "acceptEdits");
    assert_eq!(flag(&recipe, "--setting-sources"), "user");
    assert_eq!(
        flag(&recipe, "--settings"),
        "/home/agent/.aq/claude/settings.json"
    );
    for f in [
        "--verbose",
        "--strict-mcp-config",
        "--no-session-persistence",
        "--disable-slash-commands",
    ] {
        assert!(recipe.arguments.iter().any(|a| a == f), "missing {f}");
    }
    assert!(
        !recipe
            .arguments
            .iter()
            .any(|a| a == "--dangerously-skip-permissions")
    );
    let s = settings_json(&recipe);
    assert_eq!(s["sandbox"]["enabled"], true);
    assert_eq!(s["sandbox"]["allowUnsandboxedCommands"], false);
    assert_eq!(s["sandbox"]["autoAllowBashIfSandboxed"], true);
    assert!(s["sandbox"].get("enableWeakerNestedSandbox").is_none());
    assert_eq!(s["permissions"]["defaultMode"], "acceptEdits");
    assert_eq!(s["permissions"]["disableBypassPermissionsMode"], "disable");
    assert_eq!(s["disableAllHooks"], true);
    assert_eq!(s["enableAllProjectMcpServers"], false);
    assert_eq!(s["forceLoginMethod"], "claudeai");
    assert!(s.get("apiKeyHelper").is_none());
}

#[test]
fn container_claude_recipe_bypasses_permissions_with_harness_sandbox_off() {
    let (harness, model) = settings(aq_session_protocol::CONTAINER_CONFINEMENT_CAPABILITY);
    let recipe = map(&harness, &model, paths(), ConfinementMode::Container).unwrap();
    assert_eq!(flag(&recipe, "--permission-mode"), "bypassPermissions");
    let s = settings_json(&recipe);
    assert_eq!(s["sandbox"]["enabled"], false);
    assert_eq!(s["permissions"]["defaultMode"], "bypassPermissions");
    assert!(
        s["permissions"]
            .get("disableBypassPermissionsMode")
            .is_none()
    );
    // The mode must match the manifest's declared capability in both directions.
    assert!(map(&harness, &model, paths(), ConfinementMode::Native).is_err());
    let (native, _) = settings(aq_session_protocol::NATIVE_CONFINEMENT_CAPABILITY);
    assert!(map(&native, &model, paths(), ConfinementMode::Container).is_err());
}

#[test]
fn claude_recipe_carries_subscription_only_environment() {
    let (harness, model) = settings(aq_session_protocol::NATIVE_CONFINEMENT_CAPABILITY);
    let recipe = map(&harness, &model, paths(), ConfinementMode::Native).unwrap();
    let keys: Vec<_> = recipe.environment.keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "CLAUDE_CODE_DISABLE_AUTO_MEMORY",
            "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC",
            "CLAUDE_CONFIG_DIR",
            "DISABLE_AUTOUPDATER",
            "DISABLE_ERROR_REPORTING",
            "DISABLE_TELEMETRY",
            "HOME",
            "PATH",
        ]
    );
    assert_eq!(recipe.environment["HOME"], "/home/agent/.aq/claude");
    assert_eq!(
        recipe.environment["CLAUDE_CONFIG_DIR"],
        "/home/agent/.aq/claude"
    );
    for forbidden in FORBIDDEN_ENVIRONMENT {
        assert!(!recipe.environment.contains_key(*forbidden));
    }
    assert!(
        FORBIDDEN_ENVIRONMENT.contains(&"ANTHROPIC_API_KEY")
            && FORBIDDEN_ENVIRONMENT.contains(&"ANTHROPIC_AUTH_TOKEN")
    );
}

#[test]
fn claude_recipe_refuses_unsupported_settings_and_paths() {
    let (harness, model) = settings(aq_session_protocol::NATIVE_CONFINEMENT_CAPABILITY);
    let mode = ConfinementMode::Native;
    let mut other = harness.clone();
    other.harness = Text::new("codex").unwrap();
    assert!(map(&other, &model, paths(), mode).is_err());
    let mut version = harness.clone();
    version.cli_version = Text::new("2.1.3").unwrap();
    assert!(map(&version, &model, paths(), mode).is_err());
    let mut provider = model.clone();
    provider.provider = Text::new("openai").unwrap();
    assert!(map(&harness, &provider, paths(), mode).is_err());
    let mut budget = model.clone();
    budget.spending_budget = Some(aq_session_protocol::SpendingBudget {
        currency: Text::new("USD").unwrap(),
        basis: aq_session_protocol::BudgetBasis::TaskCumulativeEstimatedCost,
        limit_microunits: 1,
    });
    assert!(map(&harness, &budget, paths(), mode).is_err());
    for (workspace, home) in [
        ("relative", "/home/agent/.aq/claude"),
        ("/work", "relative"),
        ("/work", "/work/.claude"),
        ("/work", "/"),
    ] {
        let p = ClaudePaths {
            executable: Path::new("/opt/aq/bin/claude"),
            workspace: Path::new(workspace),
            private_home: Path::new(home),
        };
        assert!(
            map(&harness, &model, p, mode).is_err(),
            "{workspace} {home}"
        );
    }
}
