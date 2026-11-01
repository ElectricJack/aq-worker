# Finite prepared capsule and scoped Responses composition

Status: integration-only synthetic source plan. Production eligibility remains
false. This branch composes the independently accepted host candidate and
neutral transport; it does not introduce a production launch/history bridge.

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

Drop cannot await. It synchronously denies transport forwarding, drops its
task owner (abort requested), then drops the physical owner into its existing
bounded wait/reaper path. Drop produces neither a completed transport-closure
claim nor a stopped proof. Exceptional async shutdown failure retains the
remaining physical owner until normal bounded Drop cleanup; it does not turn
an error into release authority.

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
