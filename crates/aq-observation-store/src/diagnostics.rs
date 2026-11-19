//! Cold diagnostics only. These deliberately scan retained rows and are never
//! used by collection, delivery, ACK, admission or authorization.
use super::*;
use redb::TableHandle;
fn part(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_be_bytes());
    hash.update(bytes);
}
fn header(hash: &mut Sha256, name: &str, count: u64) {
    part(hash, name.as_bytes());
    hash.update(count.to_be_bytes());
}
impl Store {
    /// SHA256 of domain-tagged table names/counts and length-prefixed ordered
    /// key/value bytes. O(retained rows), read-only, diagnostic, not authority.
    pub fn audit_fingerprint(&self) -> Result<String> {
        ensure!(
            !self.poisoned.get(),
            "uncertain store; reopen before diagnostic fingerprint"
        );
        self.identity.check()?;
        self.audit()?;
        let tx = self.db.begin_read()?;
        let mut hash = Sha256::new();
        hash.update(b"aq-observation-row-diagnostic-v1\0");
        for definition in [META, DELIVERY] {
            let table = tx.open_table(definition)?;
            header(&mut hash, definition.name(), table.len()?);
            for row in table.iter()? {
                let (k, v) = row?;
                part(&mut hash, k.value().as_bytes());
                part(&mut hash, v.value());
            }
        }
        for definition in [SOURCES, SEEN, EVENTS]
            .into_iter()
            .chain(engine_store::TABLES)
        {
            let table = tx.open_table(definition)?;
            header(&mut hash, definition.name(), table.len()?);
            for row in table.iter()? {
                let (k, v) = row?;
                part(&mut hash, k.value());
                part(&mut hash, v.value());
            }
        }
        let table = tx.open_table(OUTBOX)?;
        header(&mut hash, OUTBOX.name(), table.len()?);
        for row in table.iter()? {
            let (k, v) = row?;
            part(&mut hash, &k.value().to_be_bytes());
            part(&mut hash, v.value());
        }
        Ok(format!("{:x}", hash.finalize()))
    }
}

impl Store {
    /// Cold retained-history checksum. Exclusions must be at most16 currently
    /// queued trusted local control events; caller parser names cannot exempt
    /// ordinary records. Source/queue metadata is intentionally outside this
    /// checksum so a control fence can be checked for preserving prior history.
    pub fn audit_retained_fingerprint(&self, exclude: &[Uuid]) -> Result<String> {
        ensure!(!self.poisoned.get(), "uncertain store; reopen first");
        self.identity.check()?;
        self.audit()?;
        let excluded: std::collections::BTreeSet<_> = exclude.iter().copied().collect();
        ensure!(
            excluded.len() == exclude.len() && excluded.len() <= 16,
            "diagnostic exclusion bound"
        );
        let tx = self.db.begin_read()?;
        let mut found = std::collections::BTreeSet::new();
        for row in tx.open_table(OUTBOX)?.iter()? {
            let (_, value) = row?;
            let event: ObservationRecord = decode(value.value())?;
            if excluded.contains(&event.event_id) {
                ensure!(
                    budget::is_urgent(&event),
                    "only trusted local control may be excluded"
                );
                found.insert(event.event_id);
            }
        }
        ensure!(found == excluded, "excluded control event absent");
        let mut hash = Sha256::new();
        hash.update(b"aq-observation-retained-diagnostic-v1\0");
        for definition in [SEEN].into_iter().chain(engine_store::TABLES) {
            let table = tx.open_table(definition)?;
            header(&mut hash, definition.name(), table.len()?);
            for row in table.iter()? {
                let (k, v) = row?;
                part(&mut hash, k.value());
                part(&mut hash, v.value());
            }
        }
        let table = tx.open_table(EVENTS)?;
        header(
            &mut hash,
            EVENTS.name(),
            table
                .len()?
                .checked_sub(excluded.len() as u64)
                .context("event exclusion count")?,
        );
        for row in table.iter()? {
            let (k, v) = row?;
            let id = Uuid::from_slice(k.value())?;
            if !excluded.contains(&id) {
                part(&mut hash, k.value());
                part(&mut hash, v.value());
            }
        }
        Ok(format!("{:x}", hash.finalize()))
    }
}
