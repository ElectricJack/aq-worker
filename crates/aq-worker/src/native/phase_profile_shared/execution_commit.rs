macro_rules! execution_commit {
    ($inputs:ty,$preparation:ty,$observation:ty,$variant:ident,$request:ident,$http_prepare:ident,$record:ident,$retry:ident,$commit:ident,$retain:ident,$lookup:ident,$journal_record:ident,$check:ident) => {
        impl InputPipeline {
            /// Keep actual Prepared owners synchronously before the first HTTP poll.
            /// A lost Grant response leaves the original inputs held as history rather
            /// than dropping their workspace/material ownership or retrying admission.
            pub fn $request<'a>(
                &'a self,
                inputs: $inputs,
                preparation: &'a $preparation,
            ) -> impl std::future::Future<Output = Result<PhaseExecutionCommit>> + Send + 'a {
                let retained = self
                    .retained
                    .lock()
                    .map_err(|_| anyhow::anyhow!("retained phase resources poisoned"))
                    .and_then(|mut owners| owners.$retain(inputs));
                async move {
                    let inputs = retained?;
                    let observed = self
                        .client
                        .$http_prepare(
                            &preparation.proposed,
                            &preparation.request,
                            &preparation.observed,
                            inputs.request(),
                            None,
                        )
                        .await?;
                    self.$commit(inputs, Arc::new(observed)).await
                }
            }

            /// Synchronous ownership handoff precedes the first poll or any admission wait.
            /// Pass an Arc clone to retain the actual opaque network observation for
            /// exact wire replay after Accepted expiry. JSON history cannot replace it.
            pub fn $record(
                &self,
                inputs: $inputs,
                observed: impl Into<Arc<$observation>>,
            ) -> impl std::future::Future<Output = Result<PhaseExecutionCommit>> + Send + '_ {
                let observed = observed.into();
                let retained = self
                    .retained
                    .lock()
                    .map_err(|_| anyhow::anyhow!("retained phase resources poisoned"))
                    .and_then(|mut map| map.$retain(inputs));
                async move { self.$commit(retained?, observed).await }
            }
            pub fn $retry(
                &self,
                id: Uuid,
                observed: impl Into<Arc<$observation>>,
            ) -> impl std::future::Future<Output = Result<PhaseExecutionCommit>> + Send + '_ {
                let observed = observed.into();
                let retained = self
                    .retained
                    .lock()
                    .map_err(|_| anyhow::anyhow!("retained phase resources poisoned"))
                    .and_then(|map| map.$lookup(id));
                async move { self.$commit(retained?, observed).await }
            }
            async fn $commit(
                &self,
                inputs: Arc<$inputs>,
                observed: Arc<$observation>,
            ) -> Result<PhaseExecutionCommit> {
                let owned = self.owned.clone();
                let resources = self.retained.clone();
                self.storage(move |owner| {
                    let id = inputs.request().original.binding.attempt_id;
                    let record = owner
                        .journal
                        .$journal_record(id, inputs.request(), &observed)?;
                    let deadline = record.deadline_ns.min(observed.deadline_ns());
                    let hard = record.hard_deadline_ns.min(observed.hard_deadline_ns());
                    let history = RetainedPhaseExecutionHistory {
                        attempt: id,
                        record_digest: record.digest()?,
                        observed_after_sync_ns: boot_ns()?,
                        owner: owned,
                        resources,
                        inputs: RetainedPhaseInputs::$variant(inputs),
                    };
                    if owner
                        .journal
                        .$check(id, &history.record_digest, deadline, hard)
                        .is_err()
                        || history
                            .inputs
                            .verify_execution_inputs(
                                &owner.journal,
                                &owner.materials,
                                deadline.min(hard),
                            )
                            .is_err()
                        || owner
                            .journal
                            .$check(id, &history.record_digest, deadline, hard)
                            .is_err()
                    {
                        return Ok(PhaseExecutionCommit::HistoryOnly(history));
                    }
                    Ok(PhaseExecutionCommit::Usable(DurablePhaseExecution {
                        history,
                        deadline_ns: deadline,
                        hard_deadline_ns: hard,
                    }))
                })
                .await
            }
        }
    };
}
pub(crate) use execution_commit;
