use super::*;

use crate::native::preparation::InputContext;
enum PreparationSource<'a> {
    Legacy(&'a Preparation),
    Context(&'a InputContext),
}
struct Installation<'a> {
    material: PrivateMaterial,
    binding: &'a aq_session_protocol::LaunchBinding,
    leases: &'a [aq_admission::MaterialLease],
    origin: Digest,
    deadline_ns: u64,
}
impl Installation<'_> {
    fn deadline(&self) -> u64 {
        self.deadline_ns.min(self.material.retrieval_deadline_ns)
    }
    fn validate(&self, identity: &WorkerIdentity) -> Result<()> {
        let material = &self.material;
        let lease = &material.delivery.lease;
        ensure!(
            matches!(lease.binding.session, aq_session_protocol::SessionIdentity::Remote {node_id, boot_id, ..} if node_id == identity.node_id && boot_id == identity.boot_id)
                && lease.binding == *self.binding
                && self.leases.iter().any(|v| same(v, lease).unwrap_or(false)),
            "material preparation scope"
        );
        let computed = material.delivery.verify_received(
            &lease.binding,
            &material.bytes,
            material.sent_ns,
            material.received_ns,
        )?;
        ensure!(
            computed == material.retrieval_deadline_ns
                && boot_ns()? < computed.min(self.deadline_ns),
            "material installation expired"
        );
        Ok(())
    }
}
impl MaterialStore {
    pub fn install(
        &mut self,
        material: PrivateMaterial,
        preparation: &Preparation,
    ) -> Result<InstalledMaterial> {
        self.install_material(material, PreparationSource::Legacy(preparation))
    }
    pub(crate) fn install_context(
        &mut self,
        material: PrivateMaterial,
        preparation: &InputContext,
    ) -> Result<InstalledMaterial> {
        self.install_material(material, PreparationSource::Context(preparation))
    }
    fn install_material(
        &mut self,
        material: PrivateMaterial,
        source: PreparationSource<'_>,
    ) -> Result<InstalledMaterial> {
        let (binding, leases, origin, deadline_ns) = match source {
            PreparationSource::Legacy(p) => (
                &p.plan().binding,
                p.plan().materials.as_slice(),
                canonical_digest(p.plan())?,
                p.deadline_ns(),
            ),
            PreparationSource::Context(p) => {
                (p.binding(), p.materials(), p.digest()?, p.deadline_ns())
            }
        };
        let install = Installation {
            material,
            binding,
            leases,
            origin,
            deadline_ns,
        };
        ensure!(
            self.owner_pid == std::process::id(),
            "forked material owner"
        );
        private_dir(&self.root)?;
        install.validate(&self.identity)?;
        let name = CString::new(install.material.delivery.lease.lease_id.to_string())?;
        match directory(&self.root, &name) {
            Ok(dir) => self.replay_install(dir, &install),
            Err(e)
                if e.downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                self.stage_install(&install, &name)
            }
            Err(e) => Err(e),
        }
    }
    fn replay_install(&self, dir: File, install: &Installation<'_>) -> Result<InstalledMaterial> {
        let material = &install.material;
        private_dir(&dir)?;
        let raw = read_member(&dir, c"record.json", MAX_RECORD)?;
        let record: Record = serde_json::from_slice(&raw)?;
        record.validate()?;
        ensure!(
            canonical_bytes(&record)? == *raw
                && record.operation_id == material.operation_id
                && record.preparation_digest == install.origin
                && record.worker == self.identity
                && record.kernel_boot_id == self.kernel_boot_id
                && same(&record.delivery.lease, &material.delivery.lease)?,
            "material replay changed"
        );
        let result = InstalledMaterial {
            directory: dir,
            record,
        };
        ensure!(
            result.read_private()?.as_slice() == material.bytes.as_slice(),
            "material replay bytes changed"
        );
        // If publication's ACK was lost, repeat the durability boundary.
        self.root.sync_all()?;
        ensure!(
            boot_ns()? < result.record.retrieval_deadline_ns.min(install.deadline()),
            "material replay cannot revive expiry"
        );
        Ok(result)
    }
    fn stage_install(
        &mut self,
        install: &Installation<'_>,
        name: &CStr,
    ) -> Result<InstalledMaterial> {
        let material = &install.material;
        ensure!(
            self.count()? < self.max_leases,
            "material capacity exhausted"
        );
        let pending = CString::new(format!("{}.pending", material.delivery.lease.lease_id))?;
        let rc = unsafe { libc::mkdirat(self.root.as_raw_fd(), pending.as_ptr(), 0o700) };
        ensure!(
            rc == 0,
            "material staging exists or unavailable; recovery required"
        );
        self.root.sync_all()?;
        let dir = directory(&self.root, &pending)?;
        private_dir(&dir)?;
        write_member(&dir, c"material.bin", &material.bytes)?;
        let record = self.write_record(&dir, install)?;
        self.publish(&pending, name)?;
        ensure!(
            boot_ns()? < install.deadline(),
            "material publication expired"
        );
        Ok(InstalledMaterial {
            directory: dir,
            record,
        })
    }
    fn write_record(&self, dir: &File, install: &Installation<'_>) -> Result<Record> {
        let material = &install.material;
        let record = Record {
            schema_version: 1,
            operation_id: material.operation_id,
            preparation_digest: install.origin.clone(),
            worker: self.identity.clone(),
            kernel_boot_id: self.kernel_boot_id,
            clock_source: "CLOCK_BOOTTIME".into(),
            delivery: material.delivery.clone(),
            sent_ns: material.sent_ns,
            received_ns: material.received_ns,
            material_synced_ns: boot_ns()?,
            retrieval_deadline_ns: material.retrieval_deadline_ns,
            login_version: material.login_version,
        };
        record.validate()?;
        let raw = canonical_bytes(&record)?;
        ensure!(raw.len() <= MAX_RECORD, "material record byte bound");
        write_member(dir, c"record.json", &raw)?;
        dir.sync_all()?;
        Ok(record)
    }
    fn publish(&mut self, pending: &CStr, name: &CStr) -> Result<()> {
        #[cfg(any(test, feature = "test-support"))]
        let fault = self.next_fault.take();
        #[cfg(any(test, feature = "test-support"))]
        match fault {
            Some(InstallFault::AfterFiles) => {
                anyhow::bail!("injected material prepublication loss")
            }
            Some(InstallFault::PauseAfterFiles) => {
                std::thread::sleep(std::time::Duration::from_millis(200))
            }
            _ => {}
        }
        let rc = unsafe {
            libc::renameat2(
                self.root.as_raw_fd(),
                pending.as_ptr(),
                self.root.as_raw_fd(),
                name.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        ensure!(rc == 0, "material publication uncertain; recovery required");
        self.root.sync_all()?;
        #[cfg(any(test, feature = "test-support"))]
        if matches!(fault, Some(InstallFault::AfterPublish)) {
            anyhow::bail!("injected material publication ACK loss");
        }
        Ok(())
    }
}
