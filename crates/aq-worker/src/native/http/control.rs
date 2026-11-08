//! Fresh control deliveries are process-local observations, not host permits.
//! The driver must durably own operations, reservations and recovery separately.
use super::*;
use anyhow::Context;
use aq_admission::{
    control as wire,
    startup::NativeOffer,
    transport::{Accept, DeliveryClock, Poll, PreparationPlan, Prepare, Reconcile, Renew},
};

pub struct PollObservation {
    pub page: wire::NativePollPage,
    /// Corresponds by index to page.offers; never an execution permit.
    pub offer_deadlines_ns: Vec<u64>,
}
/// Opaque construction: a parsed plan cannot manufacture a delivery observation.
pub struct Preparation {
    sent_ns: u64,
    received_ns: u64,
    message: wire::PreparationDelivery,
    deadline_ns: u64,
}
impl Preparation {
    pub fn sent_ns(&self) -> u64 {
        self.sent_ns
    }
    pub fn received_ns(&self) -> u64 {
        self.received_ns
    }
    pub fn message(&self) -> &wire::PreparationDelivery {
        &self.message
    }
    pub fn plan(&self) -> &PreparationPlan {
        &self.message.original
    }
    pub fn deadline_ns(&self) -> u64 {
        self.deadline_ns
    }
}
/// No Deserialize/Serialize. Durable recovery requires its own checked boundary.
/// These network facts are not actual installed material or physical host proof.
pub struct Granted {
    initial_prepare_digest: aq_admission::Digest,
    initial_sent_ns: u64,
    initial_received_ns: u64,
    initial: wire::InitialGrantDelivery,
    current: Option<wire::RenewedAuthority>,
    deadline_ns: u64,
    hard_deadline_ns: u64,
    journal_revision: u64,
}
impl Granted {
    pub fn initial_prepare_digest(&self) -> &aq_admission::Digest {
        &self.initial_prepare_digest
    }
    pub fn initial_sent_ns(&self) -> u64 {
        self.initial_sent_ns
    }
    pub fn initial_received_ns(&self) -> u64 {
        self.initial_received_ns
    }
    pub fn initial(&self) -> &wire::InitialGrantDelivery {
        &self.initial
    }
    pub fn renewal(&self) -> Option<&wire::RenewedAuthority> {
        self.current.as_ref()
    }
    pub fn deadline_ns(&self) -> u64 {
        self.deadline_ns
    }
    pub fn hard_deadline_ns(&self) -> u64 {
        self.hard_deadline_ns
    }
    pub fn versions(&self) -> &wire::Versions {
        self.current
            .as_ref()
            .map_or(&self.initial.versions, |v| &v.versions)
    }
}
#[derive(Debug)]
pub struct ControlFailure(pub wire::FailureCode);
impl std::fmt::Display for ControlFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "native control refused: {:?}", self.0)
    }
}
impl std::error::Error for ControlFailure {}
struct Received {
    reply: wire::ControlReply,
    nonce: Uuid,
    sent_ns: u64,
    received_ns: u64,
}
fn prepare_digest(request: &Prepare) -> Result<aq_admission::Digest> {
    let mut value = serde_json::to_value(request)?;
    value.sort_all_objects();
    Ok(aq_admission::Digest::of(&serde_json::to_vec(&value)?))
}
fn same(a: &impl Serialize, b: &impl Serialize) -> Result<bool> {
    Ok(serde_json::to_value(a)? == serde_json::to_value(b)?)
}
fn live(deadline: u64) -> Result<()> {
    ensure!(boot_ns()? < deadline, "native local authority expired");
    Ok(())
}
/// Local deadline for an offered poll delivery. The server retimes
/// `valid_for_ms` from the immutable offer expiry when it builds the reply,
/// i.e. after its own poll processing (selection alone is ~0.7-0.9 s). That
/// elapsed time is therefore already deducted from `valid_for_ms`; anchoring
/// the remaining window at poll send would deduct it a second time from a
/// window that is only ~1.4-1.8 s (I0 run 13 offer_window_race). Anchor at
/// response receipt instead. The server remains authoritative for a late
/// Accept; this only stops the worker discarding a still-live offer.
fn offer_deadline_ns(clock: &DeliveryClock, sent_ns: u64, received_ns: u64) -> Result<u64> {
    clock.validate()?;
    ensure!(received_ns >= sent_ns, "monotonic response interval");
    received_ns
        .checked_add(
            clock
                .valid_for_ms
                .checked_mul(1_000_000)
                .context("native offer deadline overflow")?,
        )
        .context("native offer deadline overflow")
}
impl NativeClient {
    async fn control_reply(
        &self,
        client: &reqwest::Client,
        path: &str,
        request: &impl Serialize,
    ) -> Result<Received> {
        let bytes = serde_json::to_vec(request)?;
        ensure!(bytes.len() <= 65_536, "native control request size");
        let nonce = Uuid::new_v4();
        let sent_ns = boot_ns()?;
        let response = client
            .post(format!("{}/v1/worker/profiled/{path}", self.base))
            .header(header::AUTHORIZATION, self.credential.clone())
            .header(header::CONTENT_TYPE, "application/json")
            .header("aq-delivery-nonce", nonce.to_string())
            .body(bytes)
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("native control transport failed; disposition unknown"))?;
        if !response.status().is_success() {
            return Err(refused(response).await);
        }
        let bytes = body(response, wire::MAX_REPLY_BYTES).await?;
        let received_ns = boot_ns()?;
        let reply = wire::decode_reply(&bytes)?;
        if let wire::ControlReply::Failure(ref failure) = reply {
            return Err(ControlFailure(failure.code).into());
        }
        Ok(Received {
            reply,
            nonce,
            sent_ns,
            received_ns,
        })
    }
    pub async fn poll(&self, request: &Poll) -> Result<PollObservation> {
        TransportDocument::Poll(request.clone()).validate()?;
        ensure!(request.identity.node_id == self.node, "native node scope");
        let _slot = self.control_slots.try_acquire()?;
        let got = self.control_reply(&self.control, "poll", request).await?;
        let wire::ControlReply::Poll(page) = got.reply else {
            anyhow::bail!("unexpected native poll reply");
        };
        page.validate_for(request)?;
        let offer_deadlines_ns = page
            .offers
            .iter()
            .map(|v| {
                let deadline = offer_deadline_ns(&v.offered.clock, got.sent_ns, got.received_ns)?;
                live(deadline)?;
                Ok(deadline)
            })
            .collect::<Result<_>>()?;
        Ok(PollObservation {
            page,
            offer_deadlines_ns,
        })
    }
    pub async fn accept(
        &self,
        request: &Accept,
        reservation_deadline_ns: u64,
        previous: Option<&Preparation>,
    ) -> Result<Preparation> {
        TransportDocument::Accept(request.clone()).validate()?;
        self.scope(&request.acceptance.binding)?;
        if let Some(previous) = previous {
            previous
                .message
                .validate_for(request, previous.message.delivery.nonce)?;
        }
        live(reservation_deadline_ns)?;
        let cap = previous.map_or(reservation_deadline_ns, |p| {
            p.deadline_ns.min(reservation_deadline_ns)
        });
        live(cap)?;
        let _slot = self.control_slots.try_acquire()?;
        let got = self.control_reply(&self.control, "accept", request).await?;
        let wire::ControlReply::Preparation(message) = got.reply else {
            anyhow::bail!("unexpected native preparation reply");
        };
        message.validate_for(request, got.nonce)?;
        if let Some(previous) = previous {
            ensure!(
                same(&message.original, &previous.message.original)?,
                "preparation replay changed"
            );
            message
                .versions
                .validate_after(&previous.message.versions)?;
        }
        let deadline_ns =
            message
                .delivery
                .client_deadline_ns(got.sent_ns, got.received_ns, Some(cap))?;
        live(deadline_ns)?;
        Ok(Preparation {
            sent_ns: got.sent_ns,
            received_ns: got.received_ns,
            message,
            deadline_ns,
        })
    }
    pub async fn prepare(
        &self,
        request: &Prepare,
        plan: &Preparation,
        offer: &NativeOffer,
        previous: Option<&Granted>,
    ) -> Result<Granted> {
        TransportDocument::Prepare(request.clone()).validate()?;
        self.scope(&request.binding)?;
        offer.validate()?;
        ensure!(
            request.binding == plan.plan().binding
                && request.binding == offer.offered.offer.binding
                && request.guardian_id == plan.plan().guardian_id
                && request.launch_id == plan.plan().launch_id
                && same(&request.reservation, &plan.plan().reservation)?,
            "preparation owner differs from accepted plan"
        );
        live(plan.deadline_ns)?;
        let request_digest = prepare_digest(request)?;
        if let Some(previous) = previous {
            ensure!(
                previous.initial_prepare_digest == request_digest,
                "initial grant retry request changed"
            );
            previous.initial.validate_for(
                request,
                plan.plan(),
                offer,
                previous.initial.delivery.nonce,
            )?;
            ensure!(
                previous.current.is_none(),
                "initial grant retry after renewal"
            );
            live(previous.deadline_ns)?;
        }
        let _slot = self.control_slots.try_acquire()?;
        let got = self
            .control_reply(&self.control, "prepare", request)
            .await?;
        let wire::ControlReply::InitialGrant(message) = got.reply else {
            anyhow::bail!("unexpected native initial grant reply");
        };
        message.validate_for(request, plan.plan(), offer, got.nonce)?;
        if let Some(previous) = previous {
            ensure!(
                same(&message.original, &previous.initial.original)?
                    && message.operation_id == previous.initial.operation_id
                    && message.versions == previous.initial.versions
                    && message.execution_deadline_ms == previous.initial.execution_deadline_ms,
                "initial grant replay changed"
            );
        }
        // Hard execution may outlast DeliveryClock's two-minute lease bound.
        // Charge the same full network interval using checked arithmetic.
        let mut hard_deadline_ns = message
            .execution_deadline_ms
            .checked_sub(message.delivery.clock.server_time_ms)
            .and_then(|ms| ms.checked_mul(1_000_000))
            .and_then(|ns| got.sent_ns.checked_add(ns))
            .filter(|end| got.received_ns >= got.sent_ns && got.received_ns < *end)
            .ok_or_else(|| anyhow::anyhow!("invalid local hard execution deadline"))?;
        let mut cap = plan.deadline_ns;
        if let Some(previous) = previous {
            hard_deadline_ns = hard_deadline_ns.min(previous.hard_deadline_ns);
            cap = cap.min(previous.deadline_ns);
        }
        let deadline_ns = message.delivery.client_deadline_ns(
            got.sent_ns,
            got.received_ns,
            Some(cap.min(hard_deadline_ns)),
        )?;
        live(deadline_ns)?;
        Ok(Granted {
            initial_prepare_digest: request_digest,
            initial_sent_ns: got.sent_ns,
            initial_received_ns: got.received_ns,
            initial: message,
            current: None,
            deadline_ns,
            hard_deadline_ns,
            journal_revision: request.journal_revision,
        })
    }
    pub async fn renew(&self, request: &Renew, previous: &Granted) -> Result<Granted> {
        TransportDocument::Renew(request.clone()).validate()?;
        self.scope(&request.binding)?;
        let initial = &previous.initial.original;
        ensure!(
            request.binding == initial.grant.binding
                && request.grant_id == initial.grant.grant_id
                && request.reservation_id == initial.reservation_id
                && request.guardian_id == initial.guardian_id
                && request.launch_id == initial.launch_id,
            "renewal owner differs from installed grant"
        );
        ensure!(
            request.journal_revision >= previous.journal_revision,
            "renewal journal regression"
        );
        live(previous.deadline_ns)?;
        live(previous.hard_deadline_ns)?;
        let _slot = self.renewal_slots.try_acquire()?;
        let got = self.control_reply(&self.renewal, "renew", request).await?;
        let wire::ControlReply::Renewed(message) = got.reply else {
            anyhow::bail!("unexpected native renewal reply");
        };
        message.validate_for(request, &previous.initial, got.nonce)?;
        if let Some(last) = &previous.current {
            message.validate_after(last)?;
        }
        let deadline_ns = message.client_renewal_deadline_ns(
            got.sent_ns,
            got.received_ns,
            previous.deadline_ns,
            previous.hard_deadline_ns,
            previous.versions().grant_revision,
        )?;
        live(deadline_ns)?;
        Ok(Granted {
            initial_prepare_digest: previous.initial_prepare_digest.clone(),
            initial_sent_ns: previous.initial_sent_ns,
            initial_received_ns: previous.initial_received_ns,
            initial: previous.initial.clone(),
            current: Some(message),
            deadline_ns,
            hard_deadline_ns: previous.hard_deadline_ns,
            journal_revision: request.journal_revision,
        })
    }
    /// Read-only current authority. This return type has no live-permit conversion.
    pub async fn reconcile(&self, request: &Reconcile) -> Result<wire::ReconcilePage> {
        TransportDocument::Reconcile(request.clone()).validate()?;
        ensure!(request.identity.node_id == self.node, "native node scope");
        let _slot = self.control_slots.try_acquire()?;
        let got = self
            .control_reply(&self.control, "reconcile", request)
            .await?;
        let wire::ControlReply::Reconciled(page) = got.reply else {
            anyhow::bail!("unexpected native reconciliation reply");
        };
        page.validate_for(request, got.nonce)?;
        Ok(page)
    }
}

