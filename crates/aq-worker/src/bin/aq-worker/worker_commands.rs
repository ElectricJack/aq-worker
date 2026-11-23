use super::*;

pub(super) fn template(command: TemplateCmd) -> ExitCode {
    let args = match command {
        TemplateCmd::Build {
            recipe,
            build_config,
            output,
        } => vec![
            "build".into(),
            recipe.into_os_string(),
            "--build-config".into(),
            build_config.into_os_string(),
            "--output".into(),
            output.into_os_string(),
        ],
        TemplateCmd::Scan { archive } => vec!["scan".into(), archive.into_os_string()],
        #[cfg(feature = "project_environments")]
        TemplateCmd::EnvironmentBuild {
            job,
            repo,
            build_config,
            output,
        } => vec![
            "environment-build".into(),
            "--job".into(),
            job.into_os_string(),
            "--repo".into(),
            repo.into_os_string(),
            "--build-config".into(),
            build_config.into_os_string(),
            "--output".into(),
            output.into_os_string(),
        ],
        #[cfg(feature = "project_environments")]
        TemplateCmd::EnvironmentTry {
            job,
            repo,
            build_config,
        } => vec![
            "environment-try".into(),
            "--job".into(),
            job.into_os_string(),
            "--repo".into(),
            repo.into_os_string(),
            "--build-config".into(),
            build_config.into_os_string(),
        ],
    };
    match aq_worker::template::run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => usage_err("template operation refused; Python 3 and valid inputs required"),
    }
}

#[cfg(feature = "synthetic-runtime")]
pub(super) async fn execution(args: RunExecutionArgs, path: PathBuf) -> ExitCode {
    let RunExecutionArgs {
        state_dir,
        fixture_agent,
        max_agents,
        idle_timeout_secs,
        exit_after_result,
        drop_acceptance_response_once,
        drop_report_response_once,
        fault_checkpoint,
        fault_action,
        aq_git_binary,
        aq_git_sha256,
        aq_git_state_dir,
        git_executable,
        source_max_bundle_bytes,
        source_max_expanded_bytes,
        source_transfer,
    } = args;
    let result = async {
        if let Some(name) = fault_checkpoint {
            aq_worker::fault::configure(name, fault_action)?;
        }
        let cfg = WorkerConfig::load(&path)?;
        aq_worker::driver::run_execution(
            &cfg,
            &state_dir,
            &fixture_agent,
            aq_worker::driver::DriverOptions {
                source: aq_git_binary.map(|binary| aq_worker::driver::SourceOptions {
                    binary,
                    binary_sha256: aq_git_sha256.unwrap(),
                    state_dir: aq_git_state_dir.unwrap(),
                    git_executable,
                    max_bundle_bytes: source_max_bundle_bytes,
                    max_expanded_bytes: source_max_expanded_bytes,
                    transfer: source_transfer,
                }),
                max_agents,
                idle_timeout_secs,
                exit_after_result,
                drop_acceptance_response_once,
                drop_report_response_once,
            },
        )
        .await
    }
    .await;
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => usage_err(format!("{e:#}")),
    }
}

pub(super) async fn enroll(args: EnrollArgs, path: PathBuf) -> ExitCode {
    let EnrollArgs {
        server,
        name,
        cf_service_token,
        runner_access_token,
    } = args;
    let Some(name) = name.or_else(hostname) else {
        return usage_err("cannot determine hostname; pass --name");
    };
    let secrets = match read_secrets(cf_service_token, runner_access_token) {
        Ok(s) => s,
        Err(e) => return usage_err(format!("{e:#}")),
    };
    let (cf_id, cf_secret) = match secrets.cf {
        Some((i, s)) => (Some(i), Some(s)),
        None => (None, None),
    };
    let client = match Client::new(&server, None, cf_id.clone(), cf_secret.clone()) {
        Ok(c) => c,
        Err(e) => return usage_err(format!("{e:#}")),
    };
    let resp = client
        .enroll(&EnrollRequest {
            code: secrets.code,
            name,
            worker_version: env!("AQ_WORKER_VERSION").to_string(),
            protocol_version: PROTOCOL_VERSION,
        })
        .await;
    match resp {
        Ok(r) => {
            let cfg = WorkerConfig {
                server_url: server,
                node_id: r.node_id,
                credential: r.credential,
                cf_client_id: cf_id,
                cf_client_secret: cf_secret,
            };
            save_enrollment(cfg, secrets.runner, path)
                .unwrap_or_else(|error| usage_err(format!("{error:#}")))
        }
        Err(e) => {
            eprintln!("error: {e}");
            exit_for(&e)
        }
    }
}

fn save_enrollment(
    cfg: WorkerConfig,
    runner: Option<RunnerAccess>,
    path: PathBuf,
) -> anyhow::Result<ExitCode> {
    cfg.save(&path)?;
    println!("enrolled as node {}", cfg.node_id);
    println!("config saved to {}", path.display());
    if let Some(runner) = runner {
        let runner_path = runner_access_path(&path);
        runner.save(&runner_path)?;
        println!("runner Access token saved to {}", runner_path.display());
    }
    Ok(ExitCode::SUCCESS)
}
pub(super) fn status(path: PathBuf) -> ExitCode {
    match WorkerConfig::load(&path) {
        Ok(config) => {
            println!("node id: {}", config.node_id);
            println!("server:  {}", config.server_url);
            ExitCode::SUCCESS
        }
        Err(error) => usage_err(format!("{error:#}")),
    }
}
