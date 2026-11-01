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
    renderer: ArtifactRef,
    phase_preparation: Option<PhaseConfig>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PhaseConfig {
    negotiation: aq_admission::preparation_v2::Negotiation,
    source_profile: ArtifactRef,
    maximum_preparation_ms: u64,
    #[serde(default)]
    prepare_inputs: bool,
    #[serde(default)]
    prepare_execution: bool,
    #[serde(default)]
    history_after_lost_execution: bool,
    synthetic_host: Option<HostConfig>,
}
impl PhaseConfig {
    fn validate(&self) -> Result<()> {
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
async fn run(stage: &mut &'static str) -> Result<()> {
    let stdin = std::io::stdin();
    let mut input = BufReader::new(stdin.lock());
    let config: Config = frame(&mut input, CONFIG_BYTES)?;
    if let Some(phase) = &config.phase_preparation {
        phase.validate()?;
    }
    let held = root(&config.root)?;
    let git = GitClient::new(&config.git_socket, &config.git_root)?;
    ensure!(
        git.root().starts_with(&config.root) && git.root() != config.root,
        "Git scope must be a private child"
    );
    let disk = git.native_reservation_bytes()?;
    // Fixture-only logical envelope, not an assertion about real free capacity.
    let limits = Limits {
        slots: 2,
        memory_bytes: 64 * 1024 * 1024 * 1024,
        cpu_millis: 120000,
        disk_bytes: 64 * 1024 * 1024 * 1024,
    };
    ensure!(
        disk <= limits.disk_bytes,
        "Git profile exceeds smoke envelope"
    );
    let mut journal = Journal::create(&config.root.join("journal"), config.worker.node_id, limits)?;
    current_root(&config.root, &held)?;
    create_materials(&held)?;
    let identity = journal.state().identity.clone();
    let materials = MaterialStore::open(&config.root.join("materials"), identity.clone(), 32)?;
    current_root(&config.root, &held)?;
    let client = NativeClient::from_config(&config.worker)?;
    // Qualified InventoryV2 must observe the actual child's admitted runtime.
    // Earlier probe modes keep their original ready/continue/register ordering.
    let registered_before_ready = config
        .phase_preparation
        .as_ref()
        .is_some_and(|p| p.prepare_execution);
    if registered_before_ready {
        *stage = "register";
        register(&client, &config, &identity).await?;
    }
    emit(&json!({"phase":"ready","identity":identity}))?;
    *stage = "continue";
    let next: Continue = frame(&mut input, 1024)?;
    ensure!(next.r#continue, "explicit continue required");
    current_root(&config.root, &held)?;
    if !registered_before_ready {
        *stage = "register";
        register(&client, &config, &identity).await?;
    }
    *stage = "poll";
    let available = journal.available()?;
    let mut page = client
        .poll(&Poll {
            schema_version: 1,
            operation_id: Uuid::new_v4(),
            identity,
            inventory_sequence: 1,
            available: Resources {
                slots: available.slots,
                memory_bytes: available.memory_bytes,
                cpu_millis: available.cpu_millis,
                disk_bytes: available.disk_bytes,
            },
            limit: 1,
            after_offer: None,
        })
        .await?;
    ensure!(
        page.page.offers.len() == 1 && page.offer_deadlines_ns.len() == 1,
        "exactly one native offer required"
    );
    let offer = page.page.offers.remove(0);
    let ttl = offer
        .offered
        .reservation_ttl_ms
        .checked_mul(1000000)
        .context("reservation duration overflow")?;
    let deadline = boot_ns()?
        .checked_add(ttl)
        .context("reservation deadline overflow")?
        .min(page.offer_deadlines_ns[0]);
    *stage = "reserve";
    let id = journal.reserve(
        offer.clone(),
        RuntimeIdentity {
            runtime: config.runtime,
            qualification: config.qualification,
        },
        deadline,
        disk,
    )?;
    if let Some(phase) = config.phase_preparation {
        *stage = "phase_proposal";
        let proposed = client
            .propose_preparation(
                &journal.state().identity,
                &offer,
                &phase.negotiation,
                deadline,
            )
            .await?;
        *stage = "phase_accept_intent";
        let request = journal.begin_phase_accept(
            id,
            &proposed,
            config.capabilities.clone(),
            phase.source_profile,
            phase.maximum_preparation_ms,
        )?;
        *stage = "phase_accept";
        let observed = client.accept_preparation(&proposed, &request, None).await?;
        *stage = "phase_received_history";
        let first = journal.record_phase_preparation(id, &observed)?;
        journal.check_phase_preparation(id, &observed)?;
        if !phase.prepare_execution {
            // Wait a full original delivery duration from now, then check the real
            // replay's server clock. This test pause changes no lease or deadline.
            let pause_ms = offer
                .offered
                .clock
                .valid_for_ms
                .checked_add(100)
                .context("phase probe pause overflow")?;
            ensure!(pause_ms <= 5000, "bounded phase probe pause required");
            tokio::time::sleep(std::time::Duration::from_millis(pause_ms)).await;
        }
        *stage = "phase_replay";
        let replay = client
            .accept_preparation(&proposed, &request, Some(&observed))
            .await?;
        let retained = journal.record_phase_preparation(id, &replay)?;
        let cap = journal.check_phase_preparation(id, &replay)?;
        current_root(&config.root, &held)?;
        let mut result = json!({"phase":"accepted_preparation","request":request,"proposal":proposed.proposal(),
            "initial_delivery":observed.delivery(),"replay_delivery":replay.delivery(),
            "history":retained,"first_history":first,"journal_revision":journal.state().revision,
            "journal_format":journal.state().format,"original_deadline_ns":deadline,
            "checked_at_ns":boot_ns()?,"deadline_ns":cap});
        if phase.prepare_inputs {
            let preparation = Arc::new(replay);
            let pipeline = Arc::new(InputPipeline::new(journal, materials, client.clone()));
            *stage = "phase_artifacts";
            let artifacts = pipeline.artifacts_phase(id, preparation.clone()).await?;
            *stage = "phase_materials";
            for lease in &preparation.delivery().receipt.lease.materials {
                pipeline
                    .material_phase(id, preparation.clone(), lease.lease_id)
                    .await?;
            }
            let spec = offer
                .offered
                .artifacts
                .iter()
                .find(|d| {
                    d.kind == ArtifactKind::Composition
                        && d.artifact.id.as_str() == "aq.profiled-execution-spec"
                        && d.artifact.digest == offer.offered.offer.binding.execution_spec_digest
                })
                .context("original typed phase spec absent")?;
            *stage = "phase_hydrate";
            let spec = pipeline
                .artifact_phase(id, preparation.clone(), spec.artifact.clone())
                .await?;
            let workspace = pipeline
                .hydrate_source_phase(id, preparation.clone(), spec, git)
                .await?;
            *stage = "phase_prepare_inputs";
            let prepared = pipeline
                .prepare_inputs_phase(
                    id,
                    preparation.clone(),
                    artifacts,
                    workspace,
                    config.renderer,
                )
                .await?;
            current_root(&config.root, &held)?;
            // Actual opaque owners remain alive through observation. Emitted DTOs
            // are diagnostic history and cannot create execution authority.
            let snapshot = pipeline.snapshot().await?;
            result["phase"] = json!("prepared_phase_inputs");
            result["prepare_request"] = json!(prepared.request());
            result["workspace_path"] = json!(prepared.workspace().workspace().path);
            result["material_receipts"] = json!(
                prepared
                    .materials()
                    .iter()
                    .map(|m| m.receipt())
                    .collect::<Result<Vec<_>>>()?
            );
            result["prepared_revision"] = json!(snapshot.revision);
            result["prepared_at_ns"] = json!(boot_ns()?);
            if phase.prepare_execution {
                *stage = "phase_execution_http";
                let observation = client
                    .prepare_execution(&proposed, &request, &preparation, prepared.request(), None)
                    .await;
                if phase.history_after_lost_execution {
                    expected_lost_execution_reply(observation.as_ref().err())?;
                    *stage = "phase_execution_history_only";
                    let q = aq_phase_terminal_protocol::HistoryRequest {
                        schema_version: 1,
                        negotiation: aq_phase_terminal_protocol::Negotiation::current()?,
                        operation_id: Uuid::new_v4(),
                        binding: prepared.request().original.binding.clone(),
                        accepted_receipt_digest: preparation.delivery().receipt.digest()?,
                        prepare_operation_id: prepared.request().original.operation_id,
                    };
                    let receipt = client.phase_terminal_history(&q).await?;
                    let known = receipt
                        .identity
                        .as_ref()
                        .context("actual issued execution history required")?;
                    let lease = &preparation.delivery().receipt.lease;
                    ensure!(
                        known.prepare_request_digest == prepared.request().digest()?
                            && known.guardian_id == lease.guardian_id
                            && known.launch_id == lease.launch_id
                            && known.reservation_id == lease.hold.reservation.reservation_id,
                        "history differs from actual Prepared/Accepted pins"
                    );
                    current_root(&config.root, &held)?;
                    let snapshot = pipeline.snapshot().await?;
                    let attempt = snapshot
                        .attempts
                        .get(&id)
                        .context("retained phase attempt absent")?;
                    ensure!(
                        known.runtime == attempt.reservation.runtime.runtime
                            && known.qualification == attempt.reservation.runtime.qualification
                            && attempt.phase_execution.is_none()
                            && attempt.native_host.is_none()
                            && snapshot.revision
                                == result["prepared_revision"]
                                    .as_u64()
                                    .context("prepared revision absent")?,
                        "history must not adopt execution or mutate journal"
                    );
                    result["phase"] = json!("phase_history_only");
                    result["history_request"] = json!(q);
                    result["history_receipt"] = json!(receipt);
                    result["history_revision"] = json!(snapshot.revision);
                    // No record_phase_execution, host or renewal call. Keep the
                    // actual exclusive Prepared owner through the final emit.
                    emit(&result)?;
                    drop(prepared);
                    return Ok(());
                }
                let observation = Arc::new(observation?);
                result["execution_delivery"] = json!(observation.delivery());
                *stage = "phase_execution_durable";
                let PhaseExecutionCommit::Usable(grant) = pipeline
                    .record_phase_execution(prepared, observation.clone())
                    .await?
                else {
                    anyhow::bail!("phase execution retained as history only");
                };
                pipeline.check_phase_execution(&grant).await?;
                result["phase"] = json!("granted_phase_execution");
                result["execution_record_digest"] = json!(grant.history().record_digest());
                result["execution_deadline_ns"] = json!(grant.deadline_ns());
                result["execution_hard_deadline_ns"] = json!(grant.hard_deadline_ns());
                result["execution_checked_at_ns"] = json!(boot_ns()?);
                result["execution_revision"] = json!(pipeline.snapshot().await?.revision);
                if let Some(host) = phase.synthetic_host {
                    *stage = "phase_host_profile";
                    let host_root = config.root.join("host");
                    use std::os::unix::fs::DirBuilderExt;
                    std::fs::DirBuilder::new().mode(0o700).create(&host_root)?;
                    held.sync_all()?;
                    let profile = SyntheticHostProfile::new(SyntheticHostConfig {
                        root: host_root,
                        helper: host.helper.into(),
                        tmux: host.tmux.into(),
                        program: host.program.into(),
                        harness: offer.offered.offer.manifest.harness.clone(),
                        capabilities: config.capabilities.clone(),
                        renderer: host.renderer,
                        columns: 80,
                        rows: 24,
                        mode: SyntheticMode::Wait,
                    })?;
                    *stage = "phase_host_launch";
                    let launched = pipeline.launch_phase_synthetic(&grant, profile).await?;
                    // Positive exec does not mean the synthetic program has
                    // finished its first write. Give the fixture a bounded
                    // observation window; this never extends the host permit.
                    if matches!(&launched, NativeHostLaunchResult::Running(_)) {
                        let marker = config
                            .root
                            .join("host")
                            .join(preparation.delivery().receipt.lease.launch_id.to_string());
                        for _ in 0..50 {
                            if marker.join("fixture-marker").is_file()
                                && marker.join("fixture-marker.cwd").is_file()
                            {
                                break;
                            }
                            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                        }
                    }
                    // Stop before inspecting/serializing any result, including an
                    // ambiguous launch. The original guardian cap also remains.
                    *stage = "phase_host_stop";
                    ensure!(
                        matches!(
                            pipeline.stop_phase_host(id).await?,
                            NativeStopOutcome::StoppedPendingJournal(_)
                        ),
                        "actual stop proof required"
                    );
                    pipeline.persist_phase_host_stop(id).await?;
                    let NativeHostLaunchResult::Running(host) = launched else {
                        anyhow::bail!("synthetic host lacked positive Running proof");
                    };
                    result["host_started"] = json!(host.started_request()?);
                    let snapshot = pipeline.snapshot().await?;
                    result["host_revision"] = json!(snapshot.revision);
                    result["host_stopped"] = json!(
                        snapshot.attempts[&id]
                            .native_host
                            .as_ref()
                            .context("native host history absent")?
                            .stopped
                    );
                }
                // Keep original actual network observation and input owner alive
                // through reporting; diagnostic JSON cannot restore either one.
                emit(&result)?;
                return Ok(());
            }
        }
        emit(&result)?;
        return Ok(());
    }
    let accept = journal.begin_accept(id, config.capabilities)?;
    *stage = "accept";
    let preparation = Arc::new(client.accept(&accept, deadline, None).await?);
    journal.record_preparation(id, &preparation)?;
    let pipeline = InputPipeline::new(journal, materials, client.clone());
    *stage = "artifacts";
    let artifacts = pipeline.artifacts(id, preparation.clone()).await?;
    *stage = "materials";
    for lease in &preparation.plan().materials {
        pipeline
            .material(id, preparation.clone(), lease.lease_id)
            .await?;
    }
    // hydrate_source consumes its verified spec; download the exact same planned
    // requests again rather than fabricating/cloning a proof from its receipt.
    let spec = offer
        .offered
        .artifacts
        .iter()
        .find(|d| {
            d.kind == ArtifactKind::Composition
                && d.artifact.id.as_str() == "aq.profiled-execution-spec"
                && d.artifact.digest == offer.offered.offer.binding.execution_spec_digest
        })
        .context("original typed spec absent")?;
    *stage = "hydrate";
    let spec = pipeline
        .artifact(id, preparation.clone(), spec.artifact.clone())
        .await?;
    let workspace = pipeline
        .hydrate_source(id, preparation.clone(), spec, git)
        .await?;
    *stage = "prepare_inputs";
    let prepared = pipeline
        .prepare_inputs(
            id,
            preparation.clone(),
            artifacts,
            workspace,
            config.renderer,
        )
        .await?;
    current_root(&config.root, &held)?;
    *stage = "initial_grant";
    let grant = client
        .prepare(prepared.request(), &preparation, &offer, None)
        .await?;
    let receipts = prepared
        .materials()
        .iter()
        .map(|m| m.receipt())
        .collect::<Result<Vec<_>>>()?;
    // Capture bounded diagnostic DTOs before transferring the actual exclusive
    // input owners. These serialized values never confer live authority.
    let mut result = json!({"phase":"granted","request":prepared.request(),"grant":grant.initial(),"workspace_path":prepared.workspace().workspace().path,"material_receipts":receipts});
    *stage = "durable_initial_grant";
    let committed = pipeline.record_initial_grant(prepared, grant).await?;
    let durable = match committed {
        GrantCommit::Usable(durable) => durable,
        GrantCommit::HistoryOnly(_) => {
            anyhow::bail!("received Grant retained as history only");
        }
    };
    *stage = "check_durable_initial_grant";
    pipeline.check_initial_grant(&durable).await?;
    current_root(&config.root, &held)?;
    result["durable_grant"] = json!({
        "attempt_id":durable.history().attempt_id(),
        "record_digest":durable.history().record_digest(),
        "observed_after_sync_ns":durable.history().observed_after_sync_ns(),
        "deadline_ns":durable.deadline_ns(),
        "hard_deadline_ns":durable.hard_deadline_ns()
    });
    emit(&result)?;
    Ok(())
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
        let mut p = PhaseConfig {
            negotiation: serde_json::from_value(f["proposal"]["negotiation"].clone()).unwrap(),
            source_profile: serde_json::from_value(f["accept"]["hold"]["source_profile"].clone())
                .unwrap(),
            maximum_preparation_ms: 30000,
            prepare_inputs: false,
            prepare_execution: false,
            history_after_lost_execution: true,
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
        p.validate().unwrap();
    }
    #[test]
    fn history_probe_accepts_only_explicit503_and_never_success_or_other_errors() {
        let expected: anyhow::Error = HttpStatus(reqwest::StatusCode::SERVICE_UNAVAILABLE).into();
        expected_lost_execution_reply(Some(&expected)).unwrap();
        let other: anyhow::Error = HttpStatus(reqwest::StatusCode::CONFLICT).into();
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
