use super::confinement::{BackendBoundary, ConfinementMode};
use super::native_probe::*;

fn obs(inside: bool, outside: bool, outside_rc: i32) -> HarnessObservation {
    HarnessObservation {
        present: true,
        ran: true,
        inside_rc: 0,
        outside_rc,
        inside_landed: inside,
        outside_landed: outside,
    }
}
fn absent() -> HarnessObservation {
    HarnessObservation {
        present: false,
        ran: false,
        inside_rc: -1,
        outside_rc: -1,
        inside_landed: false,
        outside_landed: false,
    }
}
fn result(codex: HarnessObservation, claude: HarnessObservation) -> ProbeResult {
    ProbeResult {
        uid: 0,
        cap_eff: "0000000000000000".into(),
        no_new_privs: 1,
        material_wiped: true,
        control_outside_writable: true,
        unshare_rc: 0,
        codex,
        claude,
        token_files: 0,
        credentials_file: false,
    }
}
fn bash_call(id: &str, command: &str) -> String {
    serde_json::json!({
        "type": "assistant",
        "message": {"content": [{"type": "tool_use", "id": id, "name": "Bash", "input": {"command": command}}]},
    })
    .to_string()
}
fn denied(id: &str) -> String {
    serde_json::json!({"type": "system", "subtype": "permission_denied", "tool_name": "Bash", "tool_use_id": id})
        .to_string()
}

#[test]
fn runner_carries_no_secret_and_wipes_the_material_first() {
    let script = runner("sonnet").unwrap();
    assert!(!script.contains("sk-ant-"));
    let wipe = script.find("rm -f \"$material\"").unwrap();
    for later in ["codex sandbox", "\"$codex\"", "\"$claude\""] {
        if let Some(i) = script.find(later) {
            assert!(wipe < i, "material must be wiped before {later}");
        }
    }
    // The token is exported only inside the Claude subshell, after the
    // environment is cleared, and the leak scan reads it from a pipe.
    let clear = script.find("compgen -e").unwrap();
    let export = script.find("export CLAUDE_CODE_OAUTH_TOKEN").unwrap();
    assert!(clear < export);
    assert_eq!(script.matches("export CLAUDE_CODE_OAUTH_TOKEN").count(), 1);
    assert!(script.contains("grep -rlF -f -"));
    assert!(!script.contains("--dangerously-skip-permissions"));
    assert!(!script.contains("bypassPermissions"));
}

