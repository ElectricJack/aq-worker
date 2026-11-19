//! Bounded indexed row views. No method enumerates complete Seen/native history.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::collections::BTreeMap;
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Table {
    Seen,
    Native,
    Snapshot,
    Mutation,
    DisplayById,
    DisplayByOrdinal,
}
/// A storage adapter must scope every lookup to one immutable source revision.
/// The engine bounds requested keys/ranges and never receives a full map.
#[derive(Debug)]
pub struct StateFailure(pub String);
impl std::fmt::Display for StateFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "state read/delta refusal: {}", self.0)
    }
}
impl std::error::Error for StateFailure {}
fn state_error(error: anyhow::Error) -> anyhow::Error {
    StateFailure(error.to_string()).into()
}
pub trait ReadView {
    fn get(&self, table: Table, key: &str) -> Result<Option<Value>>;
    fn count(&self, table: Table) -> Result<usize>;
    /// Return at most `limit` ordered visible display entries, ordinal >= start.
    fn display_suffix(&self, start: u64, limit: usize) -> Result<Vec<(u64, String)>>;
}
pub type RowWrites = BTreeMap<(Table, String), Option<Value>>;
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RowDelta {
    #[serde(with = "delta_encoding")]
    pub writes: RowWrites,
}
impl RowDelta {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.writes.len() <= 512, "normalization row delta bound");
        let mut bytes = 0usize;
        for ((_, key), value) in &self.writes {
            bytes = bytes
                .checked_add(key.len())
                .ok_or_else(|| anyhow::anyhow!("delta size overflow"))?;
            ensure!(!key.is_empty() && key.len() <= 2048, "row key bound");
            if let Some(value) = value {
                let size = serde_json::to_vec(value)?.len();
                ensure!(size <= 65536, "row value bound");
                bytes = bytes
                    .checked_add(size)
                    .ok_or_else(|| anyhow::anyhow!("delta size overflow"))?;
            }
        }
        ensure!(bytes <= 1024 * 1024, "per-record row delta byte bound");
        Ok(())
    }
}
/// Cloning this tentative overlay copies only writes touched by one record.
#[derive(Clone)]
pub(crate) struct Overlay<'a> {
    pub view: &'a dyn ReadView,
    pub delta: RowDelta,
    pub lookups: usize,
    cache: BTreeMap<(Table, String), Option<Value>>,
    counts: BTreeMap<Table, usize>,
    read_bytes: usize,
    write_bytes: usize,
    write_sizes: BTreeMap<(Table, String), usize>,
}
impl<'a> Overlay<'a> {
    pub fn new(view: &'a dyn ReadView) -> Self {
        Self {
            view,
            delta: RowDelta::default(),
            lookups: 0,
            cache: BTreeMap::new(),
            counts: BTreeMap::new(),
            read_bytes: 0,
            write_bytes: 0,
            write_sizes: BTreeMap::new(),
        }
    }
    fn base(&mut self, table: Table, key: &str) -> Result<Option<Value>> {
        self.lookups += 1;
        if self.lookups > 1024 {
            return Err(StateFailure("record indexed lookup bound".into()).into());
        }
        let cache_key = (table, key.to_owned());
        if let Some(value) = self.cache.get(&cache_key) {
            return Ok(value.clone());
        }
        let value = self.view.get(table, key).map_err(state_error)?;
        let size = value
            .as_ref()
            .map(serde_json::to_vec)
            .transpose()?
            .map_or(0, |v| v.len());
        if size > 65536 || self.read_bytes + size + key.len() > 1024 * 1024 {
            return Err(StateFailure("record row read-byte bound".into()).into());
        }
        self.read_bytes += size + key.len();
        self.cache.insert(cache_key, value.clone());
        Ok(value)
    }
    pub fn get<T: DeserializeOwned>(&mut self, table: Table, key: &str) -> Result<Option<T>> {
        let value = if let Some(value) = self.delta.writes.get(&(table, key.into())) {
            value.clone()
        } else {
            self.base(table, key)?
        };
        value
            .map(serde_json::from_value)
            .transpose()
            .map_err(|e| StateFailure(e.to_string()).into())
    }
    fn stage(&mut self, table: Table, key: String, value: Option<Value>) -> Result<()> {
        let size = key.len()
            + value
                .as_ref()
                .map(serde_json::to_vec)
                .transpose()?
                .map_or(0, |v| v.len());
        let map_key = (table, key.clone());
        let bytes = self.write_bytes - self.write_sizes.get(&map_key).copied().unwrap_or(0) + size;
        if key.is_empty()
            || key.len() > 2048
            || size - key.len() > 65536
            || bytes > 1024 * 1024
            || (!self.delta.writes.contains_key(&map_key) && self.delta.writes.len() >= 512)
        {
            return Err(StateFailure("record row delta bound".into()).into());
        }
        self.write_bytes = bytes;
        self.write_sizes.insert(map_key.clone(), size);
        self.delta.writes.insert(map_key, value);
        Ok(())
    }
    pub fn put<T: Serialize>(&mut self, table: Table, key: String, value: T) -> Result<()> {
        self.stage(table, key, Some(serde_json::to_value(value)?))
    }
    pub fn remove(&mut self, table: Table, key: String) -> Result<()> {
        self.stage(table, key, None)
    }
    pub fn count(&mut self, table: Table) -> Result<usize> {
        let mut count = if let Some(count) = self.counts.get(&table) {
            *count
        } else {
            let count = self.view.count(table).map_err(state_error)?;
            self.counts.insert(table, count);
            count
        };
        let writes = self
            .delta
            .writes
            .iter()
            .filter(|((t, _), _)| *t == table)
            .map(|((_, key), value)| (key.clone(), value.is_some()))
            .collect::<Vec<_>>();
        for (key, new) in writes {
            let old = self.base(table, &key)?.is_some();
            match (old, new) {
                (false, true) => {
                    count = count
                        .checked_add(1)
                        .ok_or_else(|| StateFailure("row count overflow".into()))?
                }
                (true, false) => {
                    count = count
                        .checked_sub(1)
                        .ok_or_else(|| StateFailure("row count underflow".into()))?
                }
                _ => {}
            }
        }
        Ok(count)
    }
    pub fn display_suffix(&mut self, start: u64) -> Result<Vec<(u64, String)>> {
        let removed = self
            .delta
            .writes
            .iter()
            .filter(|((t, _), value)| *t == Table::DisplayByOrdinal && value.is_none())
            .count();
        let limit = 33 + removed;
        let base = self
            .view
            .display_suffix(start, limit)
            .map_err(state_error)?;
        if base.len() > limit {
            return Err(StateFailure("view exceeded display range bound".into()).into());
        }
        let bytes = base.iter().try_fold(0usize, |total, (_, id)| {
            if id.is_empty() || id.len() > 4096 {
                return Err(StateFailure("display ID bound".into()));
            }
            total
                .checked_add(id.len())
                .ok_or_else(|| StateFailure("display byte overflow".into()))
        })?;
        if self.read_bytes + bytes > 1024 * 1024 {
            return Err(StateFailure("display read-byte bound".into()).into());
        }
        self.read_bytes += bytes;
        let mut order: BTreeMap<u64, String> = base.into_iter().collect();
        for ((table, key), value) in &self.delta.writes {
            if *table == Table::DisplayByOrdinal {
                let ordinal = key.parse::<u64>()?;
                if ordinal >= start {
                    if let Some(value) = value {
                        order.insert(ordinal, serde_json::from_value(value.clone())?);
                    } else {
                        order.remove(&ordinal);
                    }
                }
            }
        }
        Ok(order.into_iter().take(33).collect())
    }
}

mod delta_encoding {
    use super::*;
    pub fn serialize<S: serde::Serializer>(
        map: &BTreeMap<(Table, String), Option<Value>>,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        map.iter().collect::<Vec<_>>().serialize(serializer)
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<RowWrites, D::Error> {
        let rows = Vec::<((Table, String), Option<Value>)>::deserialize(deserializer)?;
        let mut result = BTreeMap::new();
        for (key, value) in rows {
            if result.insert(key, value).is_some() {
                return Err(serde::de::Error::custom("duplicate delta row"));
            }
        }
        Ok(result)
    }
}
