use super::*;
impl Peer {
    pub(super) fn register(&self, value: Value) -> Response {
        let r: RegisterRequest = serde_json::from_value(value).unwrap();
        assert!(r.capabilities.contains(&SOURCE_TRANSFER_CAPABILITY.into()));
        let mut p = self.coordinator.lock().unwrap();
        p.offer.identity = r.identity.clone();
        p.active_identity = r.identity.clone();
        json_response(RegisterResponse {
            identity: r.identity,
            protocol_version: 2,
            capabilities: r.capabilities,
        })
    }

    pub(super) fn poll(&self, value: Value) -> Response {
        let r: PollRequest = serde_json::from_value(value).unwrap();
        let p = self.coordinator.lock().unwrap();
        let mut directives = vec![];
        let offers = if r.available.slots > 0 {
            vec![p.offer.clone()]
        } else {
            if r.available.cpu_millis == 0 {
                self.control_polls.fetch_add(1, Ordering::SeqCst);
            }
            self.control_directive(&p.offer, &mut directives);
            vec![]
        };
        json_response(PollResponse {
            server_time_ms: 100,
            offers,
            directives,
        })
    }

    pub(super) fn control_directive(&self, offer: &Offer, directives: &mut Vec<Directive>) {
        if !self.held.load(Ordering::SeqCst) {
            return;
        }
        if self.cancel {
            directives.push(Directive {
                directive_id: Uuid::from_u128(500),
                attempt: offer.attempt.clone(),
                kind: DirectiveKind::Cancel,
            });
        } else {
            self.release.notify_one();
        }
    }

    pub(super) fn accept(&self, value: Value) -> Response {
        let r = serde_json::from_value(value).unwrap();
        let response = self.coordinator.lock().unwrap().accept(&r).unwrap();
        *self.grant.lock().unwrap() = Some(response.grant.clone());
        json_response(response)
    }

    pub(super) fn renew(&self, value: Value) -> Response {
        let r: RenewRequest = serde_json::from_value(value).unwrap();
        let mut grant = self.grant.lock().unwrap();
        let g = grant.as_mut().unwrap();
        assert_eq!(r.attempt, g.attempt);
        g.grant_sequence += 1;
        json_response(RenewResponse { grant: g.clone() })
    }

    pub(super) fn observe(&self, value: Value) -> Response {
        let r = serde_json::from_value(value).unwrap();
        json_response(self.coordinator.lock().unwrap().observe(&r).unwrap())
    }

    pub(super) fn ack_directive(&self, value: Value) -> Response {
        let r: DirectiveAck = serde_json::from_value(value).unwrap();
        assert_eq!(r.directive_id, Uuid::from_u128(500));
        self.directive_acked.store(true, Ordering::SeqCst);
        self.release.notify_one();
        json_response(Ack {
            operation_id: r.operation_id,
        })
    }

    pub(super) fn reconcile(&self, value: Value) -> Response {
        let r: ReconcileRequest = serde_json::from_value(value).unwrap();
        r.validate().unwrap();
        assert!(self.cancel);
        assert_eq!(r.attempts.len(), 1);
        assert_eq!(
            r.attempts[0].attempt,
            self.coordinator.lock().unwrap().offer.attempt
        );
        json_response(ReconcileResponse {
            attempts: r
                .attempts
                .into_iter()
                .map(|a| ReconcileDisposition {
                    attempt: a.attempt,
                    action: ReconcileAction::AwaitReport,
                    grant: None,
                    result: None,
                })
                .collect(),
            next_cursor: None,
        })
    }
}
