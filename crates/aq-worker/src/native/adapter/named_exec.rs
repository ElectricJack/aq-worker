//! Separately tagged named-readonly installation. No legacy mutable fallback.
use super::*;
use aq_session_host::observed::{
    MAX_NAMED_EXEC_BYTES, NamedReadonlyExecImage, NamedReadonlyExecPool, NamedReadonlyFile,
};
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum NamedExecKind {
    #[serde(rename = "codex-0.166-named-readonly-exec-stdin-v1")]
    Codex0166NamedReadonlyExecStdinV1,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NamedExecManifest {
    pub schema_version: u32,
    pub profile: NamedExecKind,
    pub executable_bytes: u64,
    pub environment: ArtifactRef,
    pub installation: AdapterManifest,
}
impl NamedExecManifest {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        ensure!(
            !bytes.is_empty() && bytes.len() <= MAX_MANIFEST,
            "named installation manifest bound"
        );
        Ok(serde_json::from_slice(bytes)?)
    }
    pub fn digest(&self) -> Result<Digest> {
        super::super::journal::canonical_digest(self)
    }
}
/// Opaque retained preflight installation; environment reference is independently
/// selected, and the pool has inspected actual kernel premises. Neither this
/// owner nor its artifact references grant launch/model/secret-isolation authority.
pub struct NamedExecInstallation {
    manifest: NamedExecManifest,
    digest: Digest,
    pool: NamedReadonlyExecPool,
    program: NamedReadonlyExecImage,
    local: [NamedReadonlyFile; 2],
}
impl NamedExecInstallation {
    pub fn open(
        manifest: NamedExecManifest,
        expected: ExpectedAdapter<'_>,
        expected_environment: &ArtifactRef,
        pool: &NamedReadonlyExecPool,
    ) -> Result<Self> {
        ensure!(
            manifest.schema_version == 1
                && manifest.installation.schema_version == 1
                && manifest.digest()? == *expected.manifest_digest
                && serde_json::to_vec(&manifest)?.len() <= MAX_MANIFEST
                && (64..=MAX_NAMED_EXEC_BYTES).contains(&manifest.executable_bytes)
                && manifest.environment == *expected_environment
                && manifest.environment == *pool.qualification(),
            "named installation profile/size/independent environment pin"
        );
        let m = &manifest.installation;
        ensure!(
            m.adapter == *expected.adapter
                && m.runtime == *expected.runtime
                && m.qualification == *expected.qualification
                && m.renderer == *expected.renderer,
            "named installation independent references"
        );
        for reference in [&m.adapter, &m.runtime, &m.qualification, &m.renderer] {
            reference.id.validate()?;
            reference.version.validate()?;
        }
        let program = pool.load_verified(&m.executable.path, &m.executable.artifact)?;
        ensure!(
            program.bytes() == manifest.executable_bytes,
            "named actual executable byte pin"
        );
        let local = [
            pool.load_auxiliary_verified(&m.helper.path, &m.helper.artifact)?,
            pool.load_auxiliary_verified(&m.tmux.path, &m.tmux.artifact)?,
        ];
        Ok(Self {
            manifest,
            digest: expected.manifest_digest.clone(),
            pool: pool.clone(),
            program,
            local,
        })
    }
    pub fn digest(&self) -> &Digest {
        &self.digest
    }
    pub fn environment(&self) -> &ArtifactRef {
        &self.manifest.environment
    }
    pub fn executable_bytes(&self) -> u64 {
        self.manifest.executable_bytes
    }
    pub fn image(&self) -> Result<NamedReadonlyExecImage> {
        self.program.revalidate()?;
        self.pool.load_verified(
            &self.manifest.installation.executable.path,
            &self.manifest.installation.executable.artifact,
        )
    }
    pub fn revalidate(&mut self) -> Result<()> {
        self.program.revalidate()?;
        for file in &self.local {
            file.revalidate()?;
        }
        Ok(())
    }
    pub fn material<'a>(
        &self,
        installed: &'a InstalledMaterial,
        expected: &MaterialReceipt,
        binding: &LaunchBinding,
    ) -> Result<MaterialHandoff<'a>> {
        super::material_handoff(installed, expected, binding)
    }
    pub fn installation(&self) -> &AdapterManifest {
        &self.manifest.installation
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn reference() -> ArtifactRef {
        ArtifactRef {
            id: aq_session_protocol::Text::new("named-fixture").unwrap(),
            version: aq_session_protocol::Text::new("1").unwrap(),
            digest: Digest::of(b"fixture"),
        }
    }
    fn document() -> serde_json::Value {
        let r = reference();
        let pin = serde_json::json!({"path":"/opt/aq-qualification/elf","artifact":r});
        serde_json::json!({"schema_version":1,"profile":"codex-0.166-named-readonly-exec-stdin-v1","executable_bytes":128,"environment":r,
            "installation":{"schema_version":1,"adapter":r,"runtime":r,"qualification":r,"renderer":r,"handoff":"opaque_private_file_v1","executable":pin,"helper":pin,"tmux":pin}})
    }
    #[test]
    fn named_manifest_is_distinct_and_cannot_accept_a_qualification_boolean() {
        let mut doc = document();
        NamedExecManifest::decode(&serde_json::to_vec(&doc).unwrap()).unwrap();
        assert!(
            super::super::native_exec::NativeExecManifest::decode(
                &serde_json::to_vec(&doc).unwrap()
            )
            .is_err()
        );
        doc["profile"] = serde_json::json!("codex-0.166-exec-stdin-v1");
        assert!(NamedExecManifest::decode(&serde_json::to_vec(&doc).unwrap()).is_err());
        let mut doc = document();
        doc.as_object_mut().unwrap().remove("environment");
        doc["environment_qualified"] = serde_json::json!(true);
        assert!(NamedExecManifest::decode(&serde_json::to_vec(&doc).unwrap()).is_err());
    }
    #[test]
    #[ignore = "requires explicit readonly namespace fixture; reads pinned ELF files only, never launches Codex"]
    fn named_installation_pins_environment_and_reuses_all_readonly_owners() {
        use aq_session_host::observed::NamedReadonlyEnvironment;
        assert_eq!(
            std::env::var("AQ_NATIVE_FIXTURE_CONFINED").as_deref(),
            Ok("v1")
        );
        let environment = reference();
        let pool = NamedReadonlyExecPool::new(
            64 * 1024 * 1024,
            1,
            NamedReadonlyEnvironment::inspect(environment.clone()).unwrap(),
        )
        .unwrap();
        let host = FilePin {
            path: "/opt/aq-qualification/aq-session-host".into(),
            artifact: ArtifactRef {
                digest: serde_json::from_value(serde_json::json!(
                    std::env::var("AQ_NATIVE_HOST_SHA256").unwrap()
                ))
                .unwrap(),
                ..reference()
            },
        };
        let tmux = FilePin {
            path: "/opt/aq-qualification/tmux".into(),
            artifact: ArtifactRef {
                id: aq_session_protocol::Text::new("tmux-fixture").unwrap(),
                digest: serde_json::from_value(serde_json::json!(
                    std::env::var("AQ_NATIVE_TMUX_SHA256").unwrap()
                ))
                .unwrap(),
                ..reference()
            },
        };
        let manifest = NamedExecManifest {
            schema_version: 1,
            profile: NamedExecKind::Codex0166NamedReadonlyExecStdinV1,
            executable_bytes: std::fs::metadata(&host.path).unwrap().len(),
            environment: environment.clone(),
            installation: AdapterManifest {
                schema_version: 1,
                adapter: reference(),
                runtime: reference(),
                qualification: reference(),
                renderer: reference(),
                handoff: HandoffKind::OpaquePrivateFileV1,
                executable: host.clone(),
                helper: host,
                tmux,
            },
        };
        let digest = manifest.digest().unwrap();
        let r = reference();
        let wrong = ArtifactRef {
            digest: Digest::of(b"wrong env"),
            ..reference()
        };
        let expected = || ExpectedAdapter {
            manifest_digest: &digest,
            adapter: &r,
            runtime: &r,
            qualification: &r,
            renderer: &r,
        };
        assert!(NamedExecInstallation::open(manifest.clone(), expected(), &wrong, &pool).is_err());
        assert_eq!(pool.charged_bytes().unwrap(), 0);
        let mut installed =
            NamedExecInstallation::open(manifest.clone(), expected(), &environment, &pool).unwrap();
        let charged = pool.charged_bytes().unwrap();
        let expected_bytes = manifest.executable_bytes * 2
            + std::fs::metadata(&manifest.installation.tmux.path)
                .unwrap()
                .len();
        assert_eq!(charged, expected_bytes);
        for _ in 0..16 {
            installed.revalidate().unwrap();
            assert_eq!(
                installed.image().unwrap().bytes(),
                manifest.executable_bytes
            );
        }
        assert_eq!(pool.charged_bytes().unwrap(), charged);
    }
}
