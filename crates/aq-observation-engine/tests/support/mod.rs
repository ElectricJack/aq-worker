use anyhow::Result;
use aq_observation_engine::lifecycle::*;
use aq_observation_engine::{framing::Frame, rows::*, *};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{cell::Cell, collections::BTreeMap};
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Memory {
    pub tables: BTreeMap<Table, BTreeMap<String, Value>>,
    pub display: BTreeMap<u64, String>,
    #[serde(skip)]
    pub reads: Cell<usize>,
}
impl ReadView for Memory {
    fn get(&self, t: Table, k: &str) -> Result<Option<Value>> {
        self.reads.set(self.reads.get() + 1);
        Ok(self.tables.get(&t).and_then(|v| v.get(k)).cloned())
    }
    fn count(&self, t: Table) -> Result<usize> {
        self.reads.set(self.reads.get() + 1);
        Ok(self.tables.get(&t).map_or(0, |v| v.len()))
    }
    fn display_suffix(&self, start: u64, limit: usize) -> Result<Vec<(u64, String)>> {
        self.reads.set(self.reads.get() + 1);
        Ok(self
            .display
            .range(start..)
            .take(limit)
            .map(|(k, v)| (*k, v.clone()))
            .collect())
    }
}
impl Memory {
    pub fn commit(&mut self, delta: &RowDelta) {
        for ((table, key), value) in &delta.writes {
            let map = self.tables.entry(*table).or_default();
            if let Some(v) = value {
                map.insert(key.clone(), v.clone());
            } else {
                map.remove(key);
            }
            if *table == Table::DisplayByOrdinal {
                let ordinal = key.parse().unwrap();
                if let Some(v) = value {
                    self.display
                        .insert(ordinal, serde_json::from_value(v.clone()).unwrap());
                } else {
                    self.display.remove(&ordinal);
                }
            }
        }
    }
}
/// Test-only restart projection. This intentionally clones the small fixture
/// store so old tests can retain immutable apply semantics; not the engine API.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Normalizer {
    pub context: Context,
    pub memory: Memory,
    pub seen: BTreeMap<String, Seen>,
    pub mutations: Vec<Mutation>,
    pub display_order: Vec<String>,
}
impl std::ops::Deref for Normalizer {
    type Target = Context;
    fn deref(&self) -> &Context {
        &self.context
    }
}
impl std::ops::DerefMut for Normalizer {
    fn deref_mut(&mut self) -> &mut Context {
        &mut self.context
    }
}
impl Normalizer {
    pub fn new(source: Source, max: usize) -> Result<Self> {
        Ok(Self {
            context: Context::new(source, max)?,
            memory: Memory::default(),
            seen: BTreeMap::new(),
            mutations: vec![],
            display_order: vec![],
        })
    }
    pub fn bind_scoped_record_channel(mut self, key: &str) -> Result<Self> {
        self.context = self.context.bind_scoped_record_channel(key)?;
        Ok(self)
    }
    pub fn begin_history_replay(&mut self) {
        self.context.history_replay = true;
    }
    pub fn finish_history_replay(&mut self) {
        self.context.history_replay = false;
    }
    pub fn apply(&self, frame: &Frame, time: u64) -> Result<(Self, Vec<ObservationRecord>)> {
        let delta = normalize(&self.context, &self.memory, frame, time)?;
        let mut next = self.clone();
        next.context = delta.context;
        for ((table, key), value) in &delta.rows.writes {
            if *table == Table::Mutation && self.memory.get(*table, key)?.is_none() {
                next.mutations
                    .push(serde_json::from_value(value.clone().unwrap())?);
            }
        }
        next.memory.commit(&delta.rows);
        next.seen = next
            .memory
            .tables
            .get(&Table::Seen)
            .into_iter()
            .flatten()
            .map(|(k, v)| Ok((k.clone(), serde_json::from_value(v.clone())?)))
            .collect::<Result<_>>()?;
        next.display_order = next.memory.display.values().cloned().collect();
        Ok((next, delta.events))
    }
}
