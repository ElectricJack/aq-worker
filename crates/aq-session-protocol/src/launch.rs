use super::*;

impl Validate for LaunchBinding {
    fn validate(&self) -> Result<(), ContractError> {
        self.session.validate()?;
        if let SessionIdentity::Remote {
            attempt_id,
            assignment_generation,
            ..
        } = self.session
        {
            ensure(
                attempt_id == self.attempt_id
                    && assignment_generation == self.assignment_generation,
                "remote binding identity",
            )?;
        }
        ensure(
            self.schema_version == 1
                && self.assignment_generation > 0
                && !self.offer_id.is_nil()
                && !self.attempt_id.is_nil(),
            "launch binding",
        )
    }
}
impl Validate for ProfiledOffer {
    fn validate(&self) -> Result<(), ContractError> {
        self.binding.validate()?;
        self.manifest.validate()?;
        ensure(
            self.manifest.bindings.session == self.binding.session
                && canonical_digest(&self.manifest)? == self.binding.launch_manifest_digest
                && self.manifest.bindings.attempt_id == self.binding.attempt_id
                && self.manifest.bindings.assignment_generation
                    == self.binding.assignment_generation
                && self.manifest.bindings.graph_access_profile.digest
                    == self.binding.graph_access_profile_digest,
            "manifest binding",
        )?;
        ensure(
            self.required_capabilities.contains(&Capability {
                id: Text::new(PROFILED_LAUNCH_CAPABILITY)?,
                version: 1,
            }),
            "profiled negotiation required",
        )?;
        ensure(
            self.required_capabilities.contains(&Capability {
                id: Text::new(TERMINAL_CAPABILITY)?,
                version: 1,
            }),
            "mandatory terminal capability",
        )?;
        require_capabilities(
            &self.manifest.harness.required_capabilities,
            &self.required_capabilities,
        )?;
        require_capabilities(&self.required_capabilities, &self.required_capabilities)
    }
}
impl ProfiledAcceptance {
    pub fn validate_against(&self, offer: &ProfiledOffer) -> Result<(), ContractError> {
        offer.validate()?;
        self.binding.validate()?;
        if self.binding != offer.binding {
            return Err(ContractError::Conflict);
        }
        require_capabilities(&offer.required_capabilities, &self.advertised_capabilities)?;
        require_capabilities(
            &offer.manifest.harness.required_capabilities,
            &self.advertised_capabilities,
        )
    }
}
impl ProfiledGrant {
    pub fn validate_against(
        &self,
        accepted: &ProfiledAcceptance,
        now_ms: u64,
    ) -> Result<(), ContractError> {
        self.binding.validate()?;
        if self.binding != accepted.binding {
            return Err(ContractError::Conflict);
        }
        ensure(!self.grant_id.is_nil(), "nil grant identity")?;
        if self.expires_at_ms <= now_ms {
            return Err(ContractError::Expired);
        }
        Ok(())
    }
}
impl ProfiledJournalRecord {
    pub fn validate_against(
        &self,
        grant: &ProfiledGrant,
        accepted: &ProfiledAcceptance,
    ) -> Result<(), ContractError> {
        self.binding.validate()?;
        let grant_matches = self.grant_id == grant.grant_id && !self.grant_id.is_nil();
        let bindings_match = self.binding == grant.binding && self.binding == accepted.binding;
        if !(bindings_match && grant_matches) {
            return Err(ContractError::Conflict);
        }
        Ok(())
    }
}
impl Validate for ProfiledAcceptance {
    fn validate(&self) -> Result<(), ContractError> {
        self.binding.validate()?;
        require_capabilities(&[], &self.advertised_capabilities)
    }
}
impl Validate for ProfiledGrant {
    fn validate(&self) -> Result<(), ContractError> {
        self.binding.validate()?;
        ensure(
            !self.grant_id.is_nil() && self.expires_at_ms > 0,
            "grant identity/expiry",
        )
    }
}
impl Validate for ProfiledJournalRecord {
    fn validate(&self) -> Result<(), ContractError> {
        self.binding.validate()?;
        ensure(!self.grant_id.is_nil(), "journal grant identity")
    }
}
