use super::*;

impl<B: SlotBackend, C: ControlSource> Shared<B, C> {
    pub(super) async fn control_loop(self: Arc<Self>) {
        let mut state = ControlState::default();
        loop {
            self.tick(&mut state).await;
            tokio::time::sleep(state.interval()).await;
        }
    }
    async fn tick(self: &Arc<Self>, state: &mut ControlState) {
        match self.sup.control.control().await {
            Ok(resp) => {
                self.mark_accepted(state);
                if let Err(e) = self.apply(resp).await {
                    tracing::warn!("node control: {e:#}");
                }
            }
            Err(e)
                if e.downcast_ref::<LinkError>()
                    .is_some_and(LinkError::revoked) =>
            {
                self.mark_revoked(state);
            }
            Err(e)
                if e.downcast_ref::<LinkError>()
                    .is_some_and(LinkError::edge_refused) =>
            {
                (state.unauthorized, state.edge) = (0, state.edge + 1);
                tracing::error!(
                    "{e:#} without an AQ answer: an edge in front of the server (Cloudflare Access?) refused this machine's service token. Check the Access application and the token; this is not a revoke, so slots are not paused. Next control poll in {}s",
                    state.interval().as_secs()
                );
            }
            Err(e) => {
                state.unauthorized = 0;
                tracing::warn!("control poll failed: {e:#}");
            }
        }
    }
    fn mark_accepted(&self, state: &mut ControlState) {
        (state.unauthorized, state.edge) = (0, 0);
        if std::mem::take(&mut state.revoked) {
            tracing::info!("the server accepts this machine's credential again; slots resume");
            self.pause.set(REVOKED, false);
        }
    }

    fn mark_revoked(&self, state: &mut ControlState) {
        (state.unauthorized, state.edge) = (state.unauthorized + 1, 0);
        if state.unauthorized >= 3 && !state.revoked {
            state.revoked = true;
            self.pause.set(REVOKED, true);
            tracing::error!(
                "the server refused this machine's credential (revoked from the Swarm page?); all slots are paused. Uninstall the service or enroll the machine again"
            );
        }
    }

    /// The qualification pin a reply may still carry is ignored: the worker runs whatever
    /// model an offer names, under the template's renderer.
    async fn apply(&self, resp: ControlResponse) -> Result<()> {
        let layout = &self.sup.layout;
        if layout.node.is_some() {
            self.apply_node_slots(&resp).await?;
        }
        for id in resp.handoff {
            if let Err(e) = mark_handoff(layout, &self.active_slots(), id, HandoffReason::OwnerStop)
            {
                tracing::warn!(%id, "handoff marker: {e:#}");
            }
        }
        self.pause.set(AWAITING_FIRST_CONTROL_REPLY, false);
        Ok(())
    }

    async fn apply_node_slots(&self, resp: &ControlResponse) -> Result<()> {
        let layout = &self.sup.layout;
        let slots = if self.sup.retiring {
            0
        } else {
            resp.node_slots()
        };
        layout.check_slots(slots)?;
        let enrollment = EnrollmentFile::load(&layout.slot_enrollment(1))?;
        // Explicit node retirement never registers a new boot over old claims.
        if !self.sup.retiring {
            self.sup
                .backend
                .node_control(self.ctx(1, enrollment.node_id, layout.current(1)), resp)
                .await?;
        }
        self.target
            .send_if_modified(|v| std::mem::replace(v, slots) != slots);
        Ok(())
    }
}
