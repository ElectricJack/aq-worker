# Native runner: foreground orchestration boundary

The worker now has a production-compiled `run-native --native-config <file>`
command, a reusable `native::runner::run_once` orchestrator and a concrete closed
Codex executor joined to the native host owner. Missing executor settings refuse
before admission. Production admission also explicitly refuses because effective
`ModelSettings.max_output_tokens` enforcement has not been qualified for the
pinned CLI. A binary hash or independent qualification reference cannot supply
that missing behavior. The concrete factory is available only through explicit
test-support loopback construction for isolated no-provider qualification. This
increment does not claim real-provider eligibility.

`NativeConfig::load` reads at most 64 KiB of owner-0600 canonical JSON, rejects
unknown fields, and verifies private configuration identity/mode/size. It contains
operator-selected runtime, qualification, renderer, adapter/manifest pins, source
profile, phase negotiation, logical capacity and installed inventory definition.
Enrollment credentials stay in the existing worker configuration. No task supplies
argv, environment, executable paths, provider endpoint or credentials here.

The first runtime is intentionally one fresh attempt with one logical slot. The
state root is owner-0700, locked for the invocation, and must have no existing
journal/material children. Existing roots require stop/preserve and historical
reconciliation; no code silently initializes or adopts an old live boot.
`run_once` first calls the trusted executor's `preflight(&NativeConfig)`. That
implementation must verify its concrete installed profile and independently
selected qualifications. The old generic 256 MiB installed-file helper remains
available explicitly; it is not a mandatory gate for a separately qualified
native-exec profile with different documented bounds.

After successful executor preflight, orchestration reuses enrolled heartbeats,
creates the original journal/material owners, registers its actual boot/runtime,
and publishes an actual pipeline-owned qualified InventoryV2. It polls at most one
offer, reserves under the original local deadline, persists phase Accept intent,
collects actual artifacts/private materials/Git workspace, publishes Prepared and
requests execution. There is no automatic mutation retry, phase renewal, resource
release, task settlement or arbitrary offer-to-command conversion.

## Durable ownership handoffs

`InputPipeline::reserve` performs the journal reservation and process-local
retained-membership handoff in one admitted storage job. The resource-map lock is
acquired before durable publication. The retained-input membership insert itself
is infallible after a successful journal commit. Launch-registry membership is
then installed in the same storage job; a registry refusal retains the durable
reservation and input claims while refusing launch. Cancelling the async waiter
does not cancel fsync or split these operations.
A persistence error leaves the journal poisoned. A published post-rename record is
retained on disk for recovery and cannot become fresh polling authority.

`InputPipeline::request_phase_execution` retains actual Prepared owners
synchronously, before its returned future can perform HTTP. A transport error or
lost Grant response therefore keeps the workspace/material owners and original
Prepared history; it does not create an execution owner or launch. No error clears
claims or manufactures a successful server disposition.

The executor interface consumes the actual durable Grant and pipeline plus a
shutdown receiver. Its implementation owns actual launch, readiness, output,
physical stop and exact Started/Stopped acknowledgment publication. The runner
never cancels/drops the executing future when shutdown arrives; it requests stop
through the signal and relies on the existing guardian's original caps. Fatal
enrollment-heartbeat refusal also requests stop. Before returning AttemptStopped,
the runner checks retained physical-stop history and both paired acknowledgment
receipts. It does not infer task completion from that result.

The native adapter and runner now join through `NativeCodexHostProfile` and
`launch_phase_native`, with a preflight-sealed immutable image, exact mapper
settings and original Grant/input validation. `NativeConfig.executor` is an
optional closed tagged configuration (`kind = codex-0.166-exec-stdin-v1`) carrying
operator harness/model/permission pins and private host geometry/root. Production
transport requires HTTPS and currently refuses Cloudflare proxy credentials.
`--development-loopback` is reserved in test-support CLI builds and explicitly
refuses before reading enrollment/native configuration: a numeric Node origin
cannot isolate provider traffic. The ordinary CLI requires a future explicit
scoped-provider join; the actual-process tests provide their owned loopback
transport to the same executor/driver through a test-only constructor.

The output publisher is independent of host control. A missing human Watch (409)
and output network errors preserve exact pending output bytes with bounded
backoff, while the local drain continues. Dropping the publisher on stop aborts
network waiting. Shutdown/original expiry can also interrupt a delayed Started
HTTP wait; the immutable publication remains unknown and cleanup still requires
actual physical stop. No Started ACK is manufactured and no execution/control
operation uses the output retry policy.

