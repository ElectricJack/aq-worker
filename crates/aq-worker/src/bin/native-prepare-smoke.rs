//! Explicit test-support process: actual input and Grant owners, with an opt-in
//! fixed synthetic host probe. No provider, resume or resource release.
use anyhow::{Context, Result, ensure};
use aq_admission::{
    ArtifactKind, Resources,
    transport::{Feature, Poll, Register},
};
use aq_session_protocol::{ArtifactRef, Capability};
use aq_worker::{
    config::WorkerConfig,
    native::{
        http::{HttpStatus, NativeClient},
        journal::{Journal, Limits, RuntimeIdentity},
        launch::{
            InstalledFilePin, NativeHostLaunchResult, NativeStopOutcome, SyntheticHostConfig,
            SyntheticHostProfile, SyntheticMode,
        },
        materials::MaterialStore,
        preparation::{GrantCommit, InputPipeline, PhaseExecutionCommit},
    },
    source::GitClient,
};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::json;
use std::{
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    process::ExitCode,
    sync::Arc,
};
use uuid::Uuid;
use zeroize::Zeroizing;
#[path = "native-prepare-smoke/legacy.rs"]
mod legacy;
#[path = "native-prepare-smoke/output.rs"]
mod output;
#[path = "native-prepare-smoke/phase.rs"]
mod phase;
#[path = "native-prepare-smoke/physical.rs"]
mod physical;
#[path = "native-prepare-smoke/setup.rs"]
mod setup;
const CONFIG_BYTES: usize = 32768;
const OUTPUT_BYTES: usize = 262144;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    worker: WorkerConfig,
    root: PathBuf,
    git_socket: PathBuf,
    git_root: PathBuf,
    runtime: ArtifactRef,
    qualification: ArtifactRef,
    capabilities: Vec<Capability>,
    features: Vec<Feature>,
    phase_preparation: Option<PhaseConfig>,
    refresh_preparation: Option<PhaseConfig<aq_admission::preparation_refresh_v1::Negotiation>>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PhaseConfig<N = aq_admission::preparation_v2::Negotiation> {
    negotiation: N,
    source_profile: ArtifactRef,
    maximum_preparation_ms: u64,
    #[serde(default)]
    prepare_inputs: bool,
    #[serde(default)]
    prepare_execution: bool,
    #[serde(default)]
    history_after_lost_execution: bool,
    #[serde(default)]
    report_terminal: bool,
    #[serde(default)]
    relay_output: bool,
    #[serde(default)]
    await_synthetic_assessment: bool,
    synthetic_host: Option<HostConfig>,
}
impl<N> PhaseConfig<N> {
    fn validate(&self) -> Result<()> {
        ensure!(
            (1..=aq_admission::preparation_v2::MAX_PREPARATION_MS)
                .contains(&self.maximum_preparation_ms),
            "preparation duration exceeds the declared protocol bound"
        );
        ensure!(
            !self.prepare_execution || self.prepare_inputs,
            "phase execution requires actual inputs"
        );
        ensure!(
            self.synthetic_host.is_none() || self.prepare_execution,
            "synthetic host requires actual phase execution"
        );
        ensure!(
            !self.history_after_lost_execution
                || (self.prepare_execution && self.prepare_inputs && self.synthetic_host.is_none()),
            "history-only probe requires inputs/execution and forbids host"
        );
        self.validate_terminal()
    }
    fn validate_terminal(&self) -> Result<()> {
        ensure!(
            !self.report_terminal || self.synthetic_host.is_some(),
            "terminal reporting requires actual synthetic host proof"
        );
        ensure!(
            !self.relay_output || self.report_terminal,
            "output relay probe requires terminal reporting"
        );
        ensure!(
            !self.await_synthetic_assessment || self.relay_output,
            "synthetic assessment pause requires the static output probe"
        );
        Ok(())
    }
}
fn expected_lost_execution_reply(error: Option<&anyhow::Error>) -> Result<()> {
    ensure!(
        error
            .and_then(|e| e.downcast_ref::<HttpStatus>())
            .is_some_and(|s| s.0 == reqwest::StatusCode::SERVICE_UNAVAILABLE),
        "history-only probe requires actual503 execution reply"
    );
    Ok(())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HostPin {
    path: PathBuf,
    artifact: ArtifactRef,
}
impl From<HostPin> for InstalledFilePin {
    fn from(pin: HostPin) -> Self {
        Self {
            path: pin.path,
            artifact: pin.artifact,
        }
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HostConfig {
    helper: HostPin,
    tmux: HostPin,
    program: HostPin,
    renderer: ArtifactRef,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Continue {
    r#continue: bool,
}
/// Test-only pause on the already-held input owner. Partial stdin frames and a
/// silent parent consume the original preparation deadline, never extend it.
fn continuation_before<R: Read>(reader: &mut BufReader<R>, fd: i32, deadline: u64) -> Result<()> {
    let mut bytes = Zeroizing::new(Vec::new());
    loop {
        let remaining = deadline
            .checked_sub(boot_ns()?)
            .filter(|n| *n > 0)
            .context("synthetic assessment original deadline expired")?;
        if reader.buffer().is_empty() {
            let mut descriptor = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let wait_ms = remaining.div_ceil(1_000_000).min(i32::MAX as u64) as i32;
            let count = unsafe { libc::poll(&mut descriptor, 1, wait_ms) };
            if count < 0
                && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
            {
                continue;
            }
            ensure!(
                count > 0,
                "synthetic assessment pause expired or unavailable"
            );
        }
        ensure!(
            boot_ns()? < deadline,
            "synthetic assessment original deadline expired"
        );
        let available = reader.fill_buf()?;
        ensure!(
            !available.is_empty(),
            "synthetic assessment continuation EOF"
        );
        let size = available
            .iter()
            .position(|b| *b == b'\n')
            .map_or(available.len(), |n| n + 1);
        ensure!(
            bytes.len() + size <= 1024,
            "synthetic assessment continuation bound"
        );
        bytes.extend_from_slice(&available[..size]);
        reader.consume(size);
        if bytes.last() == Some(&b'\n') {
            let next: Continue = serde_json::from_slice(&bytes)?;
            ensure!(
                next.r#continue,
                "synthetic assessment continuation required"
            );
            return Ok(());
        }
    }
}
fn boot_ns() -> Result<u64> {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    ensure!(
        unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut time) } == 0
            && time.tv_sec >= 0
            && (0..1000000000).contains(&time.tv_nsec),
        "boot clock unavailable"
    );
    (time.tv_sec as u64)
        .checked_mul(1000000000)
        .and_then(|s| s.checked_add(time.tv_nsec as u64))
        .context("boot clock overflow")
}
fn frame<T: DeserializeOwned>(reader: &mut impl BufRead, limit: usize) -> Result<T> {
    let mut bytes = Zeroizing::new(Vec::new());
    let n = reader
        .take((limit + 1) as u64)
        .read_until(b'\n', &mut bytes)?;
    ensure!(
        n > 0 && n <= limit && bytes.last() == Some(&b'\n'),
        "bounded complete input frame required"
    );
    Ok(serde_json::from_slice(&bytes)?)
}
fn emit(value: &serde_json::Value) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(bytes.len() < OUTPUT_BYTES, "output frame bound");
    let mut out = std::io::stdout().lock();
    out.write_all(&bytes)?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}
fn root(path: &Path) -> Result<File> {
    ensure!(
        path.is_absolute() && path.canonicalize()? == path,
        "canonical private scope required"
    );
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let m = file.metadata()?;
    ensure!(
        m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o7777 == 0o700,
        "private scope owner required"
    );
    Ok(file)
}
fn current_root(path: &Path, held: &File) -> Result<()> {
    let named = root(path)?;
    let a = held.metadata()?;
    let b = named.metadata()?;
    ensure!(
        a.dev() == b.dev() && a.ino() == b.ino(),
        "private scope replaced"
    );
    Ok(())
}
fn create_materials(held: &File) -> Result<()> {
    ensure!(
        unsafe { libc::mkdirat(held.as_raw_fd(), c"materials".as_ptr(), 0o700) } == 0,
        "brand-new material child required"
    );
    held.sync_all()?;
    Ok(())
}
async fn register(
    client: &NativeClient,
    config: &Config,
    identity: &aq_admission::transport::WorkerIdentity,
) -> Result<()> {
    client
        .register(&Register {
            schema_version: 1,
            operation_id: Uuid::new_v4(),
            identity: identity.clone(),
            runtime: config.runtime.clone(),
            capabilities: config.capabilities.clone(),
            features: config.features.clone(),
        })
        .await?;
    Ok(())
}
fn validate_config(config: &Config) -> Result<()> {
    ensure!(
        config.phase_preparation.is_none() || config.refresh_preparation.is_none(),
        "nominal preparation profiles cannot be combined"
    );
    if let Some(phase) = &config.phase_preparation {
        phase.validate()?;
        phase.negotiation.validate()?;
    }
    if let Some(phase) = &config.refresh_preparation {
        phase.validate()?;
        phase.negotiation.validate()?;
    }
    Ok(())
}
async fn run(stage: &mut &'static str) -> Result<()> {
    let stdin = std::io::stdin();
    let mut input = BufReader::new(stdin.lock());
    let config: Config = frame(&mut input, CONFIG_BYTES)?;
    validate_config(&config)?;
    let run = setup::SmokeRun::open(config)?;
    run.ready(&mut input, stage).await?;
    let reserved = run.reserve(stage).await?;
    if reserved.run.config.phase_preparation.is_some()
        || reserved.run.config.refresh_preparation.is_some()
    {
        return phase::run(reserved, &mut input, stage).await;
    }
    legacy::run(reserved, stage).await
}
#[tokio::main]
async fn main() -> ExitCode {
    let mut stage = "startup";
    match run(&mut stage).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => {
            let _ = emit(&json!({"phase":"refused","stage":stage}));
            ExitCode::FAILURE
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn reference(id: &str) -> ArtifactRef {
        ArtifactRef {
            id: aq_session_protocol::Text::new(id).unwrap(),
            version: aq_session_protocol::Text::new("1").unwrap(),
            digest: aq_session_protocol::Digest::of(id.as_bytes()),
        }
    }
    #[test]
    fn explicit_refresh_profile_rejects_mixed_or_unsupported_configuration_before_io() {
        use aq_admission::{preparation_refresh_v1 as refresh, preparation_v2 as frozen};
        fn phase<N: DeserializeOwned>(negotiation: serde_json::Value) -> PhaseConfig<N> {
            serde_json::from_value(json!({"negotiation":negotiation,"source_profile":reference("synthetic-source"),"maximum_preparation_ms":30000,"prepare_inputs":true,"prepare_execution":true})).unwrap()
        }
        let mut config = Config {
            worker: WorkerConfig {
                server_url: "http://127.0.0.1:9".into(),
                node_id: Uuid::new_v4(),
                credential: "synthetic-node-not-a-provider-key".into(),
                cf_client_id: None,
                cf_client_secret: None,
            },
            root: PathBuf::from("/nonexistent/aq-refresh-config-test"),
            git_socket: PathBuf::from("/nonexistent/socket"),
            git_root: PathBuf::from("/nonexistent/git"),
            runtime: reference("synthetic-worker-0.4.0"),
            qualification: reference("synthetic-qualification"),
            capabilities: vec![],
            features: vec![],
            phase_preparation: None,
            refresh_preparation: Some(phase(
                json!({"contract":refresh::contract_artifact().unwrap(),"features":refresh::required_features()}),
            )),
        };
        validate_config(&config).unwrap();
        config.phase_preparation = Some(phase(
            json!({"contract":frozen::contract_artifact().unwrap(),"features":frozen::required_features()}),
        ));
        assert!(validate_config(&config).is_err());
        config.phase_preparation = None;
        config
            .refresh_preparation
            .as_mut()
            .unwrap()
            .negotiation
            .contract = frozen::contract_artifact().unwrap();
        assert!(validate_config(&config).is_err());
        config
            .refresh_preparation
            .as_mut()
            .unwrap()
            .negotiation
            .contract = refresh::contract_artifact().unwrap();
        config
            .refresh_preparation
            .as_mut()
            .unwrap()
            .maximum_preparation_ms = refresh::MAX_PREPARATION_MS + 1;
        assert!(validate_config(&config).is_err());
    }
    #[test]
    fn synthetic_assessment_pause_keeps_the_original_deadline_for_silent_or_partial_parent() {
        for prefix in [&b""[..], &b"{\"continue\":"[..]] {
            let (read, mut write) = std::os::unix::net::UnixStream::pair().unwrap();
            write.write_all(prefix).unwrap();
            let fd = read.as_raw_fd();
            let mut reader = BufReader::new(read);
            let deadline = boot_ns().unwrap() + 20_000_000;
            assert!(continuation_before(&mut reader, fd, deadline).is_err());
            assert!(boot_ns().unwrap() >= deadline);
            drop(write);
        }
        let mut reader = BufReader::new(std::io::Cursor::new(b"{\"continue\":true}\n"));
        reader.fill_buf().unwrap();
        continuation_before(&mut reader, -1, boot_ns().unwrap() + 1_000_000_000).unwrap();
        assert!(continuation_before(&mut reader, -1, boot_ns().unwrap()).is_err());
    }
    #[test]
    fn input_frames_are_bounded_complete_and_do_not_consume_the_following_frame() {
        let mut r = std::io::Cursor::new(b"{\"continue\":true}\n{\"continue\":false}\n");
        assert!(frame::<Continue>(&mut r, 1024).unwrap().r#continue);
        assert!(!frame::<Continue>(&mut r, 1024).unwrap().r#continue);
        assert!(
            frame::<Continue>(&mut std::io::Cursor::new(b"{\"continue\":true}"), 1024).is_err()
        );
        assert!(
            frame::<Continue>(
                &mut std::io::Cursor::new(b"{\"continue\":true,\"unknown\":1}\n"),
                1024
            )
            .is_err()
        );
        let mut r = std::io::Cursor::new(vec![b'x'; 10000]);
        assert!(frame::<Continue>(&mut r, 32).is_err());
        assert_eq!(r.position(), 33);
    }
    #[test]
    fn history_mode_requires_real_inputs_execution_and_excludes_host_before_io() {
        let f: serde_json::Value = serde_json::from_slice(include_bytes!(
            "../../../../vendor/aq-admission-0.1.9/artifacts/preparation-v2.fixture.json"
        ))
        .unwrap();
        let mut p: PhaseConfig = PhaseConfig {
            negotiation: serde_json::from_value(f["proposal"]["negotiation"].clone()).unwrap(),
            source_profile: serde_json::from_value(f["accept"]["hold"]["source_profile"].clone())
                .unwrap(),
            maximum_preparation_ms: 30000,
            prepare_inputs: false,
            prepare_execution: false,
            history_after_lost_execution: true,
            report_terminal: false,
            relay_output: false,
            await_synthetic_assessment: false,
            synthetic_host: None,
        };
        assert!(p.validate().is_err());
        p.prepare_inputs = true;
        assert!(p.validate().is_err());
        p.prepare_execution = true;
        p.validate().unwrap();
        let r = p.source_profile.clone();
        p.synthetic_host = Some(HostConfig {
            helper: HostPin {
                path: "/not-executed/helper".into(),
                artifact: r.clone(),
            },
            tmux: HostPin {
                path: "/not-executed/tmux".into(),
                artifact: r.clone(),
            },
            program: HostPin {
                path: "/not-executed/program".into(),
                artifact: r.clone(),
            },
            renderer: r,
        });
        assert!(p.validate().is_err());
        p.history_after_lost_execution = false;
        p.report_terminal = true;
        p.validate().unwrap();
        p.await_synthetic_assessment = true;
        assert!(p.validate().is_err());
        p.relay_output = true;
        p.validate().unwrap();
        p.synthetic_host = None;
        assert!(p.validate().is_err());
    }
    #[test]
    fn history_probe_accepts_only_explicit503_and_never_success_or_other_errors() {
        let expected: anyhow::Error =
            HttpStatus(reqwest::StatusCode::SERVICE_UNAVAILABLE, None).into();
        expected_lost_execution_reply(Some(&expected)).unwrap();
        let other: anyhow::Error = HttpStatus(reqwest::StatusCode::CONFLICT, None).into();
        assert!(expected_lost_execution_reply(Some(&other)).is_err());
        let transport = anyhow::anyhow!("transport unknown");
        assert!(expected_lost_execution_reply(Some(&transport)).is_err());
        assert!(expected_lost_execution_reply(None).is_err());
    }
    #[test]
    fn existing_owner_children_and_replaced_scope_are_refused_without_cleanup() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let held = root(dir.path()).unwrap();
        create_materials(&held).unwrap();
        std::fs::write(dir.path().join("materials/sentinel"), b"held").unwrap();
        assert!(create_materials(&held).is_err());
        assert_eq!(
            std::fs::read(dir.path().join("materials/sentinel")).unwrap(),
            b"held"
        );
        let renamed = dir.path().with_extension("held");
        std::fs::rename(dir.path(), &renamed).unwrap();
        std::fs::create_dir(dir.path()).unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(current_root(dir.path(), &held).is_err());
        std::fs::remove_dir_all(renamed).unwrap();
    }
}
