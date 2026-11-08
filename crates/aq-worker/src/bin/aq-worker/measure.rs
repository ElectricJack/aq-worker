//! The docker-local qualification measurement (spec 0007 §7.3): aq-ms1's `remeasure.sh`
//! procedure, in one sandbox of the node's Claude template. The pure parts (prompts, the
//! in-sandbox script, the submission) live in `aq_node_service::measure`.
use anyhow::{Context, Result, bail, ensure};
use aq_node_service::measure::{
    self as m, IN_DIR, MATERIAL_PATH, MAX_OUTPUT_BYTES, Measured, OUT_DIR, PROMPTS, RELEASE,
};
use aq_worker::{
    backend::{
        Confirmation, CreateRequest, DestroyAuthorization, Ownership, PlacementBackend,
        PrivateMaterial, ResourceLimits, RunnerExit, SandboxIdentity, TemplateDigest,
        docker_local::{
            DockerLocalBackend, DockerTransport,
            tar::{TreeEntry, single_file, tree_archive},
        },
    },
    native::runner::{ExecutorConfig, NativeConfig},
    sandbox_driver::parse_token,
};
use std::{collections::BTreeMap, path::Path};
use uuid::Uuid;
use zeroize::Zeroizing;

/// A finished measurement: the submission and the files it was built from (token-free).
pub struct Measurement {
    pub measured: Measured,
    pub files: BTreeMap<String, Vec<u8>>,
}

