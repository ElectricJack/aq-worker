macro_rules! prepared_inputs {
    ($wire:path,$name:ident,$prepared:ident) => {
        use $wire as phase;
        pub struct $name {
            pub(super) retained_inline_bytes: u64,
            pub(super) startup_bytes: u64,
            pub(super) request: phase::PrepareRequest,
            pub(super) startup: RenderedStartup,
            pub(super) workspace: HydratedWorkspace,
            pub(super) artifacts: Vec<VerifiedArtifact>,
            pub(super) materials: Vec<InstalledMaterial>,
        }
        impl $name {
            pub(in crate::native::preparation) fn retained_inline_bytes(&self) -> u64 {
                self.retained_inline_bytes
            }
            pub(in crate::native::preparation) fn startup_bytes(&self) -> u64 {
                self.startup_bytes
            }
            pub(in crate::native::preparation) fn verify_execution_inputs(
                &self,
                journal: &Journal,
                store: &MaterialStore,
                deadline: u64,
            ) -> Result<()> {
                let a = journal
                    .state()
                    .attempts
                    .get(&self.request.original.binding.attempt_id)
                    .context("phase retained attempt absent")?;
                let p = a
                    .$prepared
                    .as_ref()
                    .context("phase retained Prepare absent")?;
                ensure!(
                    serde_json::to_value(&self.request)? == serde_json::to_value(&p.request)?,
                    "phase retained Prepare changed"
                );
                ensure!(
                    self.materials.len() == self.request.original.materials.len(),
                    "phase retained material count"
                );
                for material in &self.materials {
                    let r = material.receipt()?;
                    ensure!(
                        self.request
                            .original
                            .materials
                            .iter()
                            .any(|v| serde_json::to_value(v).ok() == serde_json::to_value(&r).ok()),
                        "phase retained material pin"
                    );
                    drop(material.read_private()?);
                    // Verify the current private installation namespace and actual raw
                    // record against the retained immutable receipt, without replacing
                    // the first owner bundle or extending retrieval/preparation clocks.
                    drop(store.reopen_pinned(&r)?.read_private()?);
                }
                self.workspace.revalidate_execution(journal, deadline)
            }
            pub fn request(&self) -> &phase::PrepareRequest {
                &self.request
            }
            pub fn startup(&self) -> &RenderedStartup {
                &self.startup
            }
            pub fn workspace(&self) -> &HydratedWorkspace {
                &self.workspace
            }
            pub fn artifacts(&self) -> &[VerifiedArtifact] {
                &self.artifacts
            }
            pub fn materials(&self) -> &[InstalledMaterial] {
                &self.materials
            }
        }
    };
}
pub(crate) use prepared_inputs;