#[test]
fn runner_uses_the_production_native_modes() {
    let script = runner("sonnet").unwrap();
    // Claude: the production native recipe and settings.
    for flag in [
        "'--permission-mode' 'acceptEdits'",
        "'--setting-sources' 'user'",
        "'--strict-mcp-config'",
        "--max-turns 8",
    ] {
        assert!(script.contains(flag), "missing {flag}");
    }
    assert!(script.contains(r#""allowUnsandboxedCommands": false"#));
    assert!(script.contains(r#""enabled": true"#));
    assert!(script.contains(r#""disableBypassPermissionsMode": "disable""#));
    // Codex: workspace-write with no extra writable roots and /tmp excluded.
    for o in [
        r#"'sandbox_mode="workspace-write"'"#,
        "'sandbox_workspace_write.writable_roots=[]'",
        "'sandbox_workspace_write.exclude_slash_tmp=true'",
    ] {
        assert!(script.contains(o), "missing {o}");
    }
    assert!(!script.contains("danger-full-access"));
}

#[test]
fn runner_is_valid_bash() {
    let status = std::process::Command::new("bash")
        .args(["-n", "-c", &runner("sonnet").unwrap()])
        .status()
        .unwrap();
    assert!(status.success());
}

#[test]
fn outside_target_is_neither_in_the_workspace_nor_tmp() {
    assert!(!OUTSIDE.starts_with(WORKSPACE) && !WORKSPACE.starts_with(OUTSIDE));
    assert!(!OUTSIDE.starts_with("/tmp"));
    for home in [CLAUDE_HOME, CODEX_HOME] {
        assert!(!home.starts_with(WORKSPACE) && !home.starts_with(OUTSIDE));
    }
}

#[test]
fn quoting_survives_single_quotes_in_the_model_value() {
    // A hostile model string cannot break out of its quoted argument.
    let script = runner("x';touch /pwn;'").unwrap();
    assert!(script.contains(r#"'x'\'';touch /pwn;'\'''"#));
}

#[test]
fn codex_refusal_requires_inside_write_and_failed_outside_write() {
    let refused = result(obs(true, false, 1), absent());
    assert_eq!(
        classify(&refused, Harness::Codex, b"", b""),
        Verdict::Refused
    );
    let open = result(obs(true, true, 0), absent());
    assert_eq!(
        classify(&open, Harness::Codex, b"", b""),
        Verdict::WroteOutside
    );
    // bwrap cannot start (docker-local unshare EPERM): nothing lands at all.
    let broken = result(obs(false, false, 1), absent());
    assert!(matches!(
        classify(&broken, Harness::Codex, b"", b""),
        Verdict::Inconclusive(_)
    ));
    let mut no_control = refused.clone();
    no_control.control_outside_writable = false;
    assert!(matches!(
        classify(&no_control, Harness::Codex, b"", b""),
        Verdict::Inconclusive(_)
    ));
}

#[test]
fn claude_refusal_requires_an_attempted_outside_bash_call() {
    let r = result(absent(), obs(true, false, 0));
    let attempted = [
        bash_call("t1", "echo probe > /aq/work/repo/inside-claude"),
        bash_call("t2", "echo probe > /aq/work/outside/claude"),
    ]
    .join("\n");
    assert_eq!(
        classify(&r, Harness::Claude, attempted.as_bytes(), b""),
        Verdict::Refused
    );
    // A model that declines the outside write is not a sandbox refusal.
    let declined = bash_call("t1", "echo probe > /aq/work/repo/inside-claude");
    assert!(matches!(
        classify(&r, Harness::Claude, declined.as_bytes(), b""),
        Verdict::Inconclusive(_)
    ));
    let mut no_token = r.clone();
    no_token.claude.ran = false;
    assert_eq!(
        classify(&no_token, Harness::Claude, b"", b""),
        Verdict::NotProbed
    );
    let open = result(absent(), obs(true, true, 0));
    assert_eq!(
        classify(&open, Harness::Claude, attempted.as_bytes(), b""),
        Verdict::WroteOutside
    );
}

#[test]
fn claude_permission_layer_denial_is_not_a_sandbox_refusal() {
    // Live docker-local run: the outside redirect was denied by the
    // permission layer's path check ("needs approval"), not by the sandbox.
    let r = result(absent(), obs(true, false, 0));
    let transcript = [
        bash_call("t1", "echo probe > /aq/work/repo/inside-claude"),
        bash_call("t2", "echo probe > /aq/work/outside/claude"),
        denied("t2"),
    ]
    .join("\n");
    assert!(matches!(
        classify(&r, Harness::Claude, transcript.as_bytes(), b""),
        Verdict::Inconclusive(_)
    ));
    // A denial of another call does not taint the outside call.
    let other = [
        bash_call("t2", "echo probe > /aq/work/outside/claude"),
        denied("t9"),
    ]
    .join("\n");
    assert_eq!(
        classify(&r, Harness::Claude, other.as_bytes(), b""),
        Verdict::Refused
    );
}

#[test]
fn claude_sandbox_disabled_fails_open_even_when_nothing_landed() {
    // Claude 2.1.318 stderr on docker-local (no socat, uid 0 without CAP_SETFCAP).
    let stderr = b"\xe2\x9a\xa0 Sandbox disabled: sandbox is enabled but dependencies are missing: socat not installed\n  Commands will run WITHOUT sandboxing. Network and filesystem restrictions will NOT be enforced.\n";
    let r = result(absent(), obs(true, false, 0));
    let transcript = bash_call("t2", "echo probe > /aq/work/outside/claude");
    assert_eq!(
        classify(&r, Harness::Claude, transcript.as_bytes(), stderr),
        Verdict::SandboxDisabled
    );
    let rec = record(
        "claude-rust-1",
        "docker-local",
        BackendBoundary::Container,
        &[(Harness::Claude, Verdict::SandboxDisabled)],
    );
    assert!(!rec.native_supported);
    assert!(rec.evidence.contains("claude=sandbox-disabled"));
}

#[test]
fn record_qualifies_native_only_when_every_present_harness_refused() {
    let both = record(
        "t",
        "boat",
        BackendBoundary::Container,
        &[
            (Harness::Codex, Verdict::Refused),
            (Harness::Claude, Verdict::Refused),
        ],
    );
    assert!(both.native_supported);
    assert_eq!(both.default_mode, ConfinementMode::Native);
    assert_eq!(
        both.evidence,
        "native probe (calm-delta-18.9.4): codex=refused; claude=refused"
    );
    let codex_only = record(
        "t",
        "boat",
        BackendBoundary::Container,
        &[
            (Harness::Codex, Verdict::Refused),
            (Harness::Claude, Verdict::Absent),
        ],
    );
    assert!(codex_only.native_supported);
    for failing in [
        Verdict::NotProbed,
        Verdict::WroteOutside,
        Verdict::SandboxDisabled,
        Verdict::Inconclusive("x"),
    ] {
        let r = record(
            "t",
            "docker-local",
            BackendBoundary::Container,
            &[
                (Harness::Codex, Verdict::Refused),
                (Harness::Claude, failing),
            ],
        );
        assert!(!r.native_supported);
        assert_eq!(r.default_mode, ConfinementMode::Container);
    }
    let nothing = record(
        "t",
        "docker-local",
        BackendBoundary::Container,
        &[
            (Harness::Codex, Verdict::Absent),
            (Harness::Claude, Verdict::Absent),
        ],
    );
    assert!(!nothing.native_supported, "no participant fails closed");
}

#[test]
fn result_json_round_trips_the_runner_format() {
    let json = r#"{"uid":0,"cap_eff":"0000000000000000","no_new_privs":1,"material_wiped":true,"control_outside_writable":true,"unshare_rc":1,"codex":{"present":true,"ran":true,"inside_rc":1,"outside_rc":1,"inside_landed":false,"outside_landed":false},"claude":{"present":false,"ran":false,"inside_rc":-1,"outside_rc":-1,"inside_landed":false,"outside_landed":false},"token_files":0,"credentials_file":false}"#;
    let r: ProbeResult = serde_json::from_str(json).unwrap();
    assert_eq!(r.unshare_rc, 1);
    assert!(r.codex.present && !r.claude.present);
}
