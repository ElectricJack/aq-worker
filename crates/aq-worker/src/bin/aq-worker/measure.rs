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
use tokio::sync::watch;
use uuid::Uuid;
use zeroize::Zeroizing;

/// A finished measurement: the submission and the files it was built from (token-free).
pub struct Measurement {
    pub measured: Measured,
    pub files: BTreeMap<String, Vec<u8>>,
}

/// Runs the three calls in a fresh sandbox owned by `node_id` (slot 1) and builds the
/// submission. The token is read from the 0600 `token_file` and reaches the sandbox only
/// as private material; it is never in argv, a container's env or config, or a log. When
/// `cancel` turns true, or the sandbox deadline passes, the sandbox is killed and released.
pub async fn run(
    template: &Path,
    token_file: &Path,
    model: &str,
    node_id: Uuid,
    cancel: watch::Receiver<bool>,
) -> Result<Measurement> {
    let config = NativeConfig::load(template)?;
    let host = &docker_settings(&config)?.host;
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
    run_on(
        backend,
        &host.template,
        &script,
        &token,
        model,
        node_id,
        cancel,
    )
    .await
}

fn docker_settings(
    config: &NativeConfig,
) -> Result<&aq_worker::native::runner::ClaudeDockerSettings> {
    let Some(ExecutorConfig::ClaudeDockerLocalResultLocal { settings, .. }) = &config.executor
    else {
        bail!("the measurement needs the Claude docker-local executor in the native template");
    };
    Ok(settings)
}

/// Destroys every measurement sandbox (boot `MEASUREMENT_BOOT`) of `node_id` that a cut-short
/// run left behind; returns how many. The service runs it at start. It holds the profile's
/// measurement `lock` while it sweeps and does nothing while a `service measure` holds it.
pub async fn sweep(template: &Path, node_id: Uuid, lock: &Path) -> Result<usize> {
    let config = NativeConfig::load(template)?;
    let docker = docker_settings(&config)?.host.docker_local.clone();
    let mut backend = DockerLocalBackend::connect_unix(docker).await?;
    sweep_on(&mut backend, node_id, lock).await
}

