use super::*;

pub(super) fn sandbox(
    args: SandboxRunArgs,
    sandbox_agent: Option<aq_session_host::users::AgentIdentity>,
) -> ExitCode {
    let SandboxRunArgs {
        manifest,
        state_dir,
        permit_file,
        server_url,
    } = args;
    let result = (|| -> anyhow::Result<()> {
        let _agent =
            sandbox_agent.ok_or_else(|| anyhow::anyhow!("sandbox user bootstrap required"))?;
        let frozen = aq_worker::native::runner::sandbox::FrozenLaunch::load(&manifest)?;
        frozen.validate_local(&state_dir)?;
        let _http = aq_worker::native::runner::sandbox::auth::RunnerHttp::for_execution(
            &server_url,
            &permit_file,
            &frozen,
        )?;
        anyhow::bail!(
            "sandbox runner B2 receipt codec is unavailable; permit transport validated, no launch attempted"
        );
    })();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => usage_err(e),
    }
}

pub(super) fn definitions() -> ExitCode {
    let recipe = aq_worker::native::adapter::claude::docker_local_recipe_artifact();
    let codex = aq_codex_mapper::docker_local::recipe_artifact();
    println!(
        "{}",
        serde_json::json!({
            "recipe": {
                "id": recipe.id.as_str(),
                "version": recipe.version.as_str(),
                "digest": recipe.digest.as_str(),
                "body": aq_worker::native::adapter::claude::docker_local_recipe_body(),
            },
            // Codex in the same docker-local sandbox (harness `codex`).
            "codex_recipe": {
                "id": codex.id.as_str(),
                "version": codex.version.as_str(),
                "digest": codex.digest.as_str(),
                "body": aq_codex_mapper::docker_local::recipe_body(),
            },
            "contract": aq_agent_result_local::artifact(),
            "startup": aq_agent_result_local::startup_artifact(),
        })
    );
    ExitCode::SUCCESS
}

