# Finite prepared capsule and scoped Responses composition

Status: integration-only synthetic implementation; source review pending.
Production eligibility remains false. This branch composes the independently
accepted host candidate and neutral transport; it does not introduce a
production launch/history bridge.

## Closed scope

Use a separate integration test and pinned sealed static C peer. Preserve the
existing one-stage/static-prepared fixtures, closed prepared recipe and helper
wire. The static plan selects the new peer image and an exact fixture mode
before prepare. The resulting recipe still permits only that original prefix
and the existing origin/preparation/deadline arguments and exact environment.
This is not a Codex-compatible image or a new general harness profile.

The only integration dependency is a host **dev-dependency** on the same-
workspace `aq-responses-transport` test-support API plus existing Tokio. No
runtime host-to-transport or daemon-to-worker dependency is introduced.

Mount only fresh workspace and derived-auth directories. Do not add trusted
OS folders, allowed folders, private node roots, original-material paths or
source descriptors. The static peer cannot see parent namespaces or sockets.

## Prepare and run

1. Pin the helper/peer and exact stdin, select the original deadline, then
   prepare the isolated capsule. Verify the peer has neither executed nor
   issued a request while the trusted stub is paused.
2. Consume the returned child-netns listener into the accepted transport API.
   Use preparation digest as neutral owner scope and checked multiplication
   of its original BOOTTIME milliseconds by one million as the transport
   nanosecond deadline. Never mint a new duration or extend that deadline.
3. Supply a parent-only retained synthetic-key callback and an explicit
   numeric-loopback fake upstream. The callback records initial/per-request
   reads and can pause the second read for cancellation/expiry cases.
4. Publish only the transport's fresh local capability in `/auth/auth.json`.
   The existing publisher and stub bind/check actual auth and candidate config
   file pins. Commit the exact prepared recipe and transport scope digest.
   Assert capability digest/origin/deadline relationships in the parent owner;
   do not infer that the host independently reads transport policy metadata.
5. Static guest authenticates one bounded `POST /v1/responses` to the isolated
   listener with its derived capability. It parses only the finite expected
   HTTP/chunked SSE subset, records a bounded success marker, and exits. No
   unrelated request, tool or external provider exists in this fixture.
6. Fake upstream verifies exactly one request with the distinct parent-only
   synthetic key, selected model/reasoning and injected exact output cap,
   absence of local capability bytes, then returns finite SSE.

## Aggregate ownership and refusal cases

The integration owner holds either prepared or running physical ownership and
the serving transport. Explicit async shutdown triggers transport cancellation
and **awaits completed task closure first**, then aborts/stops the capsule and
requires the appropriate actual ECHILD receipt before releasing fixture roots.
A normal guest exit is not itself the ECHILD receipt; its namespace init and
descendants still belong to the outer guardian.

An additive candidate-only `commit_retaining` consumes its prepared handle
and returns either the existing running handle or an opaque noncloneable
`CandidateCommitFailure` that still owns the physical child and original image.
It exposes only an error reference; it has no retry, abort, stopped-receipt or
secret-bearing Debug API. The legacy `commit` method delegates and drops a
failure, preserving its old behavior. The aggregate retains a failed owner
until transport stop has joined, then drops it through the accepted bounded
cleanup/reaper path without manufacturing a physical proof. Ordering concerns
parent-initiated cleanup through its owner; a child may independently exit or
its guardian may independently refuse earlier.

Drop cannot await. It synchronously denies transport forwarding, drops its
task owner (abort requested), then drops the physical owner into its existing
bounded wait/reaper path. Drop produces neither a completed transport-closure
claim nor a stopped proof. Cancellation of the async shutdown future retains
the remaining physical owner until normal bounded Drop cleanup. An awaited
transport stop may record a serving error such as expiry, but its serving task
has joined before physical cleanup proceeds; that error does not create any
execution or stopped authority. Deliberately uncertain paths retain the tiny
synthetic private roots instead of removing them under an implicit receipt.

Negative vectors use wrong model, wrong derived capability, original expiry,
and cancellation while the second retained read is paused. Wrong selection or
capability must produce zero upstream requests. For paused reads, cancellation
or deadline closes async serving; release and drain the cooperative synthetic
callback before fixture teardown. Started blocking reads may outlive async
cancellation and are never claimed forcibly canceled. No later callback result
may cause a request after the serving fence closes.

The finite peer records a request-attempt marker separately from its success
marker, so refusal is distinguishable from a guest that never tried. Root
deadline expiry can kill the guest; it must not be reported as a successful
response. Use the original bounded lifetime for all stages.

Independent source review and local static execution are separate evidence.
No real SourcePin, provider credential, Codex, tmux, production permit/history,
recovery, output accounting or packaging qualification follows from this test.

## Implemented bounded cases

The separate static peer accepts only five fixed fixture modes and the exact
existing prepared recipe. After the real running-image witness, a fresh
workspace marker permits its one request; no request can occur while the
trusted stub is paused. Its response parser supports only bounded HTTP 200,
chunked framing and the one finite expected SSE body. It writes separate
ready, request-attempt, response-complete/refused and error-line markers.
It receives no original key, source path or descriptor, additional environment
or filesystem grant. The fake upstream is a parent numeric-loopback listener.

Five explicit integration tests exercise six executed guest sessions plus a paused
commit failure: successful finite SSE, wrong model, wrong local capability,
complete request held in retained callback then cancellation, original expiry
in the same stage, aggregate Drop with late callback release, and retaining
commit failure while the stub is still physically live. They assert zero
upstream requests for all negative cases; the positive checks one actual
upstream request, distinct parent key, selected cap/model/reasoning, exact
finite SSE and actual ECHILD. Drop/failure tests check actual init/stub pidfd
exit but return no ECHILD receipt and retain private roots. They do not claim
forced blocking-read cancellation or general async Drop completion.

Source `7827d4f` passed the five composed cases in 4.31 seconds, standalone
capsule cases (11) in 8.38 seconds, host units (36 passed, three explicit
fixtures ignored) in 0.18 seconds, and transport units (15) in 1.07 seconds.
Strict all-target/all-feature Clippy and default host binaries passed. The
normal host dependency graph contains neither transport nor Tokio; these are
dev-dependencies only. The qualified transport source is byte-unchanged.
See [the qualification record](evidence/prepared-responses-composition/qualification.json)
for commands, source hashes and remaining gates. Independent source review
has not yet accepted this composed increment.
