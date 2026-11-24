use super::*;
use aq_admission::startup::NativeOffer;

pub(super) struct SmokeRun {
    pub config: Config,
    pub held: File,
    pub git: GitClient,
    pub client: NativeClient,
    pub journal: Journal,
    pub materials: MaterialStore,
    disk: u64,
}
pub(super) struct Reserved {
    pub run: SmokeRun,
    pub offer: NativeOffer,
    pub deadline: u64,
    pub id: Uuid,
}
impl SmokeRun {
    pub fn open(config: Config) -> Result<Self> {
        let held = root(&config.root)?;
        let git = GitClient::new(&config.git_socket, &config.git_root)?;
        let client = NativeClient::from_config(&config.worker)?
            .with_git_author_support(git.supports_checkpoint_author());
        ensure!(
            git.root().starts_with(&config.root) && git.root() != config.root,
            "Git scope must be a private child"
        );
        let disk = git.native_reservation_bytes()?;
        // Fixture-only logical envelope, not an assertion about real free capacity.
        let limits = Limits {
            slots: 2,
            memory_bytes: 64 * 1024 * 1024 * 1024,
            cpu_millis: 120000,
            disk_bytes: 64 * 1024 * 1024 * 1024,
        };
        ensure!(
            disk <= limits.disk_bytes,
            "Git profile exceeds smoke envelope"
        );
        let journal = Journal::create(&config.root.join("journal"), config.worker.node_id, limits)?;
        current_root(&config.root, &held)?;
        create_materials(&held)?;
        let identity = journal.state().identity.clone();
        let materials = MaterialStore::open(&config.root.join("materials"), identity, 32)?;
        current_root(&config.root, &held)?;
        Ok(Self {
            config,
            held,
            git,
            client,
            journal,
            materials,
            disk,
        })
    }
    pub async fn ready(&self, input: &mut impl BufRead, stage: &mut &'static str) -> Result<()> {
        let Self {
            config,
            held,
            client,
            journal,
            ..
        } = self;
        let identity = &journal.state().identity;
        // Qualified InventoryV2 must observe the actual child's admitted runtime.
        // Earlier probe modes keep their original ready/continue/register ordering.
        let registered_before_ready = config
            .phase_preparation
            .as_ref()
            .is_some_and(|p| p.prepare_execution)
            || config
                .refresh_preparation
                .as_ref()
                .is_some_and(|p| p.prepare_execution);
        if registered_before_ready {
            *stage = "register";
            register(client, config, identity).await?;
        }
        let mut ready = json!({"phase":"ready","identity":identity});
        if let Some(phase) = &config.refresh_preparation {
            ready["worker_version"] = json!(env!("CARGO_PKG_VERSION"));
            ready["preparation_contract"] = json!(phase.negotiation.contract);
            ready["journal_format"] = json!(journal.state().format);
        }
        emit(&ready)?;
        *stage = "continue";
        let next: Continue = frame(input, 1024)?;
        ensure!(next.r#continue, "explicit continue required");
        current_root(&config.root, held)?;
        if !registered_before_ready {
            *stage = "register";
            register(client, config, identity).await?;
        }
        Ok(())
    }
    pub async fn reserve(mut self, stage: &mut &'static str) -> Result<Reserved> {
        *stage = "poll";
        let available = self.journal.available()?;
        let mut page = self
            .client
            .poll(&Poll {
                schema_version: 1,
                operation_id: Uuid::new_v4(),
                identity: self.journal.state().identity.clone(),
                inventory_sequence: 1,
                available: Resources {
                    slots: available.slots,
                    memory_bytes: available.memory_bytes,
                    cpu_millis: available.cpu_millis,
                    disk_bytes: available.disk_bytes,
                },
                limit: 1,
                after_offer: None,
            })
            .await?;
        ensure!(
            page.page.offers.len() == 1 && page.offer_deadlines_ns.len() == 1,
            "exactly one native offer required"
        );
        let offer = page.page.offers.remove(0);
        let ttl = offer
            .offered
            .reservation_ttl_ms
            .checked_mul(1000000)
            .context("reservation duration overflow")?;
        let deadline = boot_ns()?
            .checked_add(ttl)
            .context("reservation deadline overflow")?
            .min(page.offer_deadlines_ns[0]);
        *stage = "reserve";
        let id = self.journal.reserve(
            aq_worker_protocol::git_author::AttemptOffer {
                offer: offer.clone(),
                git_author: page.git_authors.remove(0),
            },
            RuntimeIdentity {
                runtime: self.config.runtime.clone(),
                qualification: self.config.qualification.clone(),
            },
            deadline,
            self.disk,
        )?;
        Ok(Reserved {
            run: self,
            offer,
            deadline,
            id,
        })
    }
}
pub(super) fn spec(offer: &NativeOffer, missing: &'static str) -> Result<ArtifactRef> {
    offer
        .offered
        .artifacts
        .iter()
        .find(|d| {
            d.kind == ArtifactKind::Composition
                && d.artifact.id.as_str() == "aq.profiled-execution-spec"
                && d.artifact.digest == offer.offered.offer.binding.execution_spec_digest
        })
        .map(|d| d.artifact.clone())
        .context(missing)
}
