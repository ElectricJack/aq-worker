//! Shared manifest settings and lexical checks for harness launch recipes.
use aq_session_protocol::{HarnessSettings, ModelSettings};
use std::path::{Component, Path};

#[derive(Clone, Copy)]
pub(crate) struct RecipeSettings<'a> {
    pub harness: &'a HarnessSettings,
    pub model: &'a ModelSettings,
}
impl<'a> RecipeSettings<'a> {
    pub(crate) fn new(harness: &'a HarnessSettings, model: &'a ModelSettings) -> Self {
        Self { harness, model }
    }
}

pub(super) fn declares(harness: &HarnessSettings, id: &str) -> bool {
    harness
        .required_capabilities
        .iter()
        .any(|cap| cap.id.as_str() == id && cap.version == 1)
}
pub(super) fn normalized(path: &Path) -> bool {
    path.is_absolute()
        && path
            .components()
            .all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
        && !path.as_os_str().as_encoded_bytes().contains(&0)
}
pub(super) fn disjoint(first: &Path, second: &Path) -> bool {
    !first.starts_with(second) && !second.starts_with(first)
}

#[cfg(test)]
pub(super) mod fixture {
    use super::*;
    use aq_session_protocol::{ArtifactRef, Capability, Digest, Text};

    pub(crate) fn harness(name: &str, version: &str, confinement: &str) -> HarnessSettings {
        let artifact = ArtifactRef {
            id: Text::new("fixture").unwrap(),
            version: Text::new("1").unwrap(),
            digest: Digest::of(b"fixture"),
        };
        let capabilities =
            ["aq.harness.startup-context", confinement, "aq.harness.stop"].map(|id| Capability {
                id: Text::new(id).unwrap(),
                version: 1,
            });
        HarnessSettings {
            harness: Text::new(name).unwrap(),
            cli_version: Text::new(version).unwrap(),
            adapter: artifact.clone(),
            environment: artifact,
            required_capabilities: capabilities.to_vec(),
        }
    }
    pub(crate) fn model(provider: &str, name: &str, reasoning: &str) -> ModelSettings {
        ModelSettings {
            provider: Text::new(provider).unwrap(),
            model: Text::new(name).unwrap(),
            reasoning: Text::new(reasoning).unwrap(),
            max_output_tokens: 4096,
            spending_budget: None,
        }
    }
}
