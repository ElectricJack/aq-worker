//! Test-support transport and pre-admission observation around the ordinary runner.
//! This module is absent from production builds and cannot mint live authority.
use anyhow::{Context, Result, ensure};
use aq_worker::{
    config::WorkerConfig,
    native::{
        preparation::{DurablePhaseExecution, InputPipeline},
        runner::{CodexExecutor, ExecutionFuture, NativeConfig, NativeExecutor},
    },
};
use serde::Deserialize;
use std::{io::Write, path::Path, sync::Arc, time::Instant};
use tokio::sync::watch;

#[path = "../../native/journal/codex_process_guard.rs"]
mod confinement;
mod replay;

pub(super) struct Options {
    provider: String,
    pub(super) gate: bool,
    pub(super) replay_lost_b1_ack: bool,
}
impl Options {
    pub(super) fn from_flags(
        local: bool,
        provider: Option<String>,
        gate: bool,
        replay_lost_b1_ack: bool,
    ) -> Result<Option<Self>> {
        if !local {
            ensure!(
                provider.is_none() && !gate && !replay_lost_b1_ack,
                "fixture options require local transport"
            );
            return Ok(None);
        }
        ensure!(
            !replay_lost_b1_ack || gate,
            "fixture replay requires explicit preflight gate"
        );
        let provider = provider.context("local fixture requires explicit provider origin")?;
        let url = reqwest::Url::parse(&provider)?;
        ensure!(
            url.scheme() == "http"
                && url.host_str() == Some("127.0.0.1")
                && url.port().is_some_and(|p| p != 0)
                && url.username().is_empty()
                && url.password().is_none()
                && url.path() == "/"
                && url.query().is_none()
                && url.fragment().is_none(),
            "fixture provider must be an explicit numeric loopback origin"
        );
        Ok(Some(Self {
            provider,
            gate,
            replay_lost_b1_ack,
        }))
    }
    pub(super) fn executor(&self, worker: &WorkerConfig) -> Result<CodexExecutor> {
        CodexExecutor::development_loopback(worker)?.with_development_provider(&self.provider)
    }
}
pub(super) fn verify_environment() -> Result<()> {
    confinement::require_confined_binary(Path::new("/opt/aq-qualification/aq-worker"))
}

