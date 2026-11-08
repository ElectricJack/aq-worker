//! The context qualification measurement (spec 0007 §7.3): the procedure of aq-ms1's
//! `remeasure.sh` (aq-server `docs/implementation/MS1.md`, "Refreshing the context
//! measurement"). Three bounded Claude Code print-mode calls run in the node's sandbox
//! template with the runner's environment and settings. This module holds the pure
//! parts: the prompts, the in-sandbox script, the token file read, the leak check and the
//! submission. `aq-worker` runs the sandbox. aq-server validates the submission with the
//! checks of `ms1-profiled.py` `measured()`.
use anyhow::{Context, Result, bail, ensure};
use aq_handoff_note::holds_token;
use aq_protocol::{MeasuredCall, MeasurementSubmission};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::Path,
    time::Duration,
};
use zeroize::Zeroizing;

/// The prompts, byte for byte the `measure/*.txt` of aq-ms1. Their SHA-256 values are the
/// `prompt_sha256` rows of the `aq.ms1-counting` Definition, which the server checks.
pub const PROMPTS: [(&str, &[u8]); 3] = [
    (
        "minimal",
        include_bytes!("../../../packaging/measure/minimal.txt"),
    ),
    (
        "mixed",
        include_bytes!("../../../packaging/measure/mixed.txt"),
    ),
    (
        "adversarial",
        include_bytes!("../../../packaging/measure/adversarial.txt"),
    ),
];
/// Where the inputs go in the sandbox, and where the script writes its outputs.
pub const IN_DIR: &str = "/aq/in";
pub const OUT_DIR: &str = "/aq/out";
/// The fixed runner reads the token from here and deletes it before the first call.
pub const MATERIAL_PATH: &str = "/aq/private/material";
/// Destroy evidence for the measurement sandbox (`operator_released`).
pub const RELEASE: &str = "aq-swarm-measurement";
/// The sandbox carries a hard deadline this far ahead.
pub const SANDBOX_DEADLINE: Duration = Duration::from_secs(15 * 60);
/// The runner must end within this, polled every `RUNNER_POLL`.
pub const RUNNER_DEADLINE: Duration = Duration::from_secs(10 * 60);
pub const RUNNER_POLL: Duration = Duration::from_secs(2);
/// `--pids-limit 512`, one CPU, 2 GiB.
pub const CPU_MILLIS: u32 = 1000;
pub const MEMORY_BYTES: u64 = 2 * 1024 * 1024 * 1024;
pub const PIDS: u32 = 512;
/// Bound on each output file read back from the sandbox.
pub const MAX_OUTPUT_BYTES: u64 = 16 * 1024 * 1024;
const MAX_TOKEN_FILE_BYTES: u64 = 64 * 1024;
/// Claude's `settings.json`, as the runner writes it.
pub const SETTINGS_JSON: &str = r#"{"permissions":{"defaultMode":"bypassPermissions"},"sandbox":{"enabled":false},"disableAllHooks":true,"enableAllProjectMcpServers":false,"cleanupPeriodDays":1,"forceLoginMethod":"claudeai"}"#;

/// The files the script leaves in `OUT_DIR`: `observed_at_ms`, then `<call>.jsonl` and
/// `<call>.err` for each call.
pub fn output_names() -> Vec<String> {
    let mut names = vec!["observed_at_ms".to_owned()];
    for (call, _) in PROMPTS {
        names.push(format!("{call}.jsonl"));
        names.push(format!("{call}.err"));
    }
    names
}

fn plain(value: &str, extra: &[char]) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || extra.contains(&c))
}

