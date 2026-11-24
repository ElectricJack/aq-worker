use super::*;

impl Validate for LaunchManifest {
    fn validate(&self) -> Result<(), ContractError> {
        ensure(self.schema_version == 1, "schema version")?;
        self.bindings.validate_identity()?;
        ensure(
            self.modules.len() <= 64
                && self.credentials.len() <= 16
                && self.context.references.len() <= 128,
            "manifest cardinality",
        )?;
        self.validate_harness()?;
        self.permissions.validate_scopes()?;
        self.context.validate_budget(self.model.max_output_tokens)?;
        self.resources.validate_bounds()?;
        self.terminal.validate_bounds()?;
        ensure(
            ![
                self.bindings.assignment_generation,
                self.bindings.project_revision,
                self.bindings.task_revision,
            ]
            .contains(&0),
            "binding revisions",
        )
    }
}
impl LaunchBindings {
    fn validate_identity(&self) -> Result<(), ContractError> {
        self.session.validate()?;
        ensure(
            [
                self.project_id,
                self.graph_id,
                self.task_id,
                self.attempt_id,
            ]
            .iter()
            .all(|id| !id.is_nil()),
            "nil launch identity",
        )?;
        if let SessionIdentity::Remote {
            attempt_id,
            assignment_generation,
            ..
        } = self.session
        {
            ensure(
                (attempt_id, assignment_generation)
                    == (self.attempt_id, self.assignment_generation),
                "remote launch identity",
            )?;
        }
        Ok(())
    }
}
impl LaunchManifest {
    fn validate_harness(&self) -> Result<(), ContractError> {
        let declared = |id: &str| {
            self.harness
                .required_capabilities
                .iter()
                .any(|c| c.id.as_str() == id && c.version == 1)
        };
        ensure(
            declared(NATIVE_CONFINEMENT_CAPABILITY) != declared(CONTAINER_CONFINEMENT_CAPABILITY),
            "exactly one harness confinement mode",
        )?;
        for id in ["aq.harness.startup-context", "aq.harness.stop"] {
            ensure(
                self.harness.required_capabilities.contains(&Capability {
                    id: Text::new(id)?,
                    version: 1,
                }),
                "mandatory harness capability",
            )?;
        }
        let mut modules = BTreeSet::new();
        for m in &self.modules {
            ensure(modules.insert(m.id.clone()), "duplicate module identity")?;
        }
        require_capabilities(
            &self.harness.required_capabilities,
            &self.harness.required_capabilities,
        )?;
        if self
            .model
            .spending_budget
            .as_ref()
            .is_some_and(|b| b.basis == BudgetBasis::TaskCumulativeEnforcedCost)
        {
            ensure(
                self.harness.required_capabilities.contains(&Capability {
                    id: Text::new("aq.budget.task-cost")?,
                    version: 1,
                }),
                "enforced spending requires native capability",
            )?;
        }
        Ok(())
    }
}
impl PermissionPlan {
    fn validate_scopes(&self) -> Result<(), ContractError> {
        ensure(
            [
                self.requested.len(),
                self.effective.len(),
                self.required.len(),
                self.omissions.len(),
            ]
            .into_iter()
            .all(|count| count <= 128),
            "permission cardinality",
        )?;
        for scope in &self.effective {
            ensure(
                self.requested.contains(scope),
                "effective permission exceeds request",
            )?;
        }
        for scope in &self.required {
            ensure(self.effective.contains(scope), "required permission denied")?;
        }
        Ok(())
    }
}
impl ContextPlan {
    fn validate_budget(&self, output_tokens: u64) -> Result<(), ContractError> {
        let mut tokens = 0u64;
        let mut bytes = 0u64;
        for r in &self.references {
            ensure(
                r.bytes <= 1048576 && r.tokens <= 1048576,
                "context reference bounds",
            )?;
            if r.delivery == ContextDelivery::Startup {
                bytes = bytes
                    .checked_add(r.bytes)
                    .ok_or(ContractError::Invalid("context byte overflow"))?;
                tokens = tokens
                    .checked_add(r.tokens)
                    .ok_or(ContractError::Invalid("context overflow"))?;
            } else {
                ensure(!r.required, "required context deferred")?;
            }
        }
        ensure(
            bytes <= self.startup_byte_budget
                && self.startup_byte_budget <= 1048576
                && tokens <= self.startup_token_budget
                && self
                    .startup_token_budget
                    .checked_add(self.reserved_output_tokens)
                    .is_some_and(|n| n <= self.context_window_tokens),
            "context budget",
        )?;
        ensure(
            (1..=self.reserved_output_tokens).contains(&output_tokens),
            "model output budget",
        )
    }
}
impl ResourceLimits {
    fn validate_bounds(&self) -> Result<(), ContractError> {
        ensure(
            ![
                self.memory_bytes,
                self.cpu_millis,
                u64::from(self.process_limit),
                self.wall_time_ms,
            ]
            .contains(&0)
                && self.stop_grace_ms <= self.wall_time_ms,
            "resource limits",
        )
    }
}
impl TerminalPolicy {
    fn validate_bounds(&self) -> Result<(), ContractError> {
        ensure(
            (1..=4096).contains(&self.max_input_bytes)
                && (self.max_input_bytes..=32768).contains(&self.max_pending_input_bytes)
                && (1..=65536).contains(&self.max_frame_bytes)
                && (self.max_frame_bytes..=1048576).contains(&self.replay_bytes),
            "terminal limits",
        )
    }
}
