//! B6b `AqApiProxy`: a runner-owned loopback listener for a remote sandbox.
//! The agent supplies AQ authorization; only this listener supplies the
//! runner's Cloudflare Access headers. Spec 0006 §5.3 moves remote backends
//! to [`crate::AgentChannel`]-style credential injection; until then this
//! keeps the B6b behaviour exactly.
use crate::{
    Accept, AccessHeaders,
    forward::{Failure, Fixed, Origins, Rules, Seen, Upstream, forward},
};
use anyhow::Result;
use std::{sync::Arc, time::Duration};
use tokio::{
    io::AsyncWriteExt,
    net::{TcpListener, TcpStream},
    sync::watch,
    task::JoinHandle,
};

struct Loopback(TcpListener);
impl Accept for Loopback {
    type Stream = TcpStream;
    async fn accept(&mut self) -> std::io::Result<Option<TcpStream>> {
        let (stream, peer) = self.0.accept().await?;
        Ok(peer.ip().is_loopback().then_some(stream))
    }
}

pub struct LoopbackProxy {
    origin: String,
    stop: watch::Sender<bool>,
    task: Option<JoinHandle<()>>,
}
impl LoopbackProxy {
    /// Production: the upstream is an `https://` machine hostname.
    pub async fn start(server: &str, access: AccessHeaders) -> Result<Self> {
        Self::start_inner(server, Origins::MachineHostname, access).await
    }
    /// Fixtures: the upstream is `http://127.0.0.1:<port>`.
    pub async fn development_loopback(server: &str, access: AccessHeaders) -> Result<Self> {
        Self::start_inner(server, Origins::NumericLoopback, access).await
    }
    async fn start_inner(server: &str, origins: Origins, access: AccessHeaders) -> Result<Self> {
        let upstream = Upstream::new(server, origins, Duration::from_secs(25))?;
        let fixed = Fixed(
            access.headers()?,
            access
                .values()
                .iter()
                .map(|v| zeroize::Zeroizing::new(v.to_vec()))
                .collect(),
        );
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
        let local = listener.local_addr()?;
        let state = Arc::new((
            upstream,
            Rules {
                host: Some(local.to_string()),
                replace_authorization: false,
                agent_paths: false,
            },
            fixed,
        ));
        let (stop, stopped) = watch::channel(false);
        let task = tokio::spawn(crate::serve(
            Loopback(listener),
            stopped,
            16,
            Duration::ZERO,
            move |mut stream: TcpStream| {
                let state = state.clone();
                async move {
                    let (upstream, rules, fixed) = &*state;
                    let mut seen = Seen::default();
                    let result = tokio::time::timeout(
                        Duration::from_secs(30),
                        forward(upstream, rules, fixed, &mut stream, &mut seen),
                    )
                    .await;
                    match result {
                        Ok(Ok(done)) => {
                            let _ = tokio::time::timeout(
                                Duration::from_secs(5),
                                stream.write_all(&done.response),
                            )
                            .await;
                        }
                        Ok(Err(Failure::Refused(_) | Failure::Framing | Failure::Upstream))
                        | Err(_) => {
                            let _ = tokio::time::timeout(Duration::from_secs(1), stream.write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")).await;
                        }
                    }
                }
            },
        ));
        Ok(Self {
            origin: format!("http://{local}"),
            stop,
            task: Some(task),
        })
    }
    pub fn origin(&self) -> &str {
        &self.origin
    }
    pub async fn stop(mut self) -> Result<()> {
        let _ = self.stop.send(true);
        if let Some(task) = self.task.take() {
            task.await?;
        }
        Ok(())
    }
}
impl Drop for LoopbackProxy {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}
