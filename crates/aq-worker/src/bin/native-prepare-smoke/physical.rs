use super::*;
use aq_admission::startup::NativeOffer;
use aq_worker::native::preparation::DurablePhaseExecution;

pub(super) struct Fixture<'a> {
    pub config: &'a Config,
    pub offer: &'a NativeOffer,
    pub held: &'a File,
    pub pipeline: &'a Arc<InputPipeline>,
    pub id: Uuid,
    pub grant: &'a DurablePhaseExecution,
    pub launch_id: Uuid,
    pub report: bool,
    pub relay: bool,
    pub refresh: bool,
    pub result: &'a mut serde_json::Value,
}
impl Fixture<'_> {
    fn profile(&self, host: HostConfig) -> Result<SyntheticHostProfile> {
        let host_root = self.config.root.join("host");
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o700).create(&host_root)?;
        self.held.sync_all()?;
        SyntheticHostProfile::new(SyntheticHostConfig {
            root: host_root,
            helper: host.helper.into(),
            tmux: host.tmux.into(),
            program: host.program.into(),
            harness: self.offer.offered.offer.manifest.harness.clone(),
            capabilities: self.config.capabilities.clone(),
            renderer: host.renderer,
            columns: 80,
            rows: 24,
            mode: SyntheticMode::Wait,
        })
    }
    async fn wait_for_marker(&self, launched: &NativeHostLaunchResult) {
        // Positive exec does not mean the synthetic program has
        // finished its first write. Give the fixture a bounded
        // observation window; this never extends the host permit.
        if matches!(launched, NativeHostLaunchResult::Running(_)) {
            let marker = self
                .config
                .root
                .join("host")
                .join(self.launch_id.to_string());
            for _ in 0..50 {
                if marker.join("fixture-marker").is_file()
                    && marker.join("fixture-marker.cwd").is_file()
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }
    }
    async fn output(
        &self,
        launched: &NativeHostLaunchResult,
        input: &mut impl BufRead,
        stage: &mut &'static str,
    ) -> Result<Option<serde_json::Value>> {
        if !self.relay {
            return Ok(None);
        }
        let NativeHostLaunchResult::Running(host) = launched else {
            anyhow::bail!("output probe requires actual Running")
        };
        let probe = output::OutputProbe {
            pipeline: self.pipeline,
            host,
            config: &self.config.worker,
            id: self.id,
        };
        if self.refresh {
            probe.run_refresh(input, stage).await.map(Some)
        } else {
            probe.run(input, stage).await.map(Some)
        }
    }
    async fn stop(&self, stage: &mut &'static str) -> Result<()> {
        // Stop before inspecting/serializing the final result, including
        // a failed relay operation. The original guardian cap remains.
        *stage = "phase_host_stop";
        ensure!(
            matches!(
                self.pipeline.stop_phase_host(self.id).await?,
                NativeStopOutcome::StoppedPendingJournal(_)
            ),
            "actual stop proof required"
        );
        self.pipeline.persist_phase_host_stop(self.id).await?;
        Ok(())
    }
    async fn report_terminal(&mut self, stage: &mut &'static str) -> Result<()> {
        let (pipeline, id) = (self.pipeline, self.id);
        *stage = "phase_started_report";
        self.result["phase_started_ack"] = if self.refresh {
            json!(pipeline.report_refresh_terminal_started(id).await?)
        } else {
            json!(pipeline.report_phase_terminal_started(id).await?)
        };
        *stage = "phase_stopped_report";
        self.result["phase_stopped_ack"] = if self.refresh {
            json!(pipeline.report_refresh_terminal_stopped(id).await?)
        } else {
            json!(pipeline.report_phase_terminal_stopped(id).await?)
        };
        self.result["terminal_ack_revision"] = json!(pipeline.snapshot().await?.revision);
        Ok(())
    }
    pub async fn run(
        &mut self,
        host: HostConfig,
        input: &mut impl BufRead,
        stage: &mut &'static str,
    ) -> Result<()> {
        *stage = "phase_host_profile";
        let profile = self.profile(host)?;
        *stage = "phase_host_launch";
        let launched = self
            .pipeline
            .launch_phase_synthetic(self.grant, profile)
            .await?;
        self.wait_for_marker(&launched).await;
        // Keep the output error until after physical stop and its durable record.
        let output = self.output(&launched, input, stage).await;
        let output_stage = *stage;
        self.stop(stage).await?;
        if output.is_err() {
            *stage = output_stage;
        }
        if let Some(value) = output? {
            self.result["phase_output"] = value;
        }
        let NativeHostLaunchResult::Running(host) = launched else {
            anyhow::bail!("synthetic host lacked positive Running proof");
        };
        self.result["host_started"] = json!(host.started_request()?);
        let snapshot = self.pipeline.snapshot().await?;
        self.result["host_revision"] = json!(snapshot.revision);
        self.result["host_stopped"] = json!(
            snapshot.attempts[&self.id]
                .native_host
                .as_ref()
                .context("native host history absent")?
                .stopped
        );
        if self.report {
            self.report_terminal(stage).await?;
        }
        Ok(())
    }
}