/// `/aq/in/run.sh`: `remeasure.sh`'s `run.sh` with only its paths changed (inputs in
/// `/aq/in`, outputs in `/aq/out`) and the token read from the private material file
/// instead of stdin. The token becomes `CLAUDE_CODE_OAUTH_TOKEN` of each claude process
/// only: it is never exported and never in argv.
pub fn run_script(claude_executable: &str, model: &str) -> Result<String> {
    ensure!(
        claude_executable.starts_with('/') && plain(claude_executable, &['/', '.', '_', '-']),
        "the Claude executable path {claude_executable:?} is not a plain absolute path"
    );
    ensure!(
        plain(model, &['.', '_', ':', '-']),
        "the measurement model {model:?} is not a plain model name"
    );
    let mut calls = String::new();
    for (call, _) in PROMPTS {
        calls.push(' ');
        calls.push_str(call);
    }
    Ok(format!(
        r#"set -uo pipefail
m={MATERIAL_PATH}
[ -s "$m" ] || {{ echo "no token material" >&2; exit 1; }}
token=$(cat "$m"); rm -f "$m"
[ -e "$m" ] && {{ echo "token material not wiped" >&2; exit 1; }}
[ -n "$token" ] || {{ echo "no token material" >&2; exit 1; }}
export HOME=/home/agent/.aq/claude CLAUDE_CONFIG_DIR=/home/agent/.aq/claude DISABLE_AUTOUPDATER=1 DISABLE_TELEMETRY=1 DISABLE_ERROR_REPORTING=1 CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 CLAUDE_CODE_DISABLE_AUTO_MEMORY=1 PATH=/opt/aq/bin:/usr/local/bin:/usr/bin:/bin
mkdir -p "$HOME" /aq/work && cd /aq/work
echo '{SETTINGS_JSON}' > "$HOME/settings.json"
date +%s%3N > {OUT_DIR}/observed_at_ms
for c in{calls}; do
  CLAUDE_CODE_OAUTH_TOKEN="$token" IS_SANDBOX=1 {claude_executable} -p --output-format stream-json --verbose --input-format text --model {model} --permission-mode bypassPermissions --setting-sources user --settings "$HOME/settings.json" --strict-mcp-config --no-session-persistence --disable-slash-commands < {IN_DIR}/$c.txt > {OUT_DIR}/$c.jsonl 2>{OUT_DIR}/$c.err; echo "$c rc=$?"
done
"#
    ))
}

/// Reads the measurement token file: a regular file (no symlink) of this user that no
/// other user can read. The bytes stay in zeroized memory.
pub fn read_token_file(path: &Path) -> Result<Zeroizing<Vec<u8>>> {
    let shown = path.display();
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .with_context(|| format!("measurement token file {shown} unavailable"))?;
    let meta = file.metadata()?;
    ensure!(
        meta.is_file()
            && meta.mode() & 0o077 == 0
            && meta.uid() == unsafe { libc::geteuid() }
            && meta.len() <= MAX_TOKEN_FILE_BYTES,
        "measurement token file {shown} must be a regular 0600 file of this user"
    );
    let mut bytes = Zeroizing::new(Vec::with_capacity(meta.len() as usize));
    file.take(MAX_TOKEN_FILE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_TOKEN_FILE_BYTES,
        "measurement token file {shown} grew"
    );
    Ok(bytes)
}

/// The first output file holding the token's exact bytes. Nothing from such a run is kept.
pub fn leaked<'a>(files: &'a BTreeMap<String, Vec<u8>>, token: &[u8]) -> Option<&'a str> {
    files
        .iter()
        .find(|(name, data)| holds_token(data, token) || holds_token(name.as_bytes(), token))
        .map(|(name, _)| name.as_str())
}

/// The measured outcome: the submission, the sandbox's `observed_at_ms` and the
/// `(call, total input tokens, output tokens)` of each call, for the operator.
#[derive(Debug)]
pub struct Measured {
    pub submission: MeasurementSubmission,
    pub observed_at_ms: u64,
    pub capacity_tokens: u64,
    pub totals: Vec<(String, u64, u64)>,
}

