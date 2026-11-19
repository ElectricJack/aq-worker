//! Eviction for a store that outlives its sources. A finished source is dropped
//! only when no event of it still awaits a receipt, so nothing unsent is lost.
use super::*;
use redb::TableHandle;
impl Store {
    pub fn source_ids(&self) -> Result<Vec<Uuid>> {
        let tx = self.db.begin_read()?;
        let table = tx.open_table(SOURCES)?;
        let mut ids = vec![];
        for row in table.iter()? {
            ids.push(Uuid::from_slice(row?.0.value())?);
        }
        Ok(ids)
    }
    /// Removes the source row and every dedupe/parser row of `id`. `Ok(false)`
    /// while one of its events is still queued; an absent source is released.
    pub fn release_source(&self, id: Uuid) -> Result<bool> {
        let tx = self.begin()?;
        let mut meta = read_meta(&tx)?;
        let mut pending = false;
        for row in tx.open_table(OUTBOX)?.iter()? {
            let event: ObservationRecord = decode(row?.1.value())?;
            pending |= event.body.provenance.source_id == id;
        }
        let prefix = id.as_bytes().as_slice();
        let row = if pending {
            None
        } else {
            let mut sources = tx.open_table(SOURCES)?;
            sources.remove(prefix)?.map(|v| v.value().len() as u64)
        };
        let Some(row) = row else {
            self.abort(tx)?;
            return Ok(!pending);
        };
        meta.sources -= 1;
        meta.logical_bytes -= row;
        for definition in std::iter::once(SEEN).chain(engine_store::TABLES) {
            let mut table = tx.open_table(definition)?;
            let mut keys = vec![];
            for entry in table.range(prefix..)? {
                let (k, v) = entry?;
                if !k.value().starts_with(prefix) {
                    break;
                }
                meta.logical_bytes -= (k.value().len() + v.value().len()) as u64;
                keys.push(k.value().to_vec());
            }
            if definition.name() == SEEN.name() {
                meta.seen -= keys.len() as u64;
            }
            for key in keys {
                table.remove(key.as_slice())?;
            }
        }
        write_meta(&tx, &meta)?;
        self.commit(tx)?;
        Ok(true)
    }
}
