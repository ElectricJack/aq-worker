use super::*;

impl Validate for NodeTerminalPoll {
    fn validate(&self) -> Result<(), ContractError> {
        ensure(
            self.schema_version == 1
                && !self.node_id.is_nil()
                && !self.boot_id.is_nil()
                && self.hold_ms <= 20000
                && self.active_subscriptions <= 32
                && (self.active_subscriptions > 0 || self.hold_ms == 0),
            "poll bounds",
        )
    }
}
fn valid_batch_identity(version: u32, node: Uuid, boot: Uuid, batch: Uuid) -> bool {
    version == 1 && [node, boot, batch].iter().all(|id| !id.is_nil())
}
fn remote_matches(t: &TerminalIdentity, node: Uuid, boot: Uuid) -> bool {
    matches!(t.session,SessionIdentity::Remote{node_id,boot_id,..} if node_id==node&&boot_id==boot)
}
impl Validate for NodeCommandBatch {
    fn validate(&self) -> Result<(), ContractError> {
        ensure(
            valid_batch_identity(
                self.schema_version,
                self.node_id,
                self.boot_id,
                self.batch_id,
            ) && self.commands.len() <= 64,
            "command batch bounds",
        )?;
        for c in &self.commands {
            c.validate()?;
            let t = match c {
                TerminalCommand::Input(v) => &v.lease.terminal,
                TerminalCommand::Resize(v) => &v.lease.terminal,
                TerminalCommand::Controller(v) => &v.terminal,
                TerminalCommand::Watch(v) => &v.terminal,
            };
            ensure(
                remote_matches(t, self.node_id, self.boot_id),
                "batch node binding",
            )?;
        }
        ensure(
            serde_json::to_vec(self)
                .map_err(|e| ContractError::Json(e.to_string()))?
                .len()
                <= MAX_BATCH_BYTES,
            "batch bytes",
        )
    }
}
fn validate_members<T: Validate>(
    items: &[T],
    node: (Uuid, Uuid),
    terminal: impl Fn(&T) -> &TerminalIdentity,
    scope: &'static str,
) -> Result<(), ContractError> {
    for item in items {
        item.validate()?;
        ensure(remote_matches(terminal(item), node.0, node.1), scope)?;
    }
    Ok(())
}
impl Validate for NodeOutputBatch {
    fn validate(&self) -> Result<(), ContractError> {
        ensure(
            valid_batch_identity(
                self.schema_version,
                self.node_id,
                self.boot_id,
                self.batch_id,
            ) && self.frames.len() <= 64
                && self.snapshots.len() <= 32
                && self.gaps.len() <= 64
                && self.receipts.len() <= 64,
            "output batch count",
        )?;
        let node = (self.node_id, self.boot_id);
        validate_members(&self.frames, node, |v| &v.terminal, "frame node")?;
        validate_members(&self.snapshots, node, |v| &v.terminal, "snapshot node")?;
        validate_members(&self.gaps, node, |v| &v.terminal, "gap node")?;
        validate_members(&self.receipts, node, |v| &v.terminal, "receipt node")?;
        ensure(
            serde_json::to_vec(self)
                .map_err(|e| ContractError::Json(e.to_string()))?
                .len()
                <= MAX_BATCH_BYTES,
            "output batch bytes",
        )
    }
}
impl Validate for ResizeCommand {
    fn validate(&self) -> Result<(), ContractError> {
        self.lease.validate_command(
            !self.operation_id.is_nil()
                && self.sequence > 0
                && (1..=1000).contains(&self.columns)
                && (1..=1000).contains(&self.rows),
            self.issued_at_ms..self.deadline_ms,
            ("resize bounds", "resize deadline"),
        )
    }
}
impl Validate for OutputFrame {
    fn validate(&self) -> Result<(), ContractError> {
        self.terminal.validate()?;
        ensure(
            self.sequence > 0
                && !self.bytes.is_empty()
                && self.bytes.len() <= MAX_FRAME_BYTES
                && self.byte_length as usize == self.bytes.len()
                && Digest::of(&self.bytes) == self.digest,
            "output frame",
        )
    }
}
impl Validate for Snapshot {
    fn validate(&self) -> Result<(), ContractError> {
        self.terminal.validate()?;
        ensure(
            !self.snapshot_id.is_nil()
                && !self.capture_request_id.is_nil()
                && (1..=1000).contains(&self.columns)
                && (1..=1000).contains(&self.rows)
                && self.state.len() <= MAX_SNAPSHOT_BYTES
                && Digest::of(&self.state) == self.digest,
            "snapshot bounds",
        )
    }
}
impl Validate for OutputGap {
    fn validate(&self) -> Result<(), ContractError> {
        self.terminal.validate()?;
        ensure(
            self.first_missing > 0 && self.last_missing >= self.first_missing,
            "gap range",
        )
    }
}
impl Validate for InputCommand {
    fn validate(&self) -> Result<(), ContractError> {
        self.validate_envelope()?;
        ensure(
            complete_terminal_input(&self.bytes, self.escape_key),
            "incomplete terminal input",
        )
    }
}
impl InputCommand {
    /// Check the stored envelope independently of whether it can now be sent.
    pub fn validate_envelope(&self) -> Result<(), ContractError> {
        self.lease.validate_command(
            !self.operation_id.is_nil()
                && !self.bytes.is_empty()
                && self.bytes.len() <= MAX_INPUT_BYTES,
            self.issued_at_ms..self.deadline_ms,
            ("input bytes", "input deadline"),
        )
    }
}
impl ControllerLease {
    fn validate_command(
        &self,
        within_bounds: bool,
        window: std::ops::Range<u64>,
        errors: (&'static str, &'static str),
    ) -> Result<(), ContractError> {
        self.validate()?;
        ensure(within_bounds, errors.0)?;
        let (issued, deadline) = (window.start, window.end);
        ensure(
            issued >= self.issued_at_ms
                && deadline > issued
                && deadline - issued <= 2000
                && deadline <= self.expires_at_ms,
            errors.1,
        )
    }
}
impl Validate for ControllerLease {
    fn validate(&self) -> Result<(), ContractError> {
        self.terminal.validate()?;
        ensure(
            self.generation > 0
                && !self.lease_id.is_nil()
                && self.expires_at_ms > self.issued_at_ms
                && self.expires_at_ms - self.issued_at_ms <= 15000
                && self.expires_at_ms <= self.execution_expires_at_ms,
            "controller lease",
        )
    }
}
impl Validate for TerminalIdentity {
    fn validate(&self) -> Result<(), ContractError> {
        self.session.validate()?;
        ensure(
            self.epoch > 0 && !self.terminal_id.is_nil(),
            "terminal epoch",
        )
    }
}
impl Validate for TerminalCommand {
    fn validate(&self) -> Result<(), ContractError> {
        match self {
            Self::Input(v) => v.validate(),
            Self::Resize(v) => v.validate(),
            Self::Controller(v) => v.validate(),
            Self::Watch(v) => {
                v.terminal.validate()?;
                ensure(
                    !v.subscription_id.is_nil() && v.expires_at_ms > 0,
                    "subscription identity/expiry",
                )
            }
        }
    }
}
impl Validate for InputReceipt {
    fn validate(&self) -> Result<(), ContractError> {
        self.terminal.validate()?;
        ensure(!self.operation_id.is_nil(), "receipt identity")
    }
}