/// Runs the three calls in a fresh sandbox owned by `node_id` (slot 1) and builds the
/// submission. The token is read from the 0600 `token_file` and reaches the sandbox only
/// as private material; it is never in argv, a container's env or config, or a log.
pub async fn run(
    template: &Path,
    token_file: &Path,
    model: &str,
    node_id: Uuid,
) -> Result<Measurement> {
    let config = NativeConfig::load(template)?;
    let Some(ExecutorConfig::ClaudeDockerLocalResultLocal { settings, .. }) = &config.executor
    else {
        bail!("the measurement needs the Claude docker-local executor in the native template");
    };
    let host = &settings.host;
    ensure!(
        host.docker_local.material_path == MATERIAL_PATH,
        "the measurement runner reads its token from {MATERIAL_PATH}"
    );
    let exe = host
        .claude_executable
        .to_str()
        .context("the Claude executable path is not UTF-8")?;
    let script = m::run_script(exe, model)?;
    let token = parse_token(&m::read_token_file(token_file)?)?;
    let mut docker = host.docker_local.clone();
    docker.runner_command = Some(vec!["/bin/bash".into(), format!("{IN_DIR}/run.sh")]);
    let backend = DockerLocalBackend::connect_unix(docker).await?;
    run_on(backend, &host.template, &script, &token, model, node_id).await
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

pub(crate) async fn run_on<T: DockerTransport>(
    mut backend: DockerLocalBackend<T>,
    template: &TemplateDigest,
    script: &str,
    token: &Zeroizing<Vec<u8>>,
    model: &str,
    node_id: Uuid,
) -> Result<Measurement> {
    let owner = Ownership {
        node_id,
        boot_id: Uuid::new_v4(),
        attempt_id: Uuid::new_v4(),
    };
    let deadline = now_ms() + m::SANDBOX_DEADLINE.as_millis() as u64;
    let request = CreateRequest::new(owner, template.clone(), deadline, vec![]).with_resources(
        ResourceLimits {
            cpu_millis: m::CPU_MILLIS,
            memory_bytes: m::MEMORY_BYTES,
            pids: m::PIDS,
            disk_bytes: None,
        },
    );
    let sandbox = match backend.create(&request).await {
        Ok(s) => s,
        Err(_) => {
            backend.pull_template(template).await?;
            backend.create(&request).await?
        }
    };
    let driven = drive(&mut backend, &sandbox, script, token).await;
    let released = release(&mut backend, &sandbox).await;
    let files = driven?;
    released?;
    if let Some(name) = m::leaked(&files, token) {
        bail!("the Claude token appeared in the measurement output {name}; nothing is kept");
    }
    let measured = m::submission(&files, model)?;
    Ok(Measurement { measured, files })
}

async fn drive<T: DockerTransport>(
    backend: &mut DockerLocalBackend<T>,
    sandbox: &SandboxIdentity,
    script: &str,
    token: &Zeroizing<Vec<u8>>,
) -> Result<BTreeMap<String, Vec<u8>>> {
    let file = |path: String, data: &[u8]| TreeEntry::File {
        path,
        mode: 0o644,
        data: data.to_vec(),
    };
    let dir = |path: &str| TreeEntry::Dir {
        path: path.into(),
        mode: 0o755,
    };
    let mut inputs = vec![dir("in"), dir("out")];
    inputs.extend(PROMPTS.map(|(call, prompt)| file(format!("in/{call}.txt"), prompt)));
    inputs.push(file("in/run.sh".into(), script.as_bytes()));
    backend
        .put_archive(sandbox, tree_archive("aq", &inputs)?)
        .await?;
    backend
        .deliver_private_material(sandbox, PrivateMaterial::new(token.to_vec()))
        .await?;
    backend.start_runner(sandbox).await?;
    let until = tokio::time::Instant::now() + m::RUNNER_DEADLINE;
    let exit = loop {
        if let Ok(exit) = backend.runner_exit(sandbox).await {
            break exit;
        }
        ensure!(
            tokio::time::Instant::now() < until,
            "the measurement runner did not finish within {} s",
            m::RUNNER_DEADLINE.as_secs()
        );
        tokio::time::sleep(m::RUNNER_POLL).await;
    };
    ensure!(
        exit == RunnerExit::Exited(0),
        "the measurement runner ended with {exit:?}"
    );
    let mut files = BTreeMap::new();
    for name in m::output_names() {
        let archive = backend
            .get_archive(
                sandbox,
                &format!("{OUT_DIR}/{name}"),
                MAX_OUTPUT_BYTES as usize + 64 * 1024,
            )
            .await
            .with_context(|| format!("read the measurement output {name}"))?;
        files.insert(name, single_file(&archive, MAX_OUTPUT_BYTES)?);
    }
    Ok(files)
}

/// Stop, then destroy with operator-release evidence: a measurement retains nothing.
async fn release<T: DockerTransport>(
    backend: &mut DockerLocalBackend<T>,
    sandbox: &SandboxIdentity,
) -> Result<()> {
    let _ = backend.stop(sandbox).await;
    let authorization = DestroyAuthorization::operator_released(sandbox.clone(), RELEASE)?;
    match backend.destroy(sandbox, &authorization).await {
        Confirmation::Confirmed(_) => Ok(()),
        other => bail!(
            "measurement sandbox {} not destroyed: {other:?}",
            sandbox.backend_id
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docker_double::ScriptedDocker;
    use aq_worker::backend::docker_local::{
        AllowedTemplate, DockerLocalConfig, DockerMethod, DockerRequest, DockerResponse,
        TransportError,
    };
    use serde_json::{Value, json};
    use std::sync::{Arc, Mutex};

    const TOKEN: &[u8] = b"sk-ant-oat01-fixture-token-0123456789abcdef";
    const MODEL: &str = "claude-sonnet-5-5";

    /// One recorded request: method, path and body.
    type Recorded = (DockerMethod, String, Vec<u8>);
    /// The material and `run.sh` the container held when it started.
    type AtStart = Option<(Vec<u8>, Vec<u8>)>;

    /// The fake container: on start it checks what the runner would read, then writes
    /// what three Claude calls would leave in /aq/out. Every request is recorded.
    #[derive(Clone)]
    struct FakeClaude {
        docker: ScriptedDocker,
        requests: Arc<Mutex<Vec<Recorded>>>,
        outputs: Arc<BTreeMap<String, Vec<u8>>>,
        seen_at_start: Arc<Mutex<AtStart>>,
    }
    impl DockerTransport for FakeClaude {
        async fn send(&self, request: DockerRequest) -> Result<DockerResponse, TransportError> {
            let (method, path) = (request.method, request.path.clone());
            self.requests
                .lock()
                .unwrap()
                .push((method, path.clone(), request.body.to_vec()));
            let start = method == DockerMethod::Post && path.ends_with("/start");
            let name = path.split('/').nth(3).unwrap_or_default().to_owned();
            if start && let Some(c) = self.docker.container(&name) {
                let read = |p: &str| c.layer.get(p).map(|f| f.data.clone()).unwrap_or_default();
                *self.seen_at_start.lock().unwrap() =
                    Some((read("/aq/private/material"), read("/aq/in/run.sh")));
            }
            let response = self.docker.send(request).await?;
            if start {
                for (file, data) in self.outputs.iter() {
                    self.docker
                        .put_file(&name, &format!("/aq/out/{file}"), data.clone());
                }
            }
            Ok(response)
        }
    }

    fn outputs(extra_err: &[u8]) -> BTreeMap<String, Vec<u8>> {
        let mut files = BTreeMap::from([("observed_at_ms".into(), b"1794089390110\n".to_vec())]);
        for (call, _) in PROMPTS {
            let result = json!({"type": "result", "subtype": "success", "is_error": false,
                "usage": {"input_tokens": 2, "cache_creation_input_tokens": 2025,
                          "cache_read_input_tokens": 16800, "output_tokens": 4},
                "modelUsage": {MODEL: {"contextWindow": 1_000_000}}});
            let text = format!("{}\n{result}\n", json!({"type": "system"}));
            files.insert(format!("{call}.jsonl"), text.into_bytes());
            files.insert(format!("{call}.err"), extra_err.to_vec());
        }
        files
    }

    struct Rig {
        fake: FakeClaude,
        _dir: tempfile::TempDir,
        _listener: std::os::unix::net::UnixListener,
        backend: Option<DockerLocalBackend<FakeClaude>>,
        template: TemplateDigest,
    }
    async fn rig(outputs: BTreeMap<String, Vec<u8>>) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("sandbox.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let template = TemplateDigest::parse(format!("sha256:{}", "c".repeat(64))).unwrap();
        let mut config = DockerLocalConfig::new(socket);
        config.templates.push(AllowedTemplate {
            image: format!("registry.local/aq-claude@{}", template.as_str()),
            digest: template.clone(),
        });
        config.runner_command = Some(vec!["/bin/bash".into(), "/aq/in/run.sh".into()]);
        let fake = FakeClaude {
            docker: ScriptedDocker::new(),
            requests: Default::default(),
            outputs: Arc::new(outputs),
            seen_at_start: Default::default(),
        };
        let backend = DockerLocalBackend::connect(config, fake.clone())
            .await
            .unwrap();
        Rig {
            fake,
            _dir: dir,
            _listener: listener,
            backend: Some(backend),
            template,
        }
    }
    impl Rig {
        async fn run(&mut self) -> Result<Measurement> {
            let script = m::run_script("/opt/aq/bin/claude", MODEL).unwrap();
            let token = Zeroizing::new(TOKEN.to_vec());
            let backend = self.backend.take().unwrap();
            run_on(
                backend,
                &self.template,
                &script,
                &token,
                MODEL,
                Uuid::from_u128(1),
            )
            .await
        }
        fn requests(&self) -> Vec<Recorded> {
            self.fake.requests.lock().unwrap().clone()
        }
    }

    fn holds(haystack: &[u8]) -> bool {
        aq_handoff_note::holds_token(haystack, TOKEN)
    }

    #[tokio::test]
    async fn measurement_runs_three_calls_in_one_sandbox_and_destroys_it() {
        let mut rig = rig(outputs(b"")).await;
        let measurement = rig.run().await.unwrap();
        let calls: Vec<_> = (measurement.measured.submission.calls.iter())
            .map(|c| (c.call.as_str(), c.prompt_utf8_bytes))
            .collect();
        assert_eq!(
            calls,
            [("minimal", 67), ("mixed", 2738), ("adversarial", 5844)]
        );
        assert_eq!(measurement.measured.observed_at_ms, 1_794_089_390_110);

        let requests = rig.requests();
        let creates: Vec<Value> = (requests.iter())
            .filter(|(m, p, _)| *m == DockerMethod::Post && p.contains("/containers/create"))
            .map(|(_, _, b)| serde_json::from_slice(b).unwrap())
            .collect();
        let [create] = &creates[..] else {
            panic!("one sandbox expected, got {}", creates.len())
        };
        assert_eq!(create["Cmd"], json!(["/bin/bash", "/aq/in/run.sh"]));
        assert_eq!(create["Env"], json!([]));
        let host = &create["HostConfig"];
        assert_eq!(host["PidsLimit"], 512);
        assert_eq!(host["Memory"], 2u64 << 30);
        assert_eq!(host["NanoCpus"], 1_000_000_000u64);
        assert!(!host["Mounts"].to_string().contains("channel"));

        // The token is delivered once, as the private material; it is in no other
        // request body or path.
        let carrying: Vec<_> = (requests.iter())
            .filter(|(_, p, b)| holds(b) || holds(p.as_bytes()))
            .map(|(m, p, _)| (*m, p.clone()))
            .collect();
        assert_eq!(carrying.len(), 1, "{carrying:?}");
        assert_eq!(carrying[0].0, DockerMethod::Put);
        let (material, script) = rig.fake.seen_at_start.lock().unwrap().clone().unwrap();
        assert_eq!(material, TOKEN);
        assert!(!holds(&script) && script.starts_with(b"set -uo pipefail\n"));

        // Stopped and destroyed: no container or volume is left.
        assert!(rig.fake.docker.container_names().is_empty());
        assert!(rig.fake.docker.volume_names().is_empty());
        assert!(measurement.files.values().all(|f| !holds(f)));
    }

    #[tokio::test]
    async fn token_in_an_output_refuses_the_measurement_and_still_destroys() {
        let mut err = b"debug: token ".to_vec();
        err.extend_from_slice(TOKEN);
        let mut rig = rig(outputs(&err)).await;
        let e = rig.run().await.err().unwrap().to_string();
        assert!(e.contains("token appeared") && !holds(e.as_bytes()), "{e}");
        assert!(rig.fake.docker.container_names().is_empty());
        assert!(rig.fake.docker.volume_names().is_empty());
    }

    #[tokio::test]
    async fn missing_output_fails_and_destroys() {
        let mut files = outputs(b"");
        files.remove("mixed.jsonl");
        let mut rig = rig(files).await;
        let e = format!("{:#}", rig.run().await.err().unwrap());
        assert!(e.contains("mixed.jsonl"), "{e}");
        assert!(rig.fake.docker.container_names().is_empty());
    }
}
