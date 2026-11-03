//! Runner-held provider forwarding. Only the derived local capability is public.
use super::private::RunnerFiles;
use anyhow::{Result, ensure};
use aq_responses_transport::{
    ResponsesPolicy, RetainedUpstreamCredential, ScopedResponsesTransport, UpstreamEndpoint,
};
use std::{
    net::{Ipv4Addr, TcpListener},
    path::{Path, PathBuf},
    sync::Arc,
};
use zeroize::Zeroizing;
struct ProviderFile(PathBuf);
impl RetainedUpstreamCredential for ProviderFile {
    fn read_current(&self) -> Result<Zeroizing<Vec<u8>>> {
        let bytes = RunnerFiles::read_secret(&self.0)?;
        ensure!(
            bytes.len() <= 4096 && bytes.iter().all(|b| b.is_ascii_graphic()),
            "invalid runner provider key"
        );
        Ok(bytes)
    }
}
pub struct RunnerResponsesProxy(ScopedResponsesTransport);
impl RunnerResponsesProxy {
    pub async fn start(policy: ResponsesPolicy, provider_file: &Path) -> Result<Self> {
        Self::start_inner(policy, provider_file, UpstreamEndpoint::openai()).await
    }
    #[cfg(any(test, feature = "test-support", feature = "backend-conformance"))]
    pub async fn development_loopback(
        policy: ResponsesPolicy,
        provider_file: &Path,
        upstream: &str,
    ) -> Result<Self> {
        Self::start_inner(
            policy,
            provider_file,
            UpstreamEndpoint::development_loopback(upstream)?,
        )
        .await
    }
    async fn start_inner(
        policy: ResponsesPolicy,
        provider_file: &Path,
        upstream: UpstreamEndpoint,
    ) -> Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let origin = format!("http://{}", listener.local_addr()?);
        Ok(Self(
            ScopedResponsesTransport::start_on_listener(
                listener,
                &origin,
                policy,
                upstream,
                Arc::new(ProviderFile(provider_file.to_owned())),
            )
            .await?,
        ))
    }
    pub fn origin(&self) -> &str {
        self.0.origin()
    }
    pub fn local_capability(&self) -> Zeroizing<Vec<u8>> {
        self.0.local_capability()
    }
    pub async fn stop(self) -> Result<()> {
        self.0.stop().await
    }
}
