use super::*;

impl ControllerLease {
    fn key(&self) -> (Uuid, Text) {
        (self.lease_id, self.principal_id.clone())
    }
    fn validate_owner(
        &self,
        terminal: &TerminalIdentity,
        generation: u64,
    ) -> Result<(), ContractError> {
        if (&self.terminal, self.generation) != (terminal, generation) {
            return Err(ContractError::Stale);
        }
        Ok(())
    }
    fn validate_lease_identity(&self, saved: Option<&(Uuid, Text)>) -> Result<(), ContractError> {
        if saved
            .is_some_and(|(id, principal)| (id, principal) != (&self.lease_id, &self.principal_id))
        {
            return Err(ContractError::Stale);
        }
        Ok(())
    }
}
impl InputConformance {
    fn validate_fence(&self, c: &InputCommand, now_ms: u64) -> Result<(), ContractError> {
        c.lease
            .validate_owner(&self.terminal, self.controller_generation)?;
        if !(c.issued_at_ms..c.deadline_ms.min(c.lease.expires_at_ms)).contains(&now_ms) {
            return Err(ContractError::Expired);
        }
        Ok(())
    }
    pub fn new(terminal: TerminalIdentity, controller_generation: u64) -> Self {
        Self {
            terminal,
            controller_generation,
            records: BTreeMap::new(),
            pending: 0,
            uncertain: false,
            resolution_capture: None,
            used_capture_ids: BTreeSet::new(),
            controller_lease: None,
        }
    }
    pub fn receive(
        &mut self,
        c: &InputCommand,
        now_ms: u64,
    ) -> Result<InputDisposition, ContractError> {
        c.validate()?;
        let digest = canonical_digest(c)?;
        if let Some((old, status, _)) = self.records.get(&c.operation_id) {
            return if *old != digest {
                Err(ContractError::Conflict)
            } else {
                Ok(*status)
            };
        }
        self.validate_fence(c, now_ms)?;
        if self.uncertain {
            return Err(ContractError::DeliveryUnknown);
        }
        c.lease
            .validate_lease_identity(self.controller_lease.as_ref())?;
        self.controller_lease = Some(c.lease.key());
        ensure(self.records.len() < 1024, "receipt history bounds")?;
        ensure(
            self.pending + c.bytes.len() <= MAX_PENDING_INPUT_BYTES,
            "input backlog",
        )?;
        self.pending += c.bytes.len();
        self.records.insert(
            c.operation_id,
            (digest, InputDisposition::Received, c.bytes.len()),
        );
        Ok(InputDisposition::Received)
    }
    pub fn begin_injection(&mut self, c: &InputCommand, now_ms: u64) -> Result<(), ContractError> {
        self.validate_fence(c, now_ms)?;
        if self.controller_lease.as_ref() != Some(&c.lease.key()) {
            return Err(ContractError::Stale);
        }
        let r = self
            .records
            .get_mut(&c.operation_id)
            .ok_or(ContractError::Stale)?;
        if r.0 != canonical_digest(c)? {
            return Err(ContractError::Conflict);
        }
        ensure(
            r.1 == InputDisposition::Received && !self.uncertain,
            "injection transition",
        )?;
        r.1 = InputDisposition::Injecting;
        Ok(())
    }
    pub fn finish(&mut self, id: Uuid, confirmed: bool) -> Result<(), ContractError> {
        let r = self.records.get_mut(&id).ok_or(ContractError::Stale)?;
        ensure(r.1 == InputDisposition::Injecting, "finish transition")?;
        r.1 = if confirmed {
            InputDisposition::Applied
        } else {
            InputDisposition::DeliveryUnknown
        };
        self.pending -= r.2;
        self.uncertain |= !confirmed;
        Ok(())
    }
    /// Reserve a unique capture token after uncertainty. Quiet output is not freshness proof.
    pub fn request_resolution_capture(
        &mut self,
        request_id: Uuid,
        controller_generation: u64,
    ) -> Result<(), ContractError> {
        ensure(
            self.uncertain && !request_id.is_nil() && self.used_capture_ids.len() < 1024,
            "resolution request",
        )?;
        if controller_generation != self.controller_generation {
            return Err(ContractError::Stale);
        }
        ensure(
            self.used_capture_ids.insert(request_id),
            "reused capture request",
        )?;
        self.resolution_capture = Some((request_id, controller_generation));
        Ok(())
    }
    pub fn resolve_unknown(&mut self, fresh_snapshot: &Snapshot) -> Result<(), ContractError> {
        ensure(
            self.uncertain
                && self.resolution_capture
                    == Some((
                        fresh_snapshot.capture_request_id,
                        self.controller_generation,
                    )),
            "fresh capture required",
        )?;
        fresh_snapshot.validate()?;
        ensure(
            fresh_snapshot.terminal == self.terminal,
            "resolution identity",
        )?;
        self.uncertain = false;
        self.resolution_capture = None;
        Ok(())
    }
}
impl ReplayConformance {
    pub fn from_snapshot(s: &Snapshot) -> Result<Self, ContractError> {
        s.validate()?;
        Ok(Self {
            terminal: s.terminal.clone(),
            next: s
                .through_sequence
                .checked_add(1)
                .ok_or(ContractError::Invalid("sequence overflow"))?,
            seen: BTreeMap::new(),
            retained: s.state.len(),
        })
    }
    pub fn apply(&mut self, f: &OutputFrame) -> Result<bool, ContractError> {
        f.validate()?;
        if f.terminal != self.terminal {
            return Err(ContractError::Stale);
        }
        if let Some(d) = self.seen.get(&f.sequence) {
            return if *d == f.digest {
                Ok(false)
            } else {
                Err(ContractError::Conflict)
            };
        }
        if f.sequence < self.next {
            return Ok(false);
        }
        ensure(f.sequence == self.next, "explicit gap/snapshot required")?;
        ensure(
            self.retained + f.bytes.len() <= MAX_REPLAY_BYTES,
            "replay backlog",
        )?;
        self.next = self
            .next
            .checked_add(1)
            .ok_or(ContractError::Invalid("sequence overflow"))?;
        self.retained += f.bytes.len();
        self.seen.insert(f.sequence, f.digest.clone());
        Ok(true)
    }
}
impl ResizeConformance {
    pub fn new(terminal: TerminalIdentity, controller_generation: u64) -> Self {
        Self {
            terminal,
            controller_generation,
            last_sequence: 0,
            history: BTreeMap::new(),
            controller_lease: None,
        }
    }
    pub fn apply(&mut self, c: &ResizeCommand, now_ms: u64) -> Result<bool, ContractError> {
        c.validate()?;
        c.lease
            .validate_owner(&self.terminal, self.controller_generation)?;
        if !(c.issued_at_ms..c.deadline_ms).contains(&now_ms) {
            return Err(ContractError::Expired);
        }
        c.lease
            .validate_lease_identity(self.controller_lease.as_ref())?;
        let d = canonical_digest(c)?;
        if let Some(old) = self.history.get(&c.operation_id) {
            return if *old == d {
                Ok(false)
            } else {
                Err(ContractError::Conflict)
            };
        }
        if c.sequence <= self.last_sequence {
            return Err(ContractError::Stale);
        }
        ensure(self.history.len() < 1024, "resize history bounds")?;
        self.controller_lease = Some(c.lease.key());
        self.last_sequence = c.sequence;
        self.history.insert(c.operation_id, d);
        Ok(true)
    }
}
impl BatchConformance {
    fn observe<T: Serialize + Validate>(
        &mut self,
        id: Uuid,
        batch: &T,
    ) -> Result<bool, ContractError> {
        let digest = canonical_digest(batch)?;
        if let Some(old) = self.seen.get(&id) {
            return if *old == digest {
                Ok(false)
            } else {
                Err(ContractError::Conflict)
            };
        }
        ensure(self.seen.len() < 1024, "batch history bounds")?;
        self.seen.insert(id, digest);
        Ok(true)
    }
    pub fn output(&mut self, batch: &NodeOutputBatch) -> Result<bool, ContractError> {
        self.observe(batch.batch_id, batch)
    }
    pub fn commands(&mut self, batch: &NodeCommandBatch) -> Result<bool, ContractError> {
        self.observe(batch.batch_id, batch)
    }
}
