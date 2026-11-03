//! C4 contract: sandbox registration advertises templates resolved from the
//! C3b template allowlist, so each `TemplateAdvertisement.digest` is the exact
//! `InstalledHarness.environment.digest` (64 lowercase hex, no `sha256:`
//! prefix) and each harness version is the exact CLI version in the image.
//!
//! The allowlist is read like aq-server reads it: `{schema_version: 1,
//! entries: [{name, version, image_digest: "sha256:<hex>", harness_versions,
//! ...}]}`; extra provenance fields are ignored. Environment refs resolve as
//! `name@version` exactly, or as a bare `name` that must be unique.
use anyhow::{Context, Result, bail, ensure};
use aq_admission_sandbox::sandbox::{HarnessVersion, Money, TemplateAdvertisement};
use aq_session_protocol::Digest;
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

const MAX_ALLOWLIST_BYTES: u64 = 16 * 1024 * 1024;

/// Operator selection of allowlisted templates to advertise on this node.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TemplateAllowlistConfig {
    /// C3b allowlist file; relative paths resolve from the config file's directory.
    pub path: PathBuf,
    pub advertise: Vec<AdvertisedEnvironment>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdvertisedEnvironment {
    /// `name@version`, or a bare `name` that is unique in the allowlist.
    pub environment: String,
    pub estimated_cold_start_ms: u64,
    pub estimated_cold_start_cost: Money,
}

#[derive(Deserialize)]
struct AllowlistFile {
    schema_version: u32,
    entries: Vec<AllowlistEntry>,
}

/// Unknown provenance/materialization fields are intentionally ignored.
#[derive(Deserialize)]
struct AllowlistEntry {
    name: String,
    version: String,
    image_digest: String,
    harness_versions: BTreeMap<String, String>,
}

/// Strict `sha256:<64 lowercase hex>` to the bare protocol digest.
fn environment_digest(image_digest: &str) -> Result<Digest> {
    let hex = image_digest
        .strip_prefix("sha256:")
        .context("allowlist image_digest must be sha256:<hex>")?;
    serde_json::from_value(serde_json::Value::String(hex.to_owned()))
        .context("allowlist image_digest must be 64 lowercase hex")
}

fn resolve<'a>(entries: &'a [AllowlistEntry], reference: &str) -> Result<&'a AllowlistEntry> {
    let matches: Vec<_> = match reference.split_once('@') {
        Some((name, version)) => entries
            .iter()
            .filter(|e| e.name == name && e.version == version)
            .collect(),
        None => entries.iter().filter(|e| e.name == reference).collect(),
    };
    match matches.as_slice() {
        [entry] => Ok(entry),
        [] => bail!("environment {reference:?} is not in the template allowlist"),
        _ => bail!("environment {reference:?} is ambiguous; use name@version"),
    }
}

/// Resolve advertisements from allowlist bytes. Pure, for tests and loading.
pub fn advertisements(
    allowlist: &[u8],
    advertise: &[AdvertisedEnvironment],
) -> Result<Vec<TemplateAdvertisement>> {
    let file: AllowlistFile =
        serde_json::from_slice(allowlist).context("invalid template allowlist")?;
    ensure!(
        file.schema_version == 1,
        "unsupported template allowlist schema"
    );
    advertise
        .iter()
        .map(|selection| {
            let entry = resolve(&file.entries, &selection.environment)?;
            ensure!(
                !entry.harness_versions.is_empty(),
                "allowlisted environment {:?} lists no harness versions",
                selection.environment
            );
            for version in entry.harness_versions.values() {
                ensure!(
                    !version.is_empty()
                        && version != "latest"
                        && !version.contains(['*', '^', '~', '>', '<', '='])
                        && !version.chars().any(char::is_whitespace),
                    "allowlisted harness version {version:?} is not exact"
                );
            }
            Ok(TemplateAdvertisement {
                digest: environment_digest(&entry.image_digest)?,
                harnesses: entry
                    .harness_versions
                    .iter()
                    .map(|(harness, version)| HarnessVersion {
                        harness: harness.clone(),
                        version: version.clone(),
                    })
                    .collect(),
                estimated_cold_start_ms: selection.estimated_cold_start_ms,
                estimated_cold_start_cost: selection.estimated_cold_start_cost.clone(),
            })
        })
        .collect()
}

