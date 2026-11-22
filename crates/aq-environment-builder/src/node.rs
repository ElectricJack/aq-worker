//! Spec 0017 §6.5: any node runs the server's admitted environments.
use crate::Server;
use crate::client::{Client, Refused};
use aq_session_protocol::ArtifactRef;
use serde::Deserialize;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct HarnessVersion {
    pub harness: String,
    pub version: String,
}

/// One entry of `GET /v1/worker/environments`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct AdmittedEnvironment {
    pub id: String,
    pub version: String,
    pub digest: String,
    pub image: String,
    pub harnesses: Vec<HarnessVersion>,
    pub warm: Vec<String>,
    pub definition: ArtifactRef,
}

/// The admitted environments this node has pulled, shared by the poll and every launch.
#[derive(Clone, Debug, Default)]
pub struct Held(Arc<Mutex<Vec<Entry>>>);
// Runtime state, not configuration: any two are equal, so a config compares as before.
impl PartialEq for Held {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}
impl Eq for Held {}

#[derive(Debug)]
struct Entry {
    environment: AdmittedEnvironment,
    admitted: bool,
    /// One clone per running attempt (its [`Lease`]).
    uses: Arc<()>,
}

/// Keeps an environment held while an attempt runs in it.
#[derive(Debug)]
pub struct Lease {
    _uses: Arc<()>,
}

impl Held {
    fn entries(&self) -> std::sync::MutexGuard<'_, Vec<Entry>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    /// The held environment whose definition an offer names.
    pub fn get(&self, definition: &ArtifactRef) -> Option<AdmittedEnvironment> {
        let entries = self.entries();
        let entry = entries
            .iter()
            .find(|e| e.environment.definition == *definition);
        entry.map(|e| e.environment.clone())
    }
    /// An offer runs in the node's boot environment (`None`) or in one it holds
    /// (its lease); any other is refused, never run in the boot template.
    pub fn select(
        &self,
        boot: &ArtifactRef,
        offered: &ArtifactRef,
    ) -> anyhow::Result<Option<Lease>> {
        if offered == boot {
            return Ok(None);
        }
        let entries = self.entries();
        let entry = entries
            .iter()
            .find(|e| e.environment.definition == *offered);
        let entry = entry.ok_or_else(|| {
            anyhow::anyhow!(
                "this node does not hold environment {}@{}",
                offered.id.as_str(),
                offered.version.as_str()
            )
        })?;
        Ok(Some(Lease {
            _uses: Arc::clone(&entry.uses),
        }))
    }
    /// The pulled admitted set replaces the last one. An environment that left
    /// it stays held while an attempt still runs in it.
    pub fn replace(&self, pulled: Vec<AdmittedEnvironment>) {
        let mut entries = self.entries();
        let mut old = std::mem::take(&mut *entries);
        for environment in pulled {
            let kept = old.iter().position(|e| e.environment == environment);
            let uses = kept.map_or_else(Arc::default, |i| old.swap_remove(i).uses);
            entries.push(Entry {
                environment,
                admitted: true,
                uses,
            });
        }
        for mut entry in old {
            if Arc::strong_count(&entry.uses) > 1 {
                entry.admitted = false;
                entries.push(entry);
            }
        }
    }
    fn holds(&self, digest: &str) -> bool {
        self.entries()
            .iter()
            .any(|e| e.environment.digest == digest)
    }
    fn ready(&self) -> Vec<String> {
        let entries = self.entries();
        entries
            .iter()
            .filter(|e| e.admitted)
            .map(|e| e.environment.digest.clone())
            .collect()
    }
}

/// Polls the admitted list every `interval` until shutdown: pulls each new
/// digest once with `pull`, then reports what the node holds ready.
pub async fn hold<F, R>(
    server: &Server,
    held: Held,
    interval: Duration,
    mut shutdown: watch::Receiver<bool>,
    mut pull: F,
) -> anyhow::Result<()>
where
    F: FnMut(AdmittedEnvironment) -> R,
    R: Future<Output = anyhow::Result<()>>,
{
    let client = Client::new(server)?;
    while !*shutdown.borrow() {
        if let Err(error) = refresh(&client, &held, &mut pull).await {
            tracing::warn!("admitted environments not refreshed: {error}");
        }
        tokio::select! {
            _ = shutdown.wait_for(|stop| *stop) => break,
            () = tokio::time::sleep(interval) => {}
        }
    }
    Ok(())
}

pub(crate) async fn refresh<F, R>(client: &Client, held: &Held, pull: &mut F) -> Result<(), Refused>
where
    F: FnMut(AdmittedEnvironment) -> R,
    R: Future<Output = anyhow::Result<()>>,
{
    let mut pulled = Vec::new();
    for environment in client.admitted().await? {
        if !held.holds(&environment.digest)
            && let Err(error) = pull(environment.clone()).await
        {
            let digest = &environment.digest;
            tracing::warn!(%digest, "admitted template not pulled: {error:#}");
            continue;
        }
        pulled.push(environment);
    }
    held.replace(pulled);
    client.ready(&held.ready()).await
}

#[cfg(test)]
#[path = "node_tests.rs"]
mod tests;