pub(crate) async fn sweep_on<T: DockerTransport>(
    backend: &mut DockerLocalBackend<T>,
    node_id: Uuid,
    lock: &Path,
) -> Result<usize> {
    let Some(_held) = m::measure_lock(lock, false)? else {
        tracing::info!("a measurement is running; its sandbox is not swept");
        return Ok(0);
    };
    let mut released = 0;
    for entry in backend.list_owned(node_id).await? {
        if entry.sandbox.owner.boot_id == m::MEASUREMENT_BOOT
            && entry.state != aq_worker::backend::SandboxState::Destroyed
        {
            release(backend, &entry.sandbox).await?;
            released += 1;
        }
    }
    Ok(released)
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
    mut cancel: watch::Receiver<bool>,
) -> Result<Measurement> {
    let owner = Ownership {
        node_id,
        boot_id: m::MEASUREMENT_BOOT,
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
    // The sandbox is released on every path from here: success, error, the deadline (the
    // sandbox's own hard deadline, enforced here too) and a cancel.
    let until = tokio::time::Instant::now() + m::SANDBOX_DEADLINE;
    let driven = tokio::select! {
        files = drive(&mut backend, &sandbox, script, token) => files,
        () = tokio::time::sleep_until(until) => Err(anyhow::anyhow!(
            "the measurement passed its {} min deadline; its sandbox is killed",
            m::SANDBOX_DEADLINE.as_secs() / 60
        )),
        _ = cancel.wait_for(|v| *v) => Err(anyhow::anyhow!("the measurement was cancelled")),
    };
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
        /// The runner start never answers (a wedged sandbox).
        hang_start: bool,
    }
    impl DockerTransport for FakeClaude {
        async fn send(&self, request: DockerRequest) -> Result<DockerResponse, TransportError> {
            let (method, path) = (request.method, request.path.clone());
            self.requests
                .lock()
                .unwrap()
                .push((method, path.clone(), request.body.to_vec()));
            let start = method == DockerMethod::Post && path.ends_with("/start");
            if start && self.hang_start {
                std::future::pending::<()>().await;
            }
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
            hang_start: false,
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
            self.run_until(watch::channel(false).1).await
        }
        async fn run_until(&mut self, cancel: watch::Receiver<bool>) -> Result<Measurement> {
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
                cancel,
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

    /// Review #7: a wedged measurement is killed and released at its 15-minute deadline, and
    /// a cancelled one (service stop) is released at once.
    #[tokio::test(start_paused = true)]
    async fn deadline_and_cancel_release_the_sandbox() {
        let mut rig = rig_hanging().await;
        let started = tokio::time::Instant::now();
        let e = format!("{:#}", rig.run().await.err().unwrap());
        assert!(e.contains("deadline"), "{e}");
        assert_eq!(started.elapsed(), m::SANDBOX_DEADLINE);
        assert!(rig.fake.docker.container_names().is_empty());
        assert!(rig.fake.docker.volume_names().is_empty());

        let mut rig = rig_hanging().await;
        let (tx, rx) = watch::channel(false);
        let cancel = async {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            tx.send_replace(true);
        };
        let (result, ()) = tokio::join!(rig.run_until(rx), cancel);
        assert!(format!("{:#}", result.err().unwrap()).contains("cancelled"));
        assert!(rig.fake.docker.container_names().is_empty());
    }
    async fn rig_hanging() -> Rig {
        let mut rig = rig(outputs(b"")).await;
        rig.fake.hang_start = true;
        let mut config = rig.backend.take().unwrap().config().clone();
        // A daemon call that takes longer than the whole measurement may (a wedged daemon).
        config.api_timeout_ms = 3_600_000;
        rig.backend = Some(
            DockerLocalBackend::connect(config, rig.fake.clone())
                .await
                .unwrap(),
        );
        rig
    }

    /// Review #7: the start sweep destroys this node's leftover measurement sandboxes only.
    #[tokio::test]
    async fn sweep_destroys_only_measurement_sandboxes() {
        let mut rig = rig(outputs(b"")).await;
        let mut backend = rig.backend.take().unwrap();
        let node = Uuid::from_u128(1);
        for boot in [m::MEASUREMENT_BOOT, Uuid::new_v4()] {
            let owner = Ownership {
                node_id: node,
                boot_id: boot,
                attempt_id: Uuid::new_v4(),
            };
            let limits = ResourceLimits {
                cpu_millis: m::CPU_MILLIS,
                memory_bytes: m::MEMORY_BYTES,
                pids: m::PIDS,
                disk_bytes: None,
            };
            let request = CreateRequest::new(owner, rig.template.clone(), u64::MAX / 2, vec![])
                .with_resources(limits);
            backend.create(&request).await.unwrap();
        }
        assert_eq!(rig.fake.docker.container_names().len(), 2);
        // Round 2, B: while `service measure` holds the lock, its sandbox is left alone.
        let lock = rig._dir.path().join("measure.lock");
        let held = m::measure_lock(&lock, false).unwrap().unwrap();
        assert_eq!(sweep_on(&mut backend, node, &lock).await.unwrap(), 0);
        assert_eq!(rig.fake.docker.container_names().len(), 2);
        drop(held);
        assert_eq!(sweep_on(&mut backend, node, &lock).await.unwrap(), 1);
        assert_eq!(rig.fake.docker.container_names().len(), 1);
        assert_eq!(sweep_on(&mut backend, node, &lock).await.unwrap(), 0);
    }

    /// Round 2, A: the node destroys a claimed attempt's stopped sandbox even when the driver
    /// was killed right after its claim; unclaimed and running sandboxes stay. Round 3: a
    /// sandbox its driver preserved (work never copied back) stays too. The executor (after the
    /// claim), `release_claimed` and settle all go through this one function.
    #[tokio::test]
    async fn node_destroys_claimed_stopped_sandboxes_only() {
        use aq_worker::backend::{FakeBackend, PrivateMaterial, SandboxState};
        let mut backend = FakeBackend::default();
        let node = Uuid::from_u128(1);
        let template = TemplateDigest::parse(format!("sha256:{}", "1".repeat(64))).unwrap();
        let ids = [
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
        ];
        let [claimed, unclaimed, running, preserved] = ids;
        let host = tempfile::tempdir().unwrap();
        for (n, attempt) in [claimed, preserved].iter().enumerate() {
            let dir = host.path().join(format!("driver-{n}"));
            std::fs::create_dir_all(&dir).unwrap();
            let owner = serde_json::json!({"owner": {"attempt_id": attempt}}).to_string();
            std::fs::write(dir.join("driver.json"), owner).unwrap();
        }
        // The preserved attempt's driver failed before its copy-back and claimed "preserved".
        let config = host.path().join("driver-1/driver.json");
        let failure = anyhow::anyhow!("workspace copy-back: over its bound");
        assert!(aq_handoff_note::preserved_claim(&failure, "sb", &config).is_some());
        for attempt_id in ids {
            let owner = Ownership {
                node_id: node,
                boot_id: Uuid::new_v4(),
                attempt_id,
            };
            let request = CreateRequest::new(owner, template.clone(), 1 << 40, vec![]);
            let sandbox = backend.create(&request).await.unwrap();
            let material = PrivateMaterial::new(b"fixture".to_vec());
            backend
                .deliver_private_material(&sandbox, material)
                .await
                .unwrap();
            backend.start_runner(&sandbox).await.unwrap();
            if attempt_id != running {
                // The driver claimed and was killed before any destroy: stopped, not destroyed.
                assert!(backend.stop(&sandbox).await.is_confirmed());
            }
        }
        let destroyed = aq_worker::native::runner::destroy_claimed_sandboxes(
            &mut backend,
            host.path(),
            node,
            &[claimed, running, preserved],
        )
        .await
        .unwrap();
        assert_eq!(destroyed, 1);
        let states: BTreeMap<Uuid, SandboxState> = (backend.list_owned(node).await.unwrap())
            .into_iter()
            .map(|e| (e.sandbox.owner.attempt_id, e.state))
            .collect();
        assert!(
            states
                .get(&claimed)
                .is_none_or(|s| *s == SandboxState::Destroyed),
            "{states:?}"
        );
        assert_eq!(states[&unclaimed], SandboxState::Stopped);
        assert_eq!(
            states[&preserved],
            SandboxState::Stopped,
            "kept for the operator"
        );
        assert_eq!(states[&running], SandboxState::Running);
    }
}
