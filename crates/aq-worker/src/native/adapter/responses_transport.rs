//! Worker-specific retained launch/material joins. HTTP/JSON/SSE and lifetime
//! policy live in the independently tested aq-responses-transport component.
use super::*;
#[cfg(test)]
use std::time::Duration;
use std::{
    net::{Ipv4Addr, TcpListener as StdTcpListener},
    sync::Arc,
};
#[cfg(test)]
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
mod policy;
pub use policy::{MAX_REQUEST_BYTES, ResponsesScope};

pub struct UpstreamEndpoint(aq_responses_transport::UpstreamEndpoint);
impl UpstreamEndpoint {
    pub fn openai() -> Self {
        Self(aq_responses_transport::UpstreamEndpoint::openai())
    }
    #[cfg(any(test, feature = "test-support"))]
    pub fn development_loopback(origin: &str) -> Result<Self> {
        let origin = super::codex::development_origin(origin)?;
        Ok(Self(
            aq_responses_transport::UpstreamEndpoint::development_loopback(&origin)?,
        ))
    }
}

struct RetainedCredential {
    original: InstalledMaterial,
    manifest: aq_session_protocol::LaunchManifest,
    binding: LaunchBinding,
}
impl aq_responses_transport::RetainedUpstreamCredential for RetainedCredential {
    fn read_current(&self) -> Result<Zeroizing<Vec<u8>>> {
        let receipt = self.original.receipt()?;
        let handoff = super::material_handoff(&self.original, &receipt, &self.binding)?;
        super::codex::validated_api_key(&handoff, &self.manifest)
    }
}
/// Retains the real remote binding; the core's neutral digest does not replace
/// any worker admission/manifest/material authority join.
pub struct ScopedResponsesTransport {
    binding: LaunchBinding,
    core: aq_responses_transport::ScopedResponsesTransport,
}
impl ScopedResponsesTransport {
    pub(crate) async fn start_retained(
        scope: ResponsesScope,
        endpoint: UpstreamEndpoint,
        original: InstalledMaterial,
        manifest: &aq_session_protocol::LaunchManifest,
    ) -> Result<Self> {
        scope.validate_manifest(manifest)?;
        let (binding, policy) = scope.into_parts();
        let credential = Arc::new(RetainedCredential {
            original,
            manifest: manifest.clone(),
            binding: binding.clone(),
        });
        let listener = StdTcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let origin = format!("http://{}", listener.local_addr()?);
        let core = aq_responses_transport::ScopedResponsesTransport::start_on_listener(
            listener, &origin, policy, endpoint.0, credential,
        )
        .await?;
        Ok(Self { binding, core })
    }
    #[cfg(any(test, feature = "test-support"))]
    pub async fn start(
        scope: ResponsesScope,
        endpoint: UpstreamEndpoint,
        local_key: Zeroizing<Vec<u8>>,
        upstream_key: Zeroizing<Vec<u8>>,
    ) -> Result<Self> {
        let (binding, policy) = scope.into_parts();
        let listener = StdTcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let origin = format!("http://{}", listener.local_addr()?);
        let core = aq_responses_transport::ScopedResponsesTransport::start_on_listener_with_keys(
            listener,
            &origin,
            policy,
            endpoint.0,
            local_key,
            upstream_key,
        )
        .await?;
        Ok(Self { binding, core })
    }
    pub fn origin(&self) -> &str {
        self.core.origin()
    }
    pub fn binding(&self) -> &LaunchBinding {
        &self.binding
    }
    pub(crate) fn local_capability(&self) -> Zeroizing<Vec<u8>> {
        self.core.local_capability()
    }
    pub(crate) fn cancel(&self) {
        self.core.cancel();
    }
    pub async fn stop(self) -> Result<()> {
        self.core.stop().await
    }
}
#[cfg(test)]
mod tests;