/// Builds the submission from the last `{"type":"result"}` event of each call. The local
/// checks are those of `measured()` that need no server fact: success, exactly the given
/// model, one context window for all calls. The server also checks the counting bound.
/// Only call this on files `leaked` cleared.
pub fn submission(files: &BTreeMap<String, Vec<u8>>, model: &str) -> Result<Measured> {
    let file = |name: &str| {
        files
            .get(name)
            .with_context(|| format!("measurement output {name} missing"))
    };
    let observed_at_ms: u64 = std::str::from_utf8(file("observed_at_ms")?)
        .ok()
        .and_then(|t| t.trim().parse().ok())
        .context("measurement observed_at_ms is not a number")?;
    let mut calls = Vec::new();
    let mut totals = Vec::new();
    let mut windows = std::collections::BTreeSet::new();
    for (call, prompt) in PROMPTS {
        let stderr = || {
            let err = files.get(&format!("{call}.err")).map_or(&[][..], |e| e);
            let head = String::from_utf8_lossy(&err[..err.len().min(2000)]).into_owned();
            if head.trim().is_empty() {
                String::new()
            } else {
                format!("; stderr: {head}")
            }
        };
        let text = std::str::from_utf8(file(&format!("{call}.jsonl"))?)
            .with_context(|| format!("measurement {call} output is not UTF-8"))?;
        let mut result = None;
        for line in text.lines() {
            let event: Value = serde_json::from_str(line)
                .with_context(|| format!("measurement {call} output is not stream-json"))?;
            if event["type"] == "result" {
                result = Some(event);
            }
        }
        let Some(result) = result.filter(|r| r["subtype"] == "success" && r["is_error"] == false)
        else {
            bail!("measurement {call} incomplete{}", stderr());
        };
        let used: Vec<String> = result["modelUsage"]
            .as_object()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        ensure!(
            used == [model],
            "measurement {call} is not for {model} (used {used:?})"
        );
        let window = result["modelUsage"][model]["contextWindow"]
            .as_u64()
            .filter(|w| (1..=16_777_216).contains(w))
            .with_context(|| format!("measurement {call} has no context window"))?;
        windows.insert(window);
        let usage = |key: &str| {
            result["usage"][key]
                .as_u64()
                .with_context(|| format!("measurement {call} usage has no {key}"))
        };
        let total = usage("input_tokens")?
            + usage("cache_creation_input_tokens")?
            + usage("cache_read_input_tokens")?;
        totals.push((call.to_owned(), total, usage("output_tokens")?));
        calls.push(MeasuredCall {
            call: call.to_owned(),
            prompt_utf8_bytes: prompt.len() as u64,
            prompt_sha256: format!("{:x}", Sha256::digest(prompt)),
            result,
        });
    }
    ensure!(
        windows.len() == 1,
        "measured context window is not a single value"
    );
    Ok(Measured {
        submission: MeasurementSubmission {
            schema_version: 1,
            calls,
        },
        observed_at_ms,
        capacity_tokens: windows.into_iter().next().unwrap_or_default(),
        totals,
    })
}

