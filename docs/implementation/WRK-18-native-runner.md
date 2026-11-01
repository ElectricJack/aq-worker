# Native runner: foreground orchestration boundary

The worker now has a production-compiled `run-native --native-config <file>`
command and a reusable `native::runner::run_once` orchestrator. This increment
creates the command, configured preflight and actual phase preparation path. The
ordinary command deliberately uses `UnavailableExecutor`: it refuses before
registration, polling, inventory or journal publication until a qualified concrete
host executor is installed. It does not run a model or promote the synthetic host.

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
acquired before durable publication and no fallible operation follows a successful
commit. Cancelling the async waiter does not cancel fsync or split the handoff.
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

The concrete executor/factory join and real-runtime qualification remain WRK-08.
No positive full orchestration/real harness claim is made by this increment.

## Verification

The isolated component uses pinned synthetic fixtures and local files; no provider
requests or existing developer/production state are involved.

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
