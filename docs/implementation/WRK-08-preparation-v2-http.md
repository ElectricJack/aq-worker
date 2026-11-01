# Accepted preparation transport checkpoint

Source `5f26e83` adds explicit worker HTTP methods for the daemon's separate
proposal and accepted-preparation routes. Opaque process-local observations bind
actual response bytes to the original offer, node/boot, exact operation and delivery
nonce. Historical proposal clocks never create a fresh local window. First Accept
must be sent inside the original observed window; its bounded preparation lease
can outlast that window. Exact replay requires a still-live accepted observation,
identical request/proposal digests and an equal-or-shorter local deadline.

The adapter uses the existing two-slot control lane, finite HTTP timeout, disabled
redirects/retries and the frozen protocol's131072-byte request/response bound.
Cancellation frees the local HTTP slot without asserting remote rollback. No
legacy Preparation is fabricated. Callers still need an actual reservation owner
and fsynced Accept intent before invoking it; durable phase journal, input pipeline
adoption, new execution issuance and cleanup remain separate implementation work.

The exact qualified0.1.9 archive and103 extracted files were independently verified;
normalized manifests and package sources were not edited, and the prior0.1.7
archive remains unchanged. Six focused real-loopback tests pass, covering phase
separation, replay clamping after proposal expiry, wrong nonce/operation, bounded
or malformed responses, scope/negotiation refusal, delayed proposals and cancellation.
The full all-feature worker package passes170 tests with five explicit ignores in
8.78 seconds of summed test time. Two separately enabled actual-Git legacy
hydration/grant regressions pass in4.23 seconds. Strict all-target/all-feature lint
and formatting pass. [Exact source pins and logs](WRK-08-preparation-v2-http.json)
retain the initial zero-test Git filter separately from its corrected two-test run.

This checkpoint does not qualify the new phase against an actual daemon, complete
cold preparation, start a harness/provider, or activate a native worker driver.