/// `--output`: writes `<dir>/measure/` as `remeasure.sh` leaves its work directory, so
/// `ms1-profiled.py` `measured()` can validate it with `AQ_MS1_HOME=<dir>`: the prompts,
/// the `.jsonl` and `.err` files and `observed_at_ms`. Directories 0700, files 0600;
/// an existing `measure` directory is never overwritten.
pub fn write_output(dir: &Path, files: &BTreeMap<String, Vec<u8>>) -> Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("create {}", dir.display()))?;
    let measure = dir.join("measure");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&measure)
        .with_context(|| format!("create {} (it must not exist yet)", measure.display()))?;
    let prompts = PROMPTS.map(|(call, prompt)| (format!("{call}.txt"), prompt));
    let outputs = files.iter().map(|(name, data)| (name.clone(), &data[..]));
    for (name, data) in prompts.into_iter().chain(outputs) {
        ensure!(
            plain(&name, &['.', '_', '-']),
            "output name {name:?} refused"
        );
        let path = measure.join(&name);
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .and_then(|mut f| f.write_all(data))
            .with_context(|| format!("write {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;

    fn result(model: &str, window: u64, input: u64) -> Value {
        json!({"type": "result", "subtype": "success", "is_error": false, "session_id": "s",
            "usage": {"input_tokens": input, "cache_creation_input_tokens": 2025,
                      "cache_read_input_tokens": 16800, "output_tokens": 4},
            "modelUsage": {model: {"contextWindow": window, "inputTokens": input}}})
    }
    fn outputs(results: [Value; 3]) -> BTreeMap<String, Vec<u8>> {
        let mut files =
            BTreeMap::from([("observed_at_ms".to_owned(), b"1794089390110\n".to_vec())]);
        for ((call, _), result) in PROMPTS.iter().zip(results) {
            let early =
                json!({"type": "result", "subtype": "error_during_execution", "is_error": true});
            let lines = [json!({"type": "system", "subtype": "init"}), early, result];
            let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
            files.insert(format!("{call}.jsonl"), text.into_bytes());
            files.insert(format!("{call}.err"), Vec::new());
        }
        files
    }
    const MODEL: &str = "claude-sonnet-5-5";
    fn good() -> [Value; 3] {
        [2, 700, 1500].map(|i| result(MODEL, 1_000_000, i))
    }

    /// The `prompt_sha256` and `prompt_utf8_bytes` rows of the active `aq.ms1-counting`
    /// Definition (revision 17, digest eeeeafa8…).
    #[test]
    fn prompts_match_the_counting_definition() {
        let expected = [
            (
                "minimal",
                67,
                "a78f9c326a23fcfcdf6dc6b5ab820b3e9f3cf15aa0dffd43c61f4ed43d6d1c4f",
            ),
            (
                "mixed",
                2738,
                "32fc7efa5bd4826a4736849039c72bc192990f537082632983c663e816252087",
            ),
            (
                "adversarial",
                5844,
                "02ad9cd0c86ee535c80b2218554470b53a953181911ce8dd0c38a0f0b1bcae0e",
            ),
        ];
        for ((call, prompt), (name, bytes, sha)) in PROMPTS.iter().zip(expected) {
            assert_eq!((*call, prompt.len()), (name, bytes));
            assert_eq!(format!("{:x}", Sha256::digest(prompt)), sha, "{call}");
        }
    }

    #[test]
    fn run_script_is_the_remeasure_procedure() {
        let s = run_script("/opt/aq/bin/claude", MODEL).unwrap();
        let call = r#"  CLAUDE_CODE_OAUTH_TOKEN="$token" IS_SANDBOX=1 /opt/aq/bin/claude -p --output-format stream-json --verbose --input-format text --model claude-sonnet-5-5 --permission-mode bypassPermissions --setting-sources user --settings "$HOME/settings.json" --strict-mcp-config --no-session-persistence --disable-slash-commands < /aq/in/$c.txt > /aq/out/$c.jsonl 2>/aq/out/$c.err; echo "$c rc=$?""#;
        for line in [
            "export HOME=/home/agent/.aq/claude CLAUDE_CONFIG_DIR=/home/agent/.aq/claude DISABLE_AUTOUPDATER=1 DISABLE_TELEMETRY=1 DISABLE_ERROR_REPORTING=1 CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 CLAUDE_CODE_DISABLE_AUTO_MEMORY=1 PATH=/opt/aq/bin:/usr/local/bin:/usr/bin:/bin",
            r#"mkdir -p "$HOME" /aq/work && cd /aq/work"#,
            r#"echo '{"permissions":{"defaultMode":"bypassPermissions"},"sandbox":{"enabled":false},"disableAllHooks":true,"enableAllProjectMcpServers":false,"cleanupPeriodDays":1,"forceLoginMethod":"claudeai"}' > "$HOME/settings.json""#,
            "date +%s%3N > /aq/out/observed_at_ms",
            "for c in minimal mixed adversarial; do",
            call,
        ] {
            assert!(s.lines().any(|l| l == line), "missing line: {line}");
        }
        // The material is read and deleted before the first call; the token is never
        // exported, so it reaches only each claude process.
        let wipe = s.find(r#"token=$(cat "$m"); rm -f "$m""#).unwrap();
        assert!(wipe < s.find("for c in").unwrap());
        assert!(s.contains("m=/aq/private/material"));
        assert!(!s.contains("export CLAUDE_CODE_OAUTH_TOKEN") && !s.contains("export token"));
        for (exe, model) in [
            ("/opt/aq/bin/claude", "x; rm -rf /"),
            ("/opt/aq/bin/claude", "a b"),
            ("/opt/aq/bin/claude", ""),
            ("claude", MODEL),
            ("/opt/a q/claude", MODEL),
        ] {
            assert!(run_script(exe, model).is_err(), "{exe} {model}");
        }
    }

    #[test]
    fn submission_takes_the_last_result_event_of_each_call() {
        let m = submission(&outputs(good()), MODEL).unwrap();
        assert_eq!(m.observed_at_ms, 1_794_089_390_110);
        assert_eq!(m.capacity_tokens, 1_000_000);
        assert_eq!(m.submission.schema_version, 1);
        let calls: Vec<_> = m.submission.calls.iter().map(|c| c.call.as_str()).collect();
        assert_eq!(calls, ["minimal", "mixed", "adversarial"]);
        let first = &m.submission.calls[0];
        assert_eq!(first.prompt_utf8_bytes, 67);
        assert_eq!(
            first.prompt_sha256,
            "a78f9c326a23fcfcdf6dc6b5ab820b3e9f3cf15aa0dffd43c61f4ed43d6d1c4f"
        );
        assert_eq!(first.result, result(MODEL, 1_000_000, 2));
        assert_eq!(m.totals[0], ("minimal".to_owned(), 2 + 2025 + 16800, 4));
    }

    #[test]
    fn submission_refuses_incomplete_other_model_or_split_window() {
        let mut failed = good();
        failed[1]["subtype"] = json!("error_max_turns");
        let mut errored = good();
        errored[2]["is_error"] = json!(true);
        let mut other = good();
        other[0] = result("claude-opus-5-5", 1_000_000, 2);
        let mut two = good();
        two[0]["modelUsage"]["claude-haiku-5"] = json!({"contextWindow": 200_000});
        let mut split = good();
        split[2] = result(MODEL, 200_000, 2);
        for (results, why) in [
            (failed, "incomplete"),
            (errored, "incomplete"),
            (other, "is not for"),
            (two, "is not for"),
            (split, "single value"),
        ] {
            let e = submission(&outputs(results), MODEL)
                .unwrap_err()
                .to_string();
            assert!(e.contains(why), "{e}");
        }
        let mut files = outputs(good());
        files.insert("mixed.jsonl".into(), b"{\"type\":\"system\"}\n".to_vec());
        files.insert("mixed.err".into(), b"Invalid API key\n".to_vec());
        let e = submission(&files, MODEL).unwrap_err().to_string();
        assert!(
            e.contains("mixed incomplete; stderr: Invalid API key"),
            "{e}"
        );
        files.remove("observed_at_ms");
        assert!(submission(&files, MODEL).is_err());
    }

    #[test]
    fn a_token_in_any_output_is_found() {
        let token = b"sk-ant-oat01-fixture-0123456789abcdef";
        let mut files = outputs(good());
        assert_eq!(leaked(&files, token), None);
        let mut err = b"auth: ".to_vec();
        err.extend_from_slice(token);
        files.insert("adversarial.err".into(), err);
        assert_eq!(leaked(&files, token), Some("adversarial.err"));
    }

    #[test]
    fn token_file_must_be_private_and_regular() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        std::fs::write(&path, b"fixture-token-0123456789\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_token_file(&path).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            &read_token_file(&path).unwrap()[..],
            b"fixture-token-0123456789\n"
        );
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(read_token_file(&link).is_err());
    }

    #[test]
    fn output_is_written_for_the_validator_and_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("w");
        let files = outputs(good());
        write_output(&out, &files).unwrap();
        let measure = out.join("measure");
        for name in output_names()
            .into_iter()
            .chain(PROMPTS.map(|(c, _)| format!("{c}.txt")))
        {
            let meta = std::fs::metadata(measure.join(&name)).unwrap();
            assert_eq!(meta.permissions().mode() & 0o777, 0o600, "{name}");
        }
        assert_eq!(
            std::fs::read(measure.join("mixed.txt")).unwrap(),
            PROMPTS[1].1
        );
        assert!(write_output(&out, &files).is_err());
    }
}