pub(super) struct FixtureExecutor {
    inner: CodexExecutor,
    gate: bool,
    replay_lost_b1_ack: bool,
}
impl FixtureExecutor {
    pub(super) fn new(inner: CodexExecutor, gate: bool, replay_lost_b1_ack: bool) -> Self {
        Self {
            inner,
            gate,
            replay_lost_b1_ack,
        }
    }
}
impl NativeExecutor for FixtureExecutor {
    fn preflight(&mut self, config: &NativeConfig) -> Result<()> {
        ensure!(!self.replay_lost_b1_ack || (self.gate && matches!(&config.executor,
            Some(aq_worker::native::runner::ExecutorConfig::TrustedNodeNamedReadonlyResultLocal { .. }))),
            "fixture B1 replay requires explicit result-local executor");
        self.inner.preflight(config)?;
        if self.gate {
            // The actual installed factory has returned successfully. This data
            // witnesses that boundary; it grants neither admission nor execution.
            let ready = serde_json::json!({
                "schema_version":1,"phase":"native_preflight_ready",
                "worker_version":env!("CARGO_PKG_VERSION"),
                "runtime":config.runtime.runtime,"qualification":config.runtime.qualification,
                "negotiation":config.negotiation,"adapter":config.adapter,
                "adapter_manifest_digest":config.adapter_manifest_digest,
            });
            let mut output = std::io::stdout().lock();
            serde_json::to_writer(&mut output, &ready)?;
            output.write_all(b"\n")?;
            output.flush()?;
            drop(output);
            continue_preflight()?;
        }
        Ok(())
    }
    fn execute<'a>(
        &'a mut self,
        pipeline: Arc<InputPipeline>,
        grant: DurablePhaseExecution,
        shutdown: watch::Receiver<bool>,
    ) -> ExecutionFuture<'a> {
        let replay_enabled = std::mem::take(&mut self.replay_lost_b1_ack);
        let id = grant.history().attempt_id();
        let retained = pipeline.clone();
        let execution = self.inner.execute(pipeline, grant, shutdown);
        Box::pin(async move {
            replay::preserve_execution_error(execution.await, replay_enabled, || {
                replay::once(&retained, id)
            })
            .await
        })
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Continuation {
    schema_version: u32,
    r#continue: bool,
}
fn validate_continuation(bytes: &[u8]) -> Result<()> {
    ensure!(bytes.len() <= 1024, "bounded preflight continuation");
    let value: Continuation = serde_json::from_slice(bytes)?;
    ensure!(
        value.schema_version == 1 && value.r#continue,
        "preflight continuation refused"
    );
    Ok(())
}
fn continue_preflight() -> Result<()> {
    use std::io::IsTerminal;
    ensure!(
        !std::io::stdin().is_terminal(),
        "preflight gate requires a fixture pipe"
    );
    // This is before any registration or reservation. It creates no grant and
    // cannot extend a selected offer or an execution deadline.
    read_continuation(libc::STDIN_FILENO, std::time::Duration::from_secs(30))
}
fn read_continuation(input: std::os::fd::RawFd, maximum: std::time::Duration) -> Result<()> {
    let deadline = Instant::now() + maximum;
    let mut bytes = Vec::new();
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .context("preflight continuation timed out")?;
        let timeout = i32::try_from(remaining.as_millis().max(1)).unwrap_or(i32::MAX);
        let mut fd = libc::pollfd {
            fd: input,
            events: libc::POLLIN,
            revents: 0,
        };
        let polled = unsafe { libc::poll(&mut fd, 1, timeout) };
        if polled < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error.into());
        }
        ensure!(
            polled > 0 && fd.revents & libc::POLLIN != 0,
            "preflight continuation absent"
        );
        let mut chunk = [0u8; 1025];
        let read = unsafe { libc::read(input, chunk.as_mut_ptr().cast(), chunk.len()) };
        ensure!(read > 0, "preflight continuation closed");
        bytes.extend_from_slice(&chunk[..read as usize]);
        ensure!(bytes.len() <= 1024, "bounded preflight continuation");
        if let Some(end) = bytes.iter().position(|b| *b == b'\n') {
            ensure!(
                bytes[end + 1..].iter().all(u8::is_ascii_whitespace),
                "one preflight continuation only"
            );
            return validate_continuation(&bytes[..end]);
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn actual_cli_requires_explicit_local_transport_for_provider_flags() {
        use clap::Parser;
        assert!(
            crate::Cli::try_parse_from([
                "aq-worker",
                "run-native",
                "--native-config",
                "/nonexistent",
                "--development-provider",
                "http://127.0.0.1:4321/",
            ])
            .is_err()
        );
        let parsed = crate::Cli::try_parse_from([
            "aq-worker",
            "run-native",
            "--native-config",
            "/nonexistent",
            "--development-loopback",
            "--development-provider",
            "http://127.0.0.1:4321/",
            "--fixture-preflight-gate",
        ])
        .unwrap();
        assert!(matches!(
            parsed.command,
            crate::Cmd::RunNative {
                development_loopback: true,
                development_provider: Some(_),
                fixture_preflight_gate: true,
                ..
            }
        ));
    }
    #[test]
    fn lost_ack_replay_flag_requires_explicit_local_preflight_fixture() {
        use clap::Parser;
        let base = [
            "aq-worker",
            "run-native",
            "--native-config",
            "/nonexistent",
            "--development-loopback",
            "--development-provider",
            "http://127.0.0.1:4321/",
        ];
        let mut args = base.to_vec();
        args.push("--fixture-replay-lost-b1-ack");
        assert!(crate::Cli::try_parse_from(&args).is_err());
        args.push("--fixture-preflight-gate");
        let parsed = crate::Cli::try_parse_from(args).unwrap();
        assert!(matches!(
            parsed.command,
            crate::Cmd::RunNative {
                fixture_replay_lost_b1_ack: true,
                ..
            }
        ));
        assert!(Options::from_flags(false, None, false, true).is_err());
        assert!(
            Options::from_flags(true, Some("http://127.0.0.1:4321/".into()), false, true).is_err()
        );
        assert!(
            Options::from_flags(true, Some("http://127.0.0.1:4321/".into()), true, true)
                .unwrap()
                .unwrap()
                .replay_lost_b1_ack
        );
    }
    #[test]
    fn numeric_node_origin_alone_never_enables_fixture_provider() {
        assert!(Options::from_flags(true, None, false, false).is_err());
        assert!(
            Options::from_flags(false, Some("http://127.0.0.1:1/".into()), false, false).is_err()
        );
        assert!(Options::from_flags(false, None, true, false).is_err());
        assert!(
            Options::from_flags(false, None, false, false)
                .unwrap()
                .is_none()
        );
    }
    #[test]
    fn only_exact_numeric_loopback_provider_is_accepted() {
        assert!(
            Options::from_flags(true, Some("http://127.0.0.1:4321/".into()), true, false).is_ok()
        );
        for origin in [
            "https://127.0.0.1:1/",
            "http://localhost:1/",
            "http://127.0.0.1/",
            "http://127.0.0.1:0/",
            "http://127.0.0.1:1/v1",
            "http://user@127.0.0.1:1/",
            "http://127.0.0.1:1/?a=1",
            "http://127.0.0.1:1/#a",
        ] {
            assert!(
                Options::from_flags(true, Some(origin.into()), false, false).is_err(),
                "{origin}"
            );
        }
    }
    #[test]
    fn partial_preflight_pipe_times_out_without_waiting_for_a_newline() {
        use std::os::fd::AsRawFd;
        let (mut writer, reader) = std::os::unix::net::UnixStream::pair().unwrap();
        writer.write_all(b"{\"schema_version\":1").unwrap();
        let started = Instant::now();
        assert!(
            read_continuation(reader.as_raw_fd(), std::time::Duration::from_millis(20)).is_err()
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }
    #[test]
    fn preflight_pipe_accepts_one_frame_and_refuses_oversize_or_extra_frames() {
        use std::os::fd::AsRawFd;
        for (bytes, expected) in [
            (b"{\"schema_version\":1,\"continue\":true}\n".to_vec(), true),
            (vec![b' '; 1025], false),
            (
                b"{\"schema_version\":1,\"continue\":true}\n{}\n".to_vec(),
                false,
            ),
        ] {
            let (mut writer, reader) = std::os::unix::net::UnixStream::pair().unwrap();
            writer.write_all(&bytes).unwrap();
            assert_eq!(
                read_continuation(reader.as_raw_fd(), std::time::Duration::from_secs(1)).is_ok(),
                expected
            );
        }
    }
    #[test]
    fn preflight_continuation_is_closed_bounded_and_explicit() {
        assert!(validate_continuation(br#"{"schema_version":1,"continue":true}"#).is_ok());
        for value in [
            br#"{"schema_version":2,"continue":true}"#.as_slice(),
            br#"{"schema_version":1,"continue":false}"#.as_slice(),
            br#"{"schema_version":1,"continue":true,"authority":"grant"}"#.as_slice(),
        ] {
            assert!(validate_continuation(value).is_err());
        }
        assert!(validate_continuation(&vec![b' '; 1025]).is_err());
    }
}
