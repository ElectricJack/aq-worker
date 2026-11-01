# Frozen native smoke probes: cold preparation remains open

Worker source `4eef2949c366013929eb196fefb23078012417a4` produced the exact
feature-gated smoke binaries in the [manifest](WRK-08-native-prepare-smoke-evidence.json):

- debug `f476e42e681768138db8d69e35bffa76443e4fe567afcaa2dc50eba73f498ab6`;
- optimized release `bd1d24b49469a4b997b64fcb730a7fb967c3dc1112eb3e44075be70c37679bb7`.

Both use unchanged code and authority lifetimes. Release uses the workspace's
thin LTO and stripping. Two framing/private-scope unit tests, four debug and
three release negative-process redaction cases, feature-gate metadata, strict
binary Clippy, formatting and source diff checks pass. Twenty frozen source
hashes and fourteen raw logs/reports are retained. Raw cargo logs preserve their
original trailing blank lines.

**Neither binary achieved initial Grant against the actual daemon.** Debug
probes refused during artifact collection or at Poll before any local attempt.
The optimized release probe completed all 36 original artifact receipts and
received material and source status/manifest/bundle responses with HTTP 200,
then refused during hydration. Its original immutable preparation window was
1,328 ms; measured Poll and Accept handling took 471 and 67 ms. The reported
server clock was 43 ms beyond original preparation expiry at refusal. These are
single local probe observations, not throughput or latency guarantees.

The failure is retained as negative cross-component qualification. No TTL was
widened, snapshot retimed, old decision refreshed or fallback used. Optimized
inline batching can reduce the observed 72 per-artifact plan/completion journal
publications, but a tiny fast-path pass would not establish a general cold window.
The [phase-separation proposal](WRK-08-preparation-phase-separation.md) describes
a separately admitted bounded preparation lease and strict fresh Grant facts.
It enables no authority and preserves existing published behavior.

The actual-daemon test diagnostics were evolving during these probes. The
manifest records their base revision and exact current fixture file snapshot,
explicitly not a frozen daemon publication. Final daemon integration evidence
belongs to its owning checkpoint. Runtime/harness/inventory qualification
remains synthetic; provider, host launch, grant persistence and complete recovery
are not qualified. Both frozen binaries and negative logs remain separate from
any later batching or phase-authority implementation.
