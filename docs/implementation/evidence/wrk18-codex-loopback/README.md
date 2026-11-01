# Codex 0.166 loopback process observation

This is finite local process evidence from 2026-11-01, not production runtime,
model, tokenizer, resource, renderer, daemon-owner or credential qualification.
Model inference targeted the local fake provider. **Follow-up tracing revealed
implicit external plugin Git traffic and operator-profile reads**, so these runs
do not establish no-egress or operator-context isolation. See
[negative followup evidence](tool-failure-observation.json). No paid provider
inference was requested. The admission/material/Git socket
and output peers are explicit synthetic control peers. The fixture workspace is
an actual initialized Git repository; actual aq-git service composition is separate.

The test uses the real `CodexExecutor::execute`, native installed-image profile,
physical tmux guardian and shared output/stop driver. The exact startup bytes
arrive through sealed stdin. The CLI reads a scoped synthetic local capability
from private owner-0600 `auth.json`; the attempt-owned Responses transport forwards
only to a numeric-loopback fake provider using a distinct synthetic upstream key.
The peer verifies model/reasoning selection and injected `max_output_tokens=4096`.
None of the local capability, upstream key or Node credential appears in model
input. Actual observed tools contain seven local functions and no hosted web search.

A finite successful Responses SSE exchange reaches an actual zero process status,
physical `ECHILD`, and paired Started/Stopped acknowledgments. Independent output
continues while its synthetic peer refuses uploads with an unwatched 409. Separate
actual-process cases stop during blocked output and delayed Started HTTP; the
latter retains the unknown Started receipt and exact physical stop history.

The three JSON files separate the exact observed implicit messages and tool
definitions from the request summary. They include only fixture data. They show
that the CLI adds substantial context: two developer messages, an environment user
message and seven tool definitions accompany the exact 26-byte sealed user payload.
These implicit bytes require real tokenizer/context/model qualification before
production admission; the request-cap observation does not supply that qualification.

Observed pins:

- Codex 0.166.0 executable: 319,346,888 bytes,
  `b8a11fedf24b2dfb59a70c9e52b030f9a508dcd49cd2dd0d41956f4c785156b8`.
- Rebuilt host 0.1.5 helper:
  `0403416f7e3ba411afad07bb4436dcb686920e73dfc6f91487569d9c5d30f6db`.
- System tmux 3.4:
  `034b15c64035f783d43862f2775eb4828f61571ca62c8199796000b97d556ecd`.

Source lineage includes the runner join `4dcf46f` / `b55ae73`, adapter endpoint
`ab920ad`, retained-startup fix `86de4f2`, scoped transport `3fd54a1`, stateless
boundary `9124ea0`, and access-program refusal `f0dd3ce`. Packaging candidates remain
host 0.1.5 / bridge 0.1.2 / worker 0.3.4; a publication review remains root-owned.
Production `CodexExecutor::new` still refuses unqualified admission.

## Tool and isolation followup

A controlled local `exec_command` call reached the CLI and returned its real
`function_call_output` in a second stateless, capped provider request, but process
creation returned ENOENT. No local tool success is claimed. A subsequent
path/syscall-only trace timed out before its first provider request, so its
nonexistent memfd self-path probe cannot yet be identified as that failed tool
execution. The same trace directly observed login shell snapshots opening the
operator profile and curated-plugin Git operations targeting GitHub. These
unexpected background actions invalidate the initial no-egress assumption.
Operator profile or credential contents were not printed or inspected.

Further actual launches were paused. The adapter added explicit shell-snapshot,
login-shell, plugin/app and memory disables (`cc6ac7c`, `f1e6044`), and the worker
CLI now refuses the ordinary test-support loopback flag before configuration or
state effects (`7bb2179`). A fresh user/network namespace with loopback up was
probed successfully with bwrap; actual requalification must additionally mask
operator home directories and preserve the admitted helper-image identity.
This test-only confinement supplies no production isolation qualification.

The checked-in [trace excerpt](followup-paths.observed.txt) contains only selected
process/file metadata, not file contents. `git ls-remote` exited 0; the later fetch
was interrupted during fixture cleanup. The exact tool executable responsible for
ENOENT remains unestablished.

The test-only `scripts/qualify-native-loopback.sh --probe` now verifies loopback
connect/accept, rejects a numeric TEST-NET connection with no default route, and
independently masks `/home`, `/root` and host `/var/lib`. It creates owner-0700
fixture state under private `/var/lib/aq-qualification`; the child still receives
its original cleared environment, so private CODEX_HOME lies outside its default
`/tmp` helper-alias restriction. Actual mode binds only explicit test/helper/Codex/
tmux inputs at fixed readonly paths. The Rust kernel-premise probe also passes inside this wrapper, while a spoofed
environment marker is rejected outside it before any CLI launch. Every actual CLI
fixture checks these premises as its first operation. Actual harness execution
remains paused pending reviewed helper-image assumptions.

## Hardened process and image view

The wrapper has since replaced the whole-host-root bind with a private PID/UTS
namespace and an explicit trusted OS-image allowlist. `/etc` contains only literal
private passwd/group/NSS/hosts/timezone files and namespace-local links. Kernel
probes prove the outside parent's `/proc/<pid>/root` and `/fd` aliases absent,
PID1's root contains the private image, and accessible provisioning directory FDs
refer only to visible mounts. Private homes, `/tmp` and host service state remain
masked. The guard requires these actual premises; its environment marker alone
cannot establish them. No further actual Codex execution has run yet.

This does not hide the fake worker's material source from its own harness inside
the fixture. Raw upstream source/proc visibility remains an explicit production
blocker; the fixture is an operator-host isolation boundary, not a claim of
production per-harness secret separation.
