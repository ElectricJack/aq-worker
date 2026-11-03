//! Scoped output tasks; none owns the execution/renewal future.
use super::{channels::SandboxChannels, output::SandboxOutput};
use aq_session_host::terminal_output::OutputObserver;
use std::sync::Arc;
use tokio::{sync::watch, task::JoinSet};

pub(super) struct OutputRuntime {
    tasks: JoinSet<()>,
    output: Arc<SandboxOutput>,
}

impl OutputRuntime {
    pub fn start(
        channels: Arc<SandboxChannels>,
        output: Arc<SandboxOutput>,
        shutdown: watch::Receiver<bool>,
    ) -> Self {
        let mut tasks = JoinSet::new();
        let upload = output.clone();
        let cancel = shutdown.clone();
        tasks.spawn(async move {
            let _ = upload.run(cancel).await;
        });
        let watch_output = output.clone();
        tasks.spawn(async move {
            tokio::select! {
                _ = async {
                    loop {
                        let _ = channels.run_output(watch_output.clone()).await;
                        if channels.authority_lost() { break; }
                        // The outer select cancels both held polls and retries.
                        // Rejected exchanges never advance the shared cursor.
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    }
                } => {},
                _ = async { let mut cancel = shutdown; let _ = cancel.wait_for(|v| *v).await; } => {},
            }
        });
        Self { tasks, output }
    }
    pub fn drain(&mut self, observer: OutputObserver, shutdown: watch::Receiver<bool>) {
        let output = self.output.clone();
        self.tasks.spawn(async move {
            let _ = output.drain(observer, shutdown).await;
        });
    }
    pub async fn stop(mut self) {
        self.tasks.abort_all();
        while self.tasks.join_next().await.is_some() {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::runner::sandbox::{auth_tests::client, channels::TerminalDemand};
    use crate::native::terminal_output::TerminalOutput;
    use aq_session_protocol::{TerminalHost, TerminalIdentity, TerminalPolicy};
    use axum::{Router, http::StatusCode, response::IntoResponse};
    use serde_json::{Value, json};
    use std::sync::Mutex;
    use tokio::sync::Notify;
    use uuid::Uuid;

    #[tokio::test]
    async fn h7_review_watch_recovers_at_same_cursor_after_transient_failure() {
        let _clock = crate::native::http::test_clock::TestClock::new();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let (_dir, http, _) = client(&origin, false);
        let terminal = TerminalIdentity {
            session: http.launch_binding().session,
            terminal_id: Uuid::new_v4(),
            epoch: 1,
        };
        let cursors = Arc::new(Mutex::new(Vec::new()));
        let seen = cursors.clone();
        let held = Arc::new(Notify::new());
        let arrived = held.clone();
        let watch_terminal = terminal.clone();
        let router = Router::new().fallback(move |axum::Json(q): axum::Json<Value>| {
            let n = {
                let mut seen = seen.lock().unwrap();
                seen.push(q["cursor"].as_u64().unwrap());
                seen.len()
            };
            let arrived = arrived.clone();
            let terminal = watch_terminal.clone();
            async move {
                if n == 1 { return StatusCode::BAD_GATEWAY.into_response(); }
                if n == 2 {
                    return axum::Json(json!({"version":1,"capability":"aq.terminal-relay","peer":q["peer"],
                        "exchange_id":Uuid::new_v4(),"cursor":1,
                        "effects":[{"kind":"watch","body":{"terminal":terminal,"subscription_id":Uuid::new_v4(),"principal_id":"viewer","expires_at_ms":10000}}]})).into_response();
                }
                arrived.notify_one();
                std::future::pending::<axum::response::Response>().await
            }
        });
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let channels = Arc::new(SandboxChannels::new(&http).unwrap());
        channels
            .set_demand(TerminalDemand {
                subscriptions: 1,
                controllers: 0,
            })
            .unwrap();
        let output = channels
            .output_from(
                TerminalOutput::tail_only(
                    terminal,
                    &TerminalPolicy {
                        host: TerminalHost::Tmux,
                        input_enabled: false,
                        max_input_bytes: 1,
                        max_pending_input_bytes: 1,
                        max_frame_bytes: 4,
                        replay_bytes: 8,
                    },
                )
                .unwrap(),
            )
            .unwrap();
        let (_cancel, shutdown) = watch::channel(false);
        let mut runtime = OutputRuntime::start(channels.clone(), output, shutdown);
        tokio::select! {
            _ = held.notified() => {},
            result = runtime.tasks.join_next() => panic!("output task stopped before Watch recovery: {result:?}"),
        }
        assert_eq!(*cursors.lock().unwrap(), vec![0, 0, 1]);
        runtime.stop().await;
        assert!(!channels.terminal_held());
        server.abort();
    }
}
