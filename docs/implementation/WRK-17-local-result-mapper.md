# Local-result mapper and client installation candidate

This is an opt-in recipe and image installation increment for the explicit worker
0.5 local-result candidate. The qualified worker 0.4 / FORMAT11 source remains
unchanged. Native result history/FORMAT12 and the non-Serde actual channel owner
are separate, Human-owned integration work; mapping and image metadata grant no
execution, result, graph or provider authority.

`aq-codex-mapper` 0.1.1 adds `map_with_result_channel(Settings, Paths,
ResultChannel { root, client })`. The existing `map` implementation and its base
and scoped worker byte goldens remain unchanged. The new recipe:

- declares `AQ_AGENT_RESULT_DIR=root` and `AQ_AGENT_RESULT_COMMAND=client` in both
  the top-level environment and the explicit tool shell environment;
- retains `inherit="none"`, fixed `/usr/bin:/bin`, workspace-write sandboxing,
  disabled network and all previous closed Codex settings;
- replaces the empty writable-root list with exactly `root/submissions`, leaving
  the binding/presence/owner lock/worker acknowledgment parent read-only;
- preserves the original sealed stdin marker and adds no instructions or startup
  bytes. Completion instructions must already be in centrally counted startup.

Paths are metadata only. Lexical bounds refuse result roots overlapping the
workspace, private Codex home or executable, and client paths overlapping the
workspace, private home or result root. The actual native owner must separately
verify retained root/submission FD identity, canonical routes and original grant,
expected outcome, execution binding and deadline. No arbitrary external-folder
interface is added. The existing PermissionPlan Scope
`{operation:"result.submit", resource:"local-v1"}` and exact result contract
module must be admitted and bound with the recipe/client pins before launch;
these are not new required capabilities.

`InstalledResultClient` retains one separately pinned named-readonly client file
in a dedicated 64 MiB pool using the same real `NamedReadonlyEnvironment` owner.
It verifies the exact local-result contract artifact, independent installation
manifest/client/environment references, digest and actual byte count, readonly
image/path/namespace identity. It exposes metadata and O(1) revalidation only.
Its extra 64 MiB image reservation is explicit and additional to the Codex pool;
it is neither a free image nor a borrowed Codex slot. No process is launched by
installation and no source-material FD is exposed.

The pure result contract fixes `binding.json`, `presence.json`, `owner.lock` and
`ack.json` in the control root and immutable `submissions/submission.json` in the
writable child. The client reads the result root from the environment and supports
`result submit --request-file FILE` and `result status`. The conclusion remains an
agent-authored claim. Worker acknowledgment/real result reporting and all stop,
source and receipt checks remain owned by the native execution owner.

No Graph/node/provider credential is introduced into this channel. It sits outside
captured source, so no secret metadata exclusion or special `.aq` ignore rule is
assumed. This candidate does not claim actual client/tool execution, a result
receipt, source-retention closure, or production completion until the combined
native/client/mapper path is separately checked and authorized.

## Central opt-in and the actual owner join

The versioned `aq.codex-result-local-adapter@1` Definition is selected as the
admitted `LaunchManifest.harness.adapter`. Its canonical bytes bind the exact
client-installation artifact, client executable, environment, local contract,
recipe definition, runtime, renderer, permissions digest and additional 64 MiB
client image reservation. The recipe artifact is
`aq.codex-result-local-recipe@1`; the client installation artifact is
`aq.agent-result-client-installation@1`. These are code/installation definitions,
not profile/context modules. They are not placed in `manifest.modules` as a
substitute for admission. Reporting instructions are a separate centrally counted
startup context module.

The composite body deliberately does not embed the NamedExecManifest digest:
that manifest already contains the selected adapter reference, so embedding its
digest would create a content-address cycle. Its runtime reference is checked
against independent selection. Exact installed runtime qualification versus the
actual reservation/grant remains the existing native owner check; this body does
not supply that proof. A private operator file alone does not opt an offer in.
The actual retained execution must include this exact Definition-role artifact,
original descriptor and bytes selected by its manifest before a channel is made.

Pure definition validators enforce canonical bytes and closed fields, independent
selection, original retained descriptor/role/body, the installed client's pins,
and required/effective `result.submit/local-v1` permission. Their outputs remain
metadata. Human's native result owner constructs the non-Serde
`VerifiedResultChannel` only from the actual pipeline/execution/grant and retained
root/submission FDs, then freezes its adapter/client/recipe/permissions/root pins
before any launch. The adapter's future borrowed input will carry actual Entry
binding/execution-digest plus the real client/channel owners; it cannot activate
from a disk envelope or these validators succeeding.
