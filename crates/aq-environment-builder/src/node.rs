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
    /// it stays held while an attempt still runs in it; the others are returned.
    pub fn replace(&self, pulled: Vec<AdmittedEnvironment>) -> Vec<AdmittedEnvironment> {
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
        let mut dropped = Vec::new();
        for mut entry in old {
            if Arc::strong_count(&entry.uses) > 1 {
                entry.admitted = false;
                entries.push(entry);
            } else {
                dropped.push(entry.environment);
            }
        }
        dropped
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

/// What [`Templates::remove`] did with a template image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Removal {
    /// Deleted from the daemon, or already absent.
    Removed,
    /// A container (running, stopped or preserved) still uses it: tried again next poll.
    InUse,
    /// One of the node's own templates: never removed.
    Kept,
}

/// The node's sandbox daemon, as the poll uses it.
pub trait Templates {
    /// Pulls an admitted template by digest and proves it is present.
    fn pull(
        &mut self,
        environment: &AdmittedEnvironment,
    ) -> impl Future<Output = anyhow::Result<()>> + Send;
    /// Removes a template no attempt holds, unless a container still uses it.
    fn remove(
        &mut self,
        environment: &AdmittedEnvironment,
    ) -> impl Future<Output = anyhow::Result<Removal>> + Send;
}

/// The poll's operator settings (`service.toml` `[environments]`).
#[derive(Clone, Copy, Debug)]
pub struct Poll {
    pub interval: Duration,
    /// The largest admitted list the node reads.
    pub max_list_bytes: u64,
}

/// Polls the admitted list every `poll.interval` until shutdown: pulls each
/// new digest once, reports what the node holds ready, and removes the images
/// of environments that left the list once nothing uses them.
pub async fn hold(
    server: &Server,
    held: Held,
    poll: Poll,
    mut shutdown: watch::Receiver<bool>,
    mut templates: impl Templates,
) -> anyhow::Result<()> {
    let client = Client::new(server)?;
    let mut retired = Vec::new();
    while !*shutdown.borrow() {
        match refresh(&client, &held, poll, &mut templates, &mut retired).await {
            Err(error @ Refused::Oversized { .. }) => tracing::error!(
                "admitted environments not refreshed: {error}; raise [environments] \
                 max_list_bytes (the list) or max_reply_bytes (the ready reply)"
            ),
            Err(error) => tracing::warn!("admitted environments not refreshed: {error}"),
            Ok(()) => {}
        }
        tokio::select! {
            _ = shutdown.wait_for(|stop| *stop) => break,
            () = tokio::time::sleep(poll.interval) => {}
        }
    }
    Ok(())
}

/// One poll. `retired` carries images that left the list but were still in
/// use, so their removal is tried again.
pub(crate) async fn refresh(
    client: &Client,
    held: &Held,
    poll: Poll,
    templates: &mut impl Templates,
    retired: &mut Vec<AdmittedEnvironment>,
) -> Result<(), Refused> {
    let mut pulled = Vec::new();
    for environment in client.admitted(poll.max_list_bytes).await? {
        if !held.holds(&environment.digest)
            && let Err(error) = templates.pull(&environment).await
        {
            let digest = &environment.digest;
            tracing::warn!(%digest, "admitted template not pulled: {error:#}");
            continue;
        }
        pulled.push(environment);
    }
    retired.extend(held.replace(pulled));
    let ready = client.ready(&held.ready()).await;
    let mut kept = Vec::new();
    for environment in std::mem::take(retired) {
        let (digest, image) = (&environment.digest, &environment.image);
        // Another held entry, or an earlier retired one, may share the image.
        if held.holds(digest)
            || kept
                .iter()
                .any(|e: &AdmittedEnvironment| e.digest == *digest)
        {
            continue;
        }
        match templates.remove(&environment).await {
            Ok(Removal::Removed) => tracing::info!(%digest, %image, "removed template image"),
            Ok(Removal::Kept) => {}
            Ok(Removal::InUse) => kept.push(environment),
            Err(error) => {
                tracing::warn!(%digest, "template image not removed: {error:#}");
                kept.push(environment);
            }
        }
    }
    *retired = kept;
    ready
}

#[cfg(test)]
#[path = "node_tests.rs"]
mod tests;
