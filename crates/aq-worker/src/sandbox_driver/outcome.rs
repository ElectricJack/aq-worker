use super::*;
impl Drove {
    pub(super) fn handoff_note(&self, secrets: &[Zeroizing<Vec<u8>>]) -> Option<String> {
        let reason = self.handoff.filter(|_| self.leak.is_none())?;
        Some(aq_handoff_note::handoff_note_for_login(
            reason,
            &self.changed,
            &self.stream,
            secrets,
        ))
    }
    pub(super) fn write_transcript(&self, config_path: &Path, config: &DriverConfig) {
        if self.leak.is_none() {
            aq_handoff_note::write_transcript(
                config_path,
                config.harness.transcript_file(),
                &self.stream,
            );
        }
    }
    /// Writes the exit record, then the claim (the node may stop this pane as soon as the claim
    /// is recorded), and returns the driver's exit code.
    pub(super) fn claim(
        &self,
        config_path: &Path,
        config: &DriverConfig,
        note: Option<String>,
    ) -> i32 {
        let Self {
            rc,
            changed,
            stream,
            leak,
            ..
        } = self;
        let (rc, leak) = (*rc, *leak);
        if let Some(note) = note {
            // The work is in the node worktree; the executor sees a recorded
            // failure and the pipeline checkpoints, retains and reports it.
            if matches!(
                self.handoff,
                Some(aq_handoff_note::Stop::Handoff(
                    aq_protocol::HandoffReason::ServiceStop
                ))
            ) {
                aq_handoff_note::write_platform_failure(config_path);
            } else {
                write_exit(config_path, false);
            }
            submit_claim(config, note);
            return 4;
        }
        let proven = match leak {
            Some(place) => Err(place),
            None => proven_success(stream, rc, changed),
        };
        if let Err(reason) = proven {
            println!(
                "aq-sandbox-harness: success not proven ({reason}); the result is reported as failed"
            );
        }
        write_exit(config_path, proven.is_ok());
        let on = config.agent_channel && leak.is_none();
        let root = &config.docker_local.channel_root;
        let posting = on && root.is_some();
        submit_claim(config, self.conclusion(config, posting));
        // R2-M2: after the claim, best effort and bounded, so it never delays the result.
        final_message::post_from_stream(on, root, config.owner.attempt_id, stream);
        if proven.is_ok() { 0 } else { 1 }
    }
    fn conclusion(&self, config: &DriverConfig, posting: bool) -> String {
        match self.leak {
            // Never forward the agent's own text when the token leaked.
            Some(place) => {
                format!("Changes: none (copy-back refused); Refused: {place}; the attempt failed")
            }
            None if config.harness.is_claude() => {
                conclusion(&self.stream, self.rc, &self.changed, posting)
            }
            None => conclusion_of(
                config.harness.label(),
                &self.stream,
                self.rc,
                &self.changed,
                posting,
            ),
        }
    }
}