pub(super) async fn harness(driver_config: PathBuf) -> ExitCode {
    match aq_worker::sandbox_driver::run(&driver_config).await {
        Ok(0) => ExitCode::SUCCESS,
        Ok(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
        Err(e) => {
            println!("aq-sandbox-harness: refused: {e:#}");
            ExitCode::from(2)
        }
    }
}

pub(super) fn check(path: PathBuf, native_config: PathBuf) -> ExitCode {
    let disposition = "native configuration check refused";
    let worker = match WorkerConfig::load(&path) {
        Ok(worker) => worker,
        Err(error) => return native_diagnostics::refuse(&error, None, disposition),
    };
    let result = (|| -> anyhow::Result<()> {
        let config = aq_worker::native::runner::NativeConfig::load(&native_config)?;
        config.validate()?;
        if claude_docker(&config) {
            let mut executor =
                aq_worker::native::runner::ClaudeDockerExecutor::configured(&worker)?;
            return aq_worker::native::runner::NativeExecutor::preflight(&mut executor, &config);
        }
        let mut executor = configured_native_executor(&worker, &config)?
            .ok_or_else(|| anyhow::anyhow!("native executor is not configured"))?;
        aq_worker::native::runner::NativeExecutor::preflight(&mut executor, &config)?;
        Ok(())
    })();
    match result {
        Ok(()) => {
            println!("Native configuration and installed executor are valid");
            ExitCode::SUCCESS
        }
        Err(error) => native_diagnostics::refuse(&error, Some(&worker), disposition),
    }
}

pub(super) async fn release(
    path: PathBuf,
    native_config: PathBuf,
    attempt_id: uuid::Uuid,
    reason: String,
) -> ExitCode {
    let result = async {
        let worker = WorkerConfig::load(&path)?;
        let config = aq_worker::native::runner::NativeConfig::load(&native_config)?;
        aq_worker::native::runner::release_preserved_sandbox(
            &config,
            worker.node_id,
            attempt_id,
            &reason,
        )
        .await
    }
    .await;
    match result {
        Ok(released) => {
            println!(
                "operator release: attempt {attempt_id} sandbox(es) {} destroyed; reason recorded as destroy evidence",
                released.join(", ")
            );
            ExitCode::SUCCESS
        }
        Err(error) => usage_err(format!("operator release refused: {error:#}")),
    }
}

pub(super) async fn reconcile(
    path: PathBuf,
    native_config: PathBuf,
    attempt_id: uuid::Uuid,
) -> ExitCode {
    let result = async {
        let worker = WorkerConfig::load(&path)?;
        let config = aq_worker::native::runner::NativeConfig::load(&native_config)?;
        aq_worker::native::runner::reconcile_native(&worker, &config, attempt_id).await
    }
    .await;
    match result {
        Ok(disposition) => {
            println!(
                "native reconciliation: {attempt_id} {disposition:?}; retained state preserved; server settlement/release is separate"
            );
            ExitCode::SUCCESS
        }
        Err(error) => usage_err(format!(
            "native reconciliation failed: {error:#}; preserve state and retry the same attempt"
        )),
    }
}

pub(super) async fn run(args: RunNativeArgs, path: PathBuf) -> ExitCode {
    let disposition =
        "native execution unavailable or outcome unknown; preserve state for reconciliation";
    let worker = match WorkerConfig::load(&path) {
        Ok(worker) => worker,
        Err(error) => return native_diagnostics::refuse(&error, None, disposition),
    };
    let result = run_configured(&worker, args).await;
    match result {
        Ok(disposition) => {
            println!("native run: {disposition:?}; retained state preserved");
            ExitCode::SUCCESS
        }
        Err(error) => native_diagnostics::refuse(&error, Some(&worker), disposition),
    }
}

use aq_worker::native::runner::{self, NativeConfig, NativeExecutor, RunDisposition};

async fn run_configured(
    worker: &WorkerConfig,
    args: RunNativeArgs,
) -> anyhow::Result<RunDisposition> {
    let RunNativeArgs {
        native_config,
        #[cfg(feature = "test-support")]
        development_loopback,
        #[cfg(feature = "test-support")]
        development_provider,
        #[cfg(feature = "test-support")]
        fixture_preflight_gate,
        #[cfg(feature = "test-support")]
        fixture_replay_lost_b1_ack,
    } = args;
    #[cfg(feature = "test-support")]
    let fixture = native_fixture::Options::from_flags(
        development_loopback,
        development_provider,
        fixture_preflight_gate,
        fixture_replay_lost_b1_ack,
    )?;
    #[cfg(feature = "test-support")]
    if fixture.is_some() {
        native_fixture::verify_environment()?;
    }
    let config =
        aq_worker::native::runner::NativeConfig::load(&native_config).inspect_err(|_| {
            tracing::info!(stage = "native_config_load", "native execution refused");
        })?;

    if claude_docker(&config) {
        let mut executor = runner::ClaudeDockerExecutor::configured(worker)?;
        return run_until_shutdown(worker, config, &mut executor).await;
    }
    #[cfg(feature = "test-support")]
    if let Some(options) = fixture {
        let inner = options.executor(worker)?;
        let mut executor =
            native_fixture::FixtureExecutor::new(inner, options.gate, options.replay_lost_b1_ack);
        return run_until_shutdown(worker, config, &mut executor).await;
    }
    match configured_native_executor(worker, &config)? {
        Some(mut executor) => run_until_shutdown(worker, config, &mut executor).await,
        None => run_until_shutdown(worker, config, &mut runner::UnavailableExecutor).await,
    }
}
async fn run_until_shutdown<E: NativeExecutor>(
    worker: &WorkerConfig,
    config: NativeConfig,
    executor: &mut E,
) -> anyhow::Result<RunDisposition> {
    let (watcher, shutdown) = shutdown_watch();
    let result = runner::run_once(worker, config, executor, shutdown).await;
    watcher.abort();
    result
}
