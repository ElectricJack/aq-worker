//! Non-secret launch inputs sent to the fixed source-only sandbox runner.
use crate::{build_cache::SlotCache, projects, tar::TreeEntry, task_postgres as postgres};
use anyhow::{Result, ensure};
use aq_handoff_note::harness::{CHANNEL_NOTE, DriverHarness, RUNNER_NOTE};
use std::{collections::BTreeMap, path::Path};

pub struct LaunchInputs<'a> {
    pub arguments: &'a [String],
    pub environment: &'a BTreeMap<String, String>,
    pub forbidden_environment: &'a [&'a str],
    pub codex_runtime_bundle: Option<&'a Path>,
    pub build_cache: Option<&'a SlotCache>,
    pub harness: DriverHarness,
    pub workspace: &'a Path,
    pub settings_json: &'a str,
    pub executable: &'a str,
    pub agent_channel: bool,
    pub postgres: bool,
}

/// The non-secret runner inputs under `/aq/in`.
pub fn entries(config: &LaunchInputs<'_>, prompt: &[u8]) -> Result<Vec<TreeEntry>> {
    let mut args = config.arguments.to_vec();
    if config.postgres && !config.harness.is_claude() {
        aq_codex_mapper::apply_tool_environment(
            &mut args,
            &mut BTreeMap::new(),
            postgres::ENV.into_iter(),
        )?;
    }
    for a in &args {
        ensure!(!a.contains('\n') && !a.contains('\0'), "argument shape");
    }
    let env = input_environment(config)?;
    let file = |path: &str, data: Vec<u8>| TreeEntry::File {
        path: path.into(),
        mode: 0o600,
        data,
    };
    // The startup context tells the agent to run the AQ result client; inside
    // the sandbox the node driver submits the claim from the final message.
    let mut prompt = prompt.to_vec();
    prompt.extend(
        aq_node_git::conflict_note(config.workspace)
            .unwrap_or_default()
            .bytes(),
    );
    prompt.extend_from_slice(projects::note(config.workspace)?.as_bytes());
    prompt.extend_from_slice(RUNNER_NOTE.as_bytes());
    if config.postgres {
        prompt.extend_from_slice(postgres::NOTE.as_bytes());
    }
    if config.agent_channel {
        prompt.extend_from_slice(CHANNEL_NOTE.as_bytes());
    }
    let bundle = config.codex_runtime_bundle;
    let mut entries = bundle
        .map(crate::codex_runtime::bundle)
        .transpose()?
        .unwrap_or_default();
    entries.extend([
        file("prompt", prompt),
        file("args", (args.join("\n") + "\n").into_bytes()),
        file("env", env.into_bytes()),
        file("settings.json", config.settings_json.as_bytes().to_vec()),
        file("executable", config.executable.as_bytes().to_vec()),
    ]);
    Ok(entries)
}

fn input_environment(config: &LaunchInputs<'_>) -> Result<String> {
    let mut env = String::new();
    for (k, v) in config.environment {
        ensure!(
            !config.forbidden_environment.contains(&k.as_str())
                && !k.contains('=')
                && !k.is_empty()
                && !v.contains('\n'),
            "Claude environment refused"
        );
        let v = match (config.agent_channel, k.as_str()) {
            (true, "PATH") => aq_agent_channel::with_aq_bin(v),
            _ => v.clone(),
        };
        env.push_str(&format!("{k}={v}\n"));
    }
    if config.agent_channel {
        env.push_str(&format!("AQ_CHANNEL={}\n", aq_agent_channel::AQ_CHANNEL));
    }
    for (name, target) in config.build_cache.iter().flat_map(|c| c.environment()) {
        env.push_str(&format!("{name}={target}\n"));
    }
    if config.postgres {
        for (name, value) in postgres::ENV {
            env.push_str(&format!("{name}={value}\n"));
        }
    }
    Ok(env)
}
