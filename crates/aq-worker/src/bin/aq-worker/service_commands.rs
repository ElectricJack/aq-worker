use super::*;

pub(super) async fn backfill(
    path: PathBuf,
    controller_config: PathBuf,
    facts: PathBuf,
) -> ExitCode {
    let result: anyhow::Result<()> = async {
        use aq_worker::backend::boat::{BoatBackend, BoatKey, VmTimeFact};
        use std::io::Read;
        let worker = WorkerConfig::load(&path)?;
        let config = aq_worker::controller::RunConfig::load(&controller_config)?;
        let boat = config
            .boat
            .ok_or_else(|| anyhow::anyhow!("backfill requires Boat backend"))?;
        anyhow::ensure!(
            boat.node_id == worker.node_id,
            "boat node identity differs from enrollment"
        );
        let key = BoatKey::load(&boat.key_file)?;
        let mut backend =
            BoatBackend::connect(boat, key)?.with_vm_time_client(Client::from_config(&worker)?);
        let mut bytes = Vec::new();
        std::fs::File::open(facts)?
            .take(1_048_577)
            .read_to_end(&mut bytes)?;
        anyhow::ensure!(bytes.len() <= 1_048_576, "backfill file too large");
        let facts: Vec<VmTimeFact> = serde_json::from_slice(&bytes)?;
        for fact in facts {
            backend.backfill_vm_time(fact)?;
        }
        backend.replay_vm_time().await?;
        anyhow::ensure!(
            backend.pending_vm_time_reports() == 0,
            "VM-time facts journaled but delivery pending; rerun to retry"
        );
        Ok(())
    }
    .await;
    match result {
        Ok(()) => {
            println!("Boat VM-time backfill recorded");
            ExitCode::SUCCESS
        }
        Err(e) => usage_err(format!("{e:#}")),
    }
}

pub(super) async fn checks(path: PathBuf, checks_config: PathBuf) -> ExitCode {
    let result: anyhow::Result<()> = async {
        let worker = WorkerConfig::load(&path)?;
        let config = aq_worker::checks::ChecksConfig::load(&checks_config)?;
        let (watcher, rx) = shutdown_watch();
        let result = aq_worker::checks::run(&worker, config, rx).await;
        watcher.abort();
        result
    }
    .await;
    service_exit(result)
}

/// Claims and runs environment build and try jobs on a template-build node (spec 0017).
#[cfg(feature = "project_environments")]
pub(super) async fn environments(path: PathBuf, environments_config: PathBuf) -> ExitCode {
    let result: anyhow::Result<()> = async {
        let worker = WorkerConfig::load(&path)?;
        let config = aq_environment_builder::EnvironmentsConfig::load(&environments_config)?;
        let server = aq_environment_builder::Server {
            url: worker.server_url.clone(),
            credential: worker.credential.clone(),
            cf_access: (worker.cf_client_id.clone()).zip(worker.cf_client_secret.clone()),
            max_reply_bytes: config.max_reply_bytes,
        };
        let (watcher, rx) = shutdown_watch();
        let tool = aq_worker::template::command;
        let result = aq_environment_builder::run(&server, config, rx, tool).await;
        watcher.abort();
        result
    }
    .await;
    service_exit(result)
}

pub(super) async fn controller(path: PathBuf, controller_config: PathBuf) -> ExitCode {
    service_exit(run_controller(path, controller_config).await)
}

async fn run_controller(path: PathBuf, controller_config: PathBuf) -> anyhow::Result<()> {
    use aq_worker::backend::{
        SelectedBackend,
        boat::{BoatBackend, BoatKey},
        docker_local::DockerLocalBackend,
    };
    use aq_worker::controller::{Controller, RunConfig, UnavailableProvider};
    let worker = WorkerConfig::load(&path)?;
    let config = RunConfig::load(&controller_config)?;
    let client = Client::from_config(&worker)?;
    let backend = match (config.docker_local, config.boat) {
        (Some(docker), None) => {
            SelectedBackend::Docker(Box::new(DockerLocalBackend::connect_unix(docker).await?))
        }
        (None, Some(boat)) => {
            anyhow::ensure!(
                boat.node_id == worker.node_id,
                "boat node identity differs from enrollment"
            );
            let key = BoatKey::load(&boat.key_file)?;
            SelectedBackend::Boat(Box::new(
                BoatBackend::connect(boat, key)?.with_vm_time_client(client.clone()),
            ))
        }
        _ => anyhow::bail!("configure exactly one placement backend"),
    };
    let mut controller = Controller::open(
        &config.state_dir,
        worker.node_id,
        config.node,
        backend,
        UnavailableProvider,
    )?;
    let machine_reporting = aq_machine_load::sampler_from_file(
        &controller_config.with_file_name("machine.json"),
        config.state_dir.clone(),
    );
    let machine_client = client.clone();
    let (watcher, rx) = shutdown_watch();
    let machine_shutdown = rx.clone();
    let machine = machine_reporting.map(|(sampler, interval)| {
        tokio::spawn(aq_machine_load::report_loop(
            sampler,
            interval,
            move |report| {
                let c = machine_client.clone();
                async move {
                    c.post::<_, serde_json::Value>("/v1/worker/machine", &report, true)
                        .await
                        .map(|_| ())
                        .map_err(|e| e.to_string())
                }
            },
            wait_shutdown(machine_shutdown),
        ))
    });
    let heartbeat_shutdown = rx.clone();
    tracing::info!(node_id = %controller.identity().node_id, boot_id = %controller.identity().boot_id, "node controller starting");
    let result = controller
        .run_with_heartbeat(rx, run_loop(&client, wait_shutdown(heartbeat_shutdown)))
        .await;
    watcher.abort();
    if let Some(machine) = machine {
        machine.abort();
    }
    result
}

async fn wait_shutdown(mut shutdown: tokio::sync::watch::Receiver<bool>) {
    while !*shutdown.borrow() {
        if shutdown.changed().await.is_err() {
            break;
        }
    }
}
pub(super) fn service_exit(result: anyhow::Result<()>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let (message, code) = run_error(&e);
            eprintln!("error: {message}");
            code
        }
    }
}

pub(super) async fn run(
    path: PathBuf,
    controller_config: Option<PathBuf>,
    service_config: Option<PathBuf>,
) -> ExitCode {
    match (service_config, controller_config) {
        (Some(service), _) => service_exit(service::run_service(&service).await),
        (None, Some(controller_config)) => controller(path, controller_config).await,
        (None, None) => usage_err("pass --controller-config or --service-config"),
    }
}
