# WRK-17 trusted-node Codex execution pilot

This increment selects one supported Codex 0.166 executor explicitly. It reuses
worker 0.4.0 / journal FORMAT11 and the existing native host, scoped Responses
transport, original retained material, and Frozen/Refresh terminal owners. It
adds no service, dependency, capsule requirement, migration, or new wire contract.

## Operator interface

`executor.kind = "codex-0.166-trusted-node-named-readonly-v1"` requires `settings`
(the existing closed `CodexSettings`) and an independently selected `environment`
artifact reference. The selected harness must carry that exact environment.
The installed manifest remains separately tagged
`codex-0.166-named-readonly-exec-stdin-v1`; its canonical digest and adapter,
runtime, qualification, renderer and environment references must match operator
configuration exactly. No boolean can qualify the environment or installation.

Construction of the HTTPS relay is not admission. Preflight inspects the actual
`NamedReadonlyEnvironment` (all capability sets empty, no-new-privileges and
retained namespace identities), then opens a bounded named-readonly pool and
installation. The installation verifies actual executable bytes, retained readonly
file/mount/path identity and pinned helper/tmux auxiliaries. External provisioning
qualification must still establish immutable image topology, no outside writable
aliases and controlled namespace transitions. An artifact reference alone does
not establish those premises.

The old `codex-0.166-exec-stdin-v1` executor remains refused before manifest I/O,
journal/state admission, registration or polling. Its sealed-memfd image policy is
unchanged. It is not promoted to Codex self-helper/tool compatibility.

## Runtime boundary

The host uses the existing named image and actual tmux/guardian owners. Launch
rechecks original retained `InstalledMaterial`/SourcePin bytes and exact launch
bindings before deriving private child auth. A fresh local capability is placed
in the child auth home; the original provider key is used by the owned parent
scoped transport. That transport validates exact model/reasoning/settings and
injects the sealed per-response `max_output_tokens`. Physical stop, durable stop
recording and the selected nominal Started/Stopped receipt pair remain required.
Output/Started failure cannot skip real stop. Frozen and Refresh terminal records
are selected from the genuine retained execution and Running context, not from an
untrusted tag or DTO-to-authority conversion.

The closed pilot supports OpenAI Responses, Codex 0.166, low/medium/high reasoning,
16 through 1,048,576 output tokens per response and the existing five mandatory
capabilities. It refuses every aggregate spending budget, unsupported capability,
Cloudflare relay credential configuration, and mutable/legacy installation
fallback. The output cap is not an aggregate token or monetary budget.

The existing mapper retains workspace-write sandboxing, explicit fixed tool
`PATH=/usr/bin:/bin`, `shell_environment_policy.inherit="none"`, disabled login
shells/snapshots/plugins/apps/memory/web search, private CODEX_HOME, exact startup
stdin and the owned local Responses endpoint. Named host state must be outside
CLI temporary `/tmp`. Provisioning must supply the trusted readonly OS image,
including Git and the Codex sandbox helper dependency `/usr/bin/bwrap`. Previously
qualified bwrap was 0.9.0, 72,160 bytes, SHA256
`e318903862396f96de3df57264e0158682b952fd3fb53ac23d876413e7b30f71`.
The NamedExecManifest pins Codex/host/tmux; bwrap is an OS-environment provisioning
dependency, not a separately verified manifest pin.

This is a trusted-node profile with settings-based workspace confinement. It does
not promise hostile-process isolation or that same-UID processes cannot inspect
worker source material through filesystem/proc aliases. Per-harness capsules and
independent egress confinement remain separate profiles and are not silently
claimed here.

## Qualification and remaining closure

Source tests cover closed selection, early legacy refusal, independent environment
mismatch, per-response cap bounds, aggregate spending refusal and explicit test-only
numeric-loopback routing. Test-support permits a finite loopback upstream override
before ordinary preflight; it still opens the actual named installation and uses
the same scoped transport. Production always uses the fixed HTTPS provider route.
No source test or constructor claims provider behavior or live credentials were
qualified. Actual named CLI/real AQ task/finite peer composition requires separate
root-authorized execution and exact runtime, helper, image and environment pins.

A successful Codex process/terminal lifecycle is not by itself task completion.
The v1 identity/bootstrap checklist includes task/session/project/profile scope and
an authenticated AQ CLI endpoint. This increment does not invent that agent-client
credential/bootstrap join, result settlement, or graph/messaging integration.
Those joins must use the existing v2 authenticated agent-client contracts, rather
than passing a daemon/operator credential through the child environment.

## Pinned v1 reuse audit

The v1 reference revision is
`9a9b8e8ef992d1ff699755a9383ed3c91e2844ea` in `agent-queue2`, following the v1
implementation reference map. Its useful source boundaries are:

- `src/sessions/default_harnesses/codex.md`: interactive Codex configuration,
  real resume-ID caution, startup/login/trust classifications, terminal quirks.
- `src/sessions/spec.py`: immutable resolved launch specification; model and
  reasoning selected from intelligence profiles; explicit argv ordering and
  separate capability/permission policy.
- `src/sessions/env.py`: task/session/project/profile identity, authenticated
  AQ endpoint bootstrap and protected daemon-only credentials.
- `src/sessions/tmux.py`: explicit env clearing, fixed PATH, dedicated tmux
  socket, startup stderr and ready-vs-dead distinction.
- `src/sessions/provider.py`: separate pane presence, process liveness and
  unknown physical-stop state.

V2 already has the corresponding immutable profile/settings join, env clearing,
fixed PATH, sealed startup stdin, tmux/guardian ownership and stronger positive
physical-stop proof. The remaining factory gap was selection of the actual named
installation and owned scoped transport in the production runner; this increment
addresses that gap. The old unconditional output-bound refusal predates the
qualified scoped cap and real named tool fixture, but removing it alone would
incorrectly select the memfd image.

V1 interactive prompt prefixes and 3-second ready delays are not readiness proof
for the noninteractive v2 exec profile. Its optional dangerous bypass flags, hook
trust bypasses, inherited authentication and older model names are not reused.
Transcript/session-ID/usage parsing can supply versioned fixture examples for
later observability, but is not process authority or exact task completion.
