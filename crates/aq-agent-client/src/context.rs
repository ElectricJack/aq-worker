//! Immutable labelled context, separate from both live authority and graph state.
use anyhow::{Result, ensure};
use aq_agent_api::{Binding, Context};
use aq_session_protocol::{ContextDelivery, Digest, ProfiledOffer, SessionIdentity, Validate};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub const MAX_BUNDLE_BYTES: usize = 8 * 1024 * 1024;
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextContent {
    pub digest: Digest,
    pub text: String,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextBundle {
    pub format_version: u32,
    pub binding: Binding,
    pub profiled_offer: ProfiledOffer,
    pub graph_context: Value,
    pub contents: Vec<ContextContent>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartupContext {
    pub binding: Binding,
    pub profiled_offer: ProfiledOffer,
    pub contents: Vec<ContextContent>,
}
impl StartupContext {
    pub fn validate(&self) -> Result<()> {
        validate_parts(&self.binding, &self.profiled_offer, &self.contents)?;
        ensure!(
            serde_json::to_vec(self)?.len() <= MAX_BUNDLE_BYTES - aq_agent_api::MAX_DOCUMENT_BYTES,
            "startup context reservation bound"
        );
        Ok(())
    }
    pub fn with_graph_context(self, graph_context: Value) -> Result<ContextBundle> {
        let bundle = ContextBundle {
            format_version: 1,
            binding: self.binding,
            profiled_offer: self.profiled_offer,
            contents: self.contents,
            graph_context,
        };
        bundle.validate()?;
        Ok(bundle)
    }
}
impl ContextBundle {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.format_version == 1 && self.contents.len() <= 128,
            "context bundle bounds"
        );
        validate_parts(&self.binding, &self.profiled_offer, &self.contents)?;
        Context::new(self.graph_context.clone(), &self.binding)?;
        ensure!(
            serde_json::to_vec(self)?.len() <= MAX_BUNDLE_BYTES,
            "context cache byte bound"
        );
        Ok(())
    }
    /// Return exact startup bytes in manifest order. The adapter must preserve
    /// these segment boundaries/order; it must not silently shorten required text.
    pub fn startup(&self) -> Result<Vec<&str>> {
        self.validate()?;
        Ok(self
            .profiled_offer
            .manifest
            .context
            .references
            .iter()
            .filter(|r| r.delivery == ContextDelivery::Startup)
            .filter_map(|r| {
                self.contents
                    .iter()
                    .find(|c| c.digest == r.artifact.digest)
                    .map(|c| c.text.as_str())
            })
            .collect())
    }
    pub fn cached_view(&self) -> Result<Value> {
        self.validate()?;
        Ok(serde_json::json!({"cached":true,"live_authority":false,
            "binding":self.binding,"graph_context":self.graph_context,
            "launch_manifest_digest":self.profiled_offer.binding.launch_manifest_digest,
            "missing_optional_context":self.profiled_offer.manifest.context.references.iter()
                .filter(|r|!self.contents.iter().any(|c|c.digest==r.artifact.digest))
                .map(|r|&r.artifact).collect::<Vec<_>>() }))
    }
}

fn validate_parts(
    binding: &Binding,
    profiled_offer: &ProfiledOffer,
    contents: &[ContextContent],
) -> Result<()> {
    binding.validate()?;
    profiled_offer.validate()?;
    let b = binding;
    let offer = profiled_offer;
    let launch = &offer.binding;
    let m = &offer.manifest;
    ensure!(
        launch.session
            == SessionIdentity::Remote {
                node_id: b.node_id,
                boot_id: b.boot_id,
                attempt_id: b.attempt_id,
                assignment_generation: b.generation,
                session_id: b.session_id,
            }
            && launch.execution_spec_digest.as_str() == b.resolved_spec_digest
            && launch.graph_access_profile_digest.as_str() == b.profile_digest
            && m.bindings.graph_id == b.workspace_id
            && m.bindings.task_id == b.task_id
            && m.bindings.project_id == b.project_id,
        "context offer/access binding mismatch"
    );
    let mut by_digest = BTreeMap::new();
    for content in contents {
        ensure!(
            content.text.len() <= 1_048_576
                && Digest::of(content.text.as_bytes()) == content.digest
                && by_digest.insert(content.digest.as_str(), content).is_none(),
            "context bytes/digest conflict"
        );
    }
    let mut referenced = std::collections::BTreeSet::new();
    for reference in &m.context.references {
        referenced.insert(reference.artifact.digest.as_str());
        if let Some(content) = by_digest.get(reference.artifact.digest.as_str()) {
            ensure!(
                content.text.len() as u64 == reference.bytes,
                "context length mismatch"
            );
        } else {
            ensure!(
                !reference.required,
                "required context missing; no truncation allowed"
            );
        }
    }
    ensure!(
        by_digest.keys().all(|k| referenced.contains(k)),
        "unbound context content"
    );

    Ok(())
}
