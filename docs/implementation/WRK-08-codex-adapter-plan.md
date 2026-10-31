# First Codex adapter: pinned interface and ownership gates

The first adapter remains WRK08's Codex CLI target. Local interface inspection
found `codex-cli 0.166.0`; the [canonical nonsecret observation](evidence/wrk08-codex-cli-0166/observation.json)
records the actual native executable SHA256 and exact help/version output hashes.
It does not establish runtime, provider, model, context-counting or sandbox
qualification. No provider request or authentication file inspection was used.
The npm launcher is not the executable pin: it dynamically resolves a platform
package and starts another process.

## Exact startup delivery

Use a narrowly qualified noninteractive exec mode under the existing tmux host
as the first adapter candidate. Official [CLI documentation](https://learn.chatgpt.com/docs/developer-commands?surface=cli)
and the installed help support a stdin prompt with `-`, JSONL events, explicit
working directory, sandbox selection and ignoring user config/rules. The worker
must supply the one sealed payload through a bounded file/stdin handoff without
an additional prompt argument, interpolation, concatenation or shell command.
Do not put a potentially1MiB payload in argv. An exec-mode qualification does not
advertise interactive TUI input or live reattachment; those require separate
evidence and must be refused when required.

The wrapper must recheck the actual executable, staged payload and private
configuration before execution, preserve actual exit/stop evidence, and expose
readiness from a documented observed harness event. Merely starting a wrapper,
seeing a PID or writing a pre-exec marker is not harness readiness. Bounded event
capture must continue draining independently of slow network delivery.

## Configuration and process ownership

Official [configuration documentation](https://learn.chatgpt.com/docs/config-file/config-reference)
describes model/provider selection, reasoning settings, project-document limits,
workspace-write roots and optional temporary-directory exclusions. Derive exact
settings from the sealed manifest and current installed qualification. Unsupported
mandatory scopes/model/reasoning/resource behavior refuse instead of falling back.
Use isolated per-attempt configuration/authentication state; do not inherit the
operator's Codex configuration, session database, project hooks or credentials.
Additional project instruction loading must be explicitly included in the frozen
context plan or disabled. Native implicit instructions/tools still require actual
renderer/counting qualification; zero implicit overhead is not assumed.

The installed top-level CLI also advertises a shared background app server and
`--no-daemon`. This creates a separate ownership gate: prove that the actual
model/tool executor and every workspace writer remain owned by the guardian.
A pane containing only a frontend connected to an unrelated daemon is ineligible.
Help output alone does not prove standalone execution. No shared server takeover
or external session attachment is permitted as a substitute.

Workspace-write settings alone do not prove arbitrary external files are
unreadable or hard CPU/memory limits. Required stronger restrictions need an
actually qualified environment. The public credential definition, adapter,
environment and runtime/counting evidence must have retained canonical owner
bytes matching their ArtifactRefs; executable hashes may be referenced by those
definitions without inlining the executable.

## Implementation sequence

1. Complete native journal, original reservation/deadline preservation and actual
   source/material installation. Consume versioned control replies after freeze.
2. Add the pinned adapter/wrapper and exact config mapping, with offline refusal,
   byte-delivery, ownership, stop and event-buffer tests using synthetic peers.
3. Qualify the actual CLI's standalone process tree and observed readiness under
   tmux. Keep raw terminal events distinct from renderer snapshots/accounting.
4. Use the selected configured provider/model and bounded budget for actual AQ
   reads, confined source edits and result flow. Retain exact nonsecret evidence
   and owner definitions; only then advertise the qualified runtime capability.
