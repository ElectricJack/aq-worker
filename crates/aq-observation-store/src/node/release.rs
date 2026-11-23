use super::*;
impl<P: NodePeer> Service<P> {
    /// Finished sources and rows of earlier boots leave the store once none of
    /// their events awaits a receipt.
    pub(super) fn release(&mut self, store: &Store) -> Result<()> {
        let acked = store.snapshot()?.acked;
        let now = Instant::now();
        self.release_finished(store, acked, now)?;
        self.release_orphans(store, now)
    }
    fn release_finished(&mut self, store: &Store, acked: u64, now: Instant) -> Result<()> {
        let finished: Vec<Uuid> = (self.sources.iter())
            .filter(|(_, t)| {
                t.finish.is_some_and(|deadline| {
                    t.closed || t.file.is_none() || t.drained || now >= deadline
                }) && t.release_tried != Some(acked)
            })
            .map(|(id, _)| *id)
            .collect();
        for id in finished {
            let tracked = self.sources.get_mut(&id).expect("tracked source");
            let reason = tracked.finish_release(acked);
            if let Some(reason) = reason {
                self.status(id, SourceStatus::Incomplete(reason.into()));
            }
            if store.release_source(id)? {
                self.sources.remove(&id);
                self.released.insert(id);
                self.statuses.lock().unwrap().remove(&id);
            }
        }
        Ok(())
    }
    fn release_orphans(&mut self, store: &Store, now: Instant) -> Result<()> {
        for (id, mut orphan) in std::mem::take(&mut self.orphans) {
            if now < orphan.deadline {
                self.orphans.insert(id, orphan);
                continue;
            }
            if !orphan.ensure_fenced(store, id)? {
                self.orphans.insert(id, orphan);
                continue;
            }
            let acked = store.snapshot()?.acked;
            if orphan.release_tried == Some(acked) || !store.release_source(id)? {
                orphan.release_tried = Some(acked);
                self.orphans.insert(id, orphan);
            }
        }
        Ok(())
    }
}

impl Tracked {
    fn finish_release(&mut self, acked: u64) -> Option<&'static str> {
        let reason = if self.file.is_none() && !self.closed {
            Some("its transcript never appeared")
        } else if !self.drained && !self.closed {
            Some("finished before its transcript was fully collected")
        } else {
            None
        };
        self.closed = true;
        self.release_tried = Some(acked);
        reason
    }
}

impl Orphan {
    fn ensure_fenced(&mut self, store: &Store, id: Uuid) -> Result<bool> {
        if self.fenced {
            return Ok(true);
        }
        // A full gap quota prevents the restart fence and is retried next round.
        if store
            .fence_source(id, FENCED_BY_RESTART, wall_ms())
            .is_err()
        {
            ensure!(
                !store.snapshot()?.persistence_uncertain,
                "a failed fence for source {id}"
            );
            return Ok(false);
        }
        self.fenced = true;
        Ok(true)
    }
}