#[cfg(test)]
mod offer_deadline_tests {
    use super::*;
    const MS: u64 = 1_000_000;

    /// I0 runs 13a/13b: the server stamps a ~1.4 s window after ~0.9 s of poll
    /// processing. A send-anchored deadline expires ~0.5 s after receipt, so the
    /// following proposal round trip lost by -22 ms / +1 ms. The receipt-anchored
    /// deadline keeps the full stated window after the reply arrives.
    #[test]
    fn offer_window_is_anchored_at_poll_receipt_not_send() {
        let clock = DeliveryClock::new(1_000_900, 1_002_300).unwrap();
        assert_eq!(clock.valid_for_ms, 1_400);
        let (sent, received) = (10_000 * MS, 10_920 * MS);
        let send_anchored = clock.client_deadline_ns(sent, received).unwrap();
        assert_eq!(send_anchored - received, 480 * MS);
        let deadline = offer_deadline_ns(&clock, sent, received).unwrap();
        assert_eq!(deadline, received + 1_400 * MS);
        // A proposal sent 500 ms after receipt is still inside the window.
        assert!(received + 500 * MS < deadline && received + 500 * MS >= send_anchored);
    }

    #[test]
    fn poll_latency_beyond_the_stated_window_still_leaves_the_stated_window() {
        // Send-anchoring refused this delivery outright ("consumed remaining TTL").
        let clock = DeliveryClock::new(1_500, 2_000).unwrap();
        let (sent, received) = (MS, 700 * MS);
        assert!(clock.client_deadline_ns(sent, received).is_err());
        assert_eq!(
            offer_deadline_ns(&clock, sent, received).unwrap(),
            received + 500 * MS
        );
    }

    #[test]
    fn offer_deadline_refuses_invalid_clocks_and_intervals() {
        let clock = DeliveryClock::new(1_500, 2_000).unwrap();
        assert!(offer_deadline_ns(&clock, 20, 10).is_err());
        assert!(offer_deadline_ns(&clock, 0, u64::MAX).is_err());
        let mut bad = clock.clone();
        bad.valid_for_ms = 0;
        assert!(offer_deadline_ns(&bad, 0, 1).is_err());
        bad.valid_for_ms = 120_001;
        assert!(offer_deadline_ns(&bad, 0, 1).is_err());
    }
}
