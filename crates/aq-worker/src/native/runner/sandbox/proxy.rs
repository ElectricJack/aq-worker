//! Fixed-upstream, bounded AQ forwarding (B6b). The agent supplies AQ
//! authorization; only this runner-owned listener supplies Cloudflare Access
//! authorization. The forwarding core is `aq_agent_channel` (shared with the
//! spec 0006 agent channel); this wrapper reads the runner-private Access file.
use super::private::RunnerFiles;
use anyhow::Result;
use aq_agent_channel::{AccessHeaders, LoopbackProxy};
use std::path::Path;
pub struct AqApiProxy(LoopbackProxy);
impl AqApiProxy {
    pub async fn start(server: &str, access_file: &Path) -> Result<Self> {
        let access = AccessHeaders::from_json(&RunnerFiles::read_secret(access_file)?)?;
        Ok(Self(LoopbackProxy::start(server, access).await?))
    }
    #[cfg(any(test, feature = "test-support", feature = "backend-conformance"))]
    pub async fn development_loopback(server: &str, access_file: &Path) -> Result<Self> {
        let access = AccessHeaders::from_json(&RunnerFiles::read_secret(access_file)?)?;
        Ok(Self(
            LoopbackProxy::development_loopback(server, access).await?,
        ))
    }
    pub fn origin(&self) -> &str {
        self.0.origin()
    }
    pub async fn stop(self) -> Result<()> {
        self.0.stop().await
    }
}