The first physical lifecycle checks use an actual pinned tmux/ELF host and
synthetic typed control/relay peers. The next composition uses actual pinned
Codex 0.166 under the same executor and real tmux, with a finite loopback provider
and an attempt-owned scoped Responses transport. Real daemon-owner and production
runtime/model/tokenizer qualification remain separate. An unstarted/unknown native launch
requests physical cleanup and preserves closure history; its separate no-Started
publication owner remains required.

The output producer also uses a smaller local frame ceiling within the negotiated
policy, and a 256 KiB total encoded packet ceiling matching the actual HTTP
transport. Worst-case JSON byte-array expansion cannot permanently wedge a
64 KiB high-byte frame. The sealed terminal policy itself remains unchanged.

## Verification

The fixtures route model inference to local peers. Follow-up tracing found
implicit curated-plugin Git traffic and operator-profile reads by the CLI, so
those runs do not qualify no-egress or operator-context isolation. Actual launches
are paused pending hardened mapping and independently confined qualification.

- Three reserve tests cover exact membership/capacity, duplicate refusal,
  post-rename recovery and cancellation at the actual fsync barrier.
- Three runner/config tests cover unavailable-executor refusal before effects,
  private config symlink/mode/size refusal, unsupported capacity and changed pins.
- An actual HTTP/private material/Git-peer preparation test closes the HTTP peer
  before Prepare and proves actual Prepared owners survive the unknown response
  without journal mutation, execution owner or host launch.
- Strict all-target/all-feature lint and a default-production binary check cover
  both compiled feature profiles. The new command is compiled without test-support.

Initial failed test attempts were fixture errors: changing a frozen offer's boot
invalidated its bound contract (fixed with the existing journal test-constructor
pattern), and writing a deliberately read-only fixture required explicit chmod.
No owner rule, deadline or filesystem permission constraint was relaxed.

## Joined candidate checks

- Seven runner/preflight cases include explicit refusal of production admission
  with an unqualified output limit, invalid transport/settings and legacy manifest.
- Three pinned physical lifecycle cases pass: an unwatched 409 keeps the host live
  with exact pending bytes; a blocked output upload cannot delay stop/ACKs; a
  delayed Started reply preserves its unknown receipt while actual ECHILD stop
  and the synthetic peer's stop receipt are retained (16.80 seconds total).
- Dense high-byte output is tested through the actual RelayHttp client against
  the local typed HTTP peer, alongside prefix/eviction/replay cases.
- Strict all-feature and default-production profiles are checked separately.
  Compilation uses the new host 0.1.5 / bridge 0.1.2 / worker 0.3.4 candidate package pins.

The physical cases do not fabricate a LocalPhaseHost or infer a stop from a JSON
record. Their synthetic coordinator is explicitly an HTTP fault peer, not an
actual daemon owner qualification. The final composed owner/runtime check remains
separate. The first dense-output followup exposed an outdated expected frame count
and a test fixture still installing its old 4-byte framing/seeded-prefix expectations; those fixtures were
corrected without changing the production byte ceilings or transport policy.

## Actual Codex loopback composition

Three actual-process cases pass against the same `CodexExecutor::execute` path:
finite successful Responses SSE and zero process status, shutdown during blocked
output, and shutdown during delayed Started acknowledgment. Each requires actual
physical `ECHILD`; paired receipts are retained for successful Started, while the
delayed case retains its unknown Started receipt. The output peer's unwatched 409
does not terminate the actual running CLI. The tests retain original Grant deadlines
and actual preparation/material/workspace owners throughout cleanup.

The tests use a preflight-sealed 319MiB image, separately pinned helper and tmux,
private scoped synthetic local capability, and distinct upstream synthetic key.
The peer verifies the sealed model/reasoning and exact injected response cap, and
asserts that neither key nor the Node credential enters model input. Hosted web
search is explicitly disabled and absent in the observed request. The actual CLI
adds developer context, environment context and local tool definitions; those are
recorded separately from the exact 26-byte stdin payload. See the
[loopback process evidence](evidence/wrk18-codex-loopback/README.md).

This composition found and fixed a real owner join error: preparation consumes
the startup payload artifact into `RenderedStartup`, so launch now borrows verified
bytes from that retained owner. It also exposed the finite Git fixture's missing
repository and the CLI's implicit environment message. No sealed startup assertion,
material/provider binding, physical proof or production admission gate was relaxed.
The default unit profile excludes loopback helpers whose dependency requires the
explicit test-support feature.

The controlled tool followup produced an actual stateless function-call/output
round trip but failed unified-process creation with ENOENT. No successful tool
execution is claimed. Its separate diagnostic trace revealed the isolation gaps
above, and only a startup probe of the nonexistent memfd self-path; that probe is
not yet established as the exact failed tool executable. See the recorded
[negative qualification observation](evidence/wrk18-codex-loopback/tool-failure-observation.json).