pub(super) fn load(
    config: &TemplateAllowlistConfig,
    config_path: &Path,
) -> Result<Vec<TemplateAdvertisement>> {
    use std::io::Read;
    let path = if config.path.is_absolute() {
        config.path.clone()
    } else {
        config_path
            .parent()
            .unwrap_or(Path::new("."))
            .join(&config.path)
    };
    let mut bytes = Vec::new();
    std::fs::File::open(&path)
        .with_context(|| format!("open template allowlist {}", path.display()))?
        .take(MAX_ALLOWLIST_BYTES + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_ALLOWLIST_BYTES,
        "template allowlist too large"
    );
    advertisements(&bytes, &config.advertise)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CODEX_DIGEST: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    const OTHER_DIGEST: &str = "2222222222222222222222222222222222222222222222222222222222222222";

    fn allowlist() -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "schema_version": 1,
            "entries": [
                {"name": "codex-tools", "version": "1",
                 "image_digest": format!("sha256:{CODEX_DIGEST}"),
                 "image_reference": format!("registry.example/aq/template@sha256:{CODEX_DIGEST}"),
                 "provenance": {"image_digest": format!("sha256:{CODEX_DIGEST}"),
                                "harness_versions": {"codex": "0.166.0"},
                                "builder_identity": "desktop-1"},
                 "harness_versions": {"codex": "0.166.0"},
                 "materializations": {}},
                {"name": "multi", "version": "1",
                 "image_digest": format!("sha256:{OTHER_DIGEST}"),
                 "harness_versions": {"codex": "0.166.0", "claude": "2.1.3"}},
                {"name": "multi", "version": "2",
                 "image_digest": format!("sha256:{CODEX_DIGEST}"),
                 "harness_versions": {"codex": "0.167.0"}}
            ]
        }))
        .unwrap()
    }
    fn select(environment: &str) -> AdvertisedEnvironment {
        AdvertisedEnvironment {
            environment: environment.into(),
            estimated_cold_start_ms: 1500,
            estimated_cold_start_cost: Money {
                currency: "USD".into(),
                micros: 20,
            },
        }
    }

    #[test]
    fn advertises_bare_hex_digest_and_exact_harness_versions() {
        let ads =
            advertisements(&allowlist(), &[select("codex-tools"), select("multi@1")]).unwrap();
        assert_eq!(ads[0].digest.as_str(), CODEX_DIGEST);
        assert!(!ads[0].digest.as_str().starts_with("sha256:"));
        let versions = |a: &TemplateAdvertisement| {
            a.harnesses
                .iter()
                .map(|h| (h.harness.clone(), h.version.clone()))
                .collect::<Vec<_>>()
        };
        assert_eq!(versions(&ads[0]), [("codex".into(), "0.166.0".into())]);
        assert_eq!(ads[1].digest.as_str(), OTHER_DIGEST);
        assert_eq!(
            versions(&ads[1]),
            [
                ("claude".into(), "2.1.3".into()),
                ("codex".into(), "0.166.0".into())
            ]
        );
        assert_eq!(ads[1].estimated_cold_start_ms, 1500);
    }

    #[test]
    fn exact_name_at_version_and_unique_bare_name_only() {
        let ads = advertisements(&allowlist(), &[select("multi@2")]).unwrap();
        assert_eq!(ads[0].harnesses[0].version, "0.167.0");
        for bad in ["multi", "missing", "codex-tools@2", "multi@"] {
            assert!(
                advertisements(&allowlist(), &[select(bad)]).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn refuses_malformed_digests_and_schema() {
        for digest in [
            CODEX_DIGEST.to_owned(),
            format!("sha256:{}", "A".repeat(64)),
            format!("sha512:{CODEX_DIGEST}"),
            "sha256:abc".to_owned(),
        ] {
            let doc = serde_json::json!({"schema_version": 1, "entries": [
                {"name": "t", "version": "1", "image_digest": digest,
                 "harness_versions": {"codex": "0.166.0"}}]});
            assert!(
                advertisements(&serde_json::to_vec(&doc).unwrap(), &[select("t")]).is_err(),
                "{digest}"
            );
        }
        let doc = serde_json::json!({"schema_version": 2, "entries": []});
        assert!(advertisements(&serde_json::to_vec(&doc).unwrap(), &[]).is_err());
        let doc = serde_json::json!({"schema_version": 1, "entries": [
            {"name": "t", "version": "1", "image_digest": format!("sha256:{CODEX_DIGEST}"),
             "harness_versions": {}}]});
        assert!(advertisements(&serde_json::to_vec(&doc).unwrap(), &[select("t")]).is_err());
        for version in ["latest", "^0.166", ">=1", "0.166 .0", ""] {
            let doc = serde_json::json!({"schema_version": 1, "entries": [
                {"name": "t", "version": "1", "image_digest": format!("sha256:{CODEX_DIGEST}"),
                 "harness_versions": {"codex": version}}]});
            assert!(
                advertisements(&serde_json::to_vec(&doc).unwrap(), &[select("t")]).is_err(),
                "{version}"
            );
        }
    }

    #[test]
    fn run_config_load_merges_allowlist_advertisement_into_registration() {
        use aq_admission_sandbox::sandbox::{BackendAdvertisement, BackendKind};
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("templates")).unwrap();
        std::fs::write(dir.path().join("templates/allowlist.json"), allowlist()).unwrap();
        let mut node = serde_json::to_value(super::super::tests::config()).unwrap();
        node["templates"] = serde_json::json!([]);
        let write = |backend_digest: &str| {
            let doc = serde_json::json!({
                "state_dir": "/var/lib/aq-controller",
                "node": node,
                "docker_local": {"socket": "/run/aq-sandbox/docker.sock", "templates": [
                    {"image": format!("registry.example/aq/template@sha256:{backend_digest}"),
                     "digest": format!("sha256:{backend_digest}")}]},
                "template_allowlist": {"path": "templates/allowlist.json",
                    "advertise": [{"environment": "codex-tools@1",
                                   "estimated_cold_start_ms": 1500,
                                   "estimated_cold_start_cost": {"currency": "USD", "micros": 20}}]}
            });
            let path = dir.path().join("controller.json");
            std::fs::write(&path, serde_json::to_vec(&doc).unwrap()).unwrap();
            super::super::RunConfig::load(&path)
        };
        let config = write(CODEX_DIGEST).unwrap();
        assert_eq!(config.node.templates.len(), 1);
        let template = &config.node.templates[0];
        assert_eq!(template.digest.as_str(), CODEX_DIGEST);
        assert_eq!(template.harnesses[0].harness, "codex");
        assert_eq!(template.harnesses[0].version, "0.166.0");
        BackendAdvertisement {
            kind: BackendKind::DockerLocal,
            capabilities: vec![],
            max_concurrent_sandboxes: 1,
            spend_ceiling: None,
            templates: config.node.templates.clone(),
        }
        .validate()
        .unwrap();
        // An allowlisted template the backend cannot launch is never advertised.
        assert!(write(OTHER_DIGEST).is_err());
    }
}
