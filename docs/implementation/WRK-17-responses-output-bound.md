# Responses output cap: contract finding and scoped transport candidate

Status: bounded transport implementation, local peer tests and an initial actual
pinned-CLI request through the loopback transport. Production native execution
remains ineligible. Final source composition and upstream model/provider semantics
are separate qualification boundaries.

## Meaning of the frozen fields

The frozen `ModelSettings.max_output_tokens` is a selected output limit. It is
not a bookkeeping-only reservation: `ContextPlan.reserved_output_tokens` is the
separate reservation, and session validation requires the selected limit to fit
inside it. The selector requires installed native output-limit support at least
as large as the policy's requested cap and copies that exact requested value
into the concrete selection. Architecture section 3 explicitly refuses adapters
that cannot enforce a required hard limit.

For this Responses adapter, enforcement applies to **each provider response**,
including each continuation after a tool call. It does not cap the sum over a
multi-response agent run or a task lineage. Aggregate spending is a separate
frozen budget, and this candidate refuses any nonempty spending budget. There
is no silent conversion to an aggregate allowance or estimated telemetry stop.

References in the source tree:

- `aq-session-protocol/src/lib.rs`: ModelSettings, ContextPlan and manifest
  output-budget validation.
- agent-q `aq-selection/src/select.rs`: installed NativeLimits compatibility
  and exact ConcreteChoice output cap.
- agent-q `docs/architecture/0003-agent-profiles-harnesses-and-terminals.md`:
  required native limits and separate observed usage/estimated cost.

The [OpenAI Responses create reference](https://developers.openai.com/api/reference/python/resources/responses/methods/create)
assigns `max_output_tokens` to one response and includes visible and reasoning
tokens. Its minimum is 16. This adapter therefore refuses selected values below
16 instead of increasing them. [OpenAI token-count guidance](https://developers.openai.com/api/docs/guides/token-counting)
also explains that generated output limits include tokens not visible in the
returned text. The HTTP boundary passes the actual parameter upstream; it does
not estimate enforcement from terminal byte counts.

## Why configuration or a model catalog does not solve this version

Exact upstream Codex `rust-v0.166.0` sources were inspected. The config schema has
no model max-output setting; `ModelInfo` has context/compaction metadata but no
output-limit field, and `ResponsesApiRequest` plus its builder has no such member.
An arbitrary TOML override or catalog JSON property does not establish behavior.
The current mapper's exact ModelSettings comparison establishes selection only.

Exact-version primary source paths:

- `codex-rs/core/config.schema.json`
- `codex-rs/protocol/src/openai_models.rs`, `ModelInfo`
- `codex-rs/codex-api/src/common.rs`, `ResponsesApiRequest`
- `codex-rs/core/src/client.rs`, request construction

The [official configuration reference](https://learn.chatgpt.com/docs/config-file/config-reference)
provides configuration and catalog entry points. The [catalog guidance](https://learn.chatgpt.com/docs/model-catalogs)
requires matching the actual client's schema; supplying a catalog does not add
an unsupported request parameter. A model's independently qualified native
maximum could satisfy a requested ceiling at least that large, but no such
model/provider guarantee has been qualified for this candidate. It cannot
satisfy an arbitrary smaller selected cap.

## Candidate owner and byte boundary

`adapter::responses_transport` is self-contained inside aq-worker. It requires
no external proxy service or new dependency. `ResponsesScope` binds one full
LaunchBinding, exact model/reasoning selection and original monotonic deadline.
Construction grants no execution authority. The lifecycle owner must derive
these fields from the retained actual grant and must stop the transport with its
attempt; serialized metadata cannot recreate that owner.

The transport binds an ephemeral IPv4 loopback listener. It requires a local
capability distinct from the upstream provider key. Production upstream routing
is a closed HTTPS OpenAI Responses URL, with redirects and ambient proxies
disabled. Only test-support permits an exact numeric-loopback upstream origin.
Secrets stay in opaque owners and are not logged, serialized, placed in argv or
copied into errors. Owner buffers are zeroized; this is not a guarantee that
reqwest's internal header/body copies are zeroized.

Only `POST /v1/responses HTTP/1.1` with a matching Host, one valid bearer token,
JSON content type and one Content-Length is accepted. Duplicate header names,
transfer/content encoding, upgrades, expectations and invalid framing refuse.
This is a closed private HTTP subset, not a general proxy. Requests are bounded
before parsing, duplicate JSON fields at every level refuse, and root fields are
limited to the pinned CLI request shape. Stateful/conversation/background routes
are unsupported. Each input requires an explicit supported kind and a full
text/local-call body; server-side item references, id-only references, provider
file/image/audio handles, additional tool declarations and hosted/remote tools
refuse. Accepted tools are local function/custom declarations. Non-null provider access
programs also refuse because this model selection qualifies no alternate program. This closed subset
is narrower than the complete Responses API. Exact model, reasoning effort, streaming and disabled storage
must match. The only semantic change is adding the exact selected output cap;
a preexisting different cap refuses. Every other accepted JSON field is retained.

The upstream response must be successful SSE. Chunks are forwarded with awaited
writes and bounded slices; there is no transcript accumulation or unbounded
producer queue. Stop or original expiry drops the serving future, including the
active upstream request and downstream socket. That local cancellation does not
prove rollback or cancellation of work already accepted by the provider. A failed partial stream closes;
it never appends a second HTTP response or replays the request.

Explicit limits: 16 KiB headers, 4 MiB request including cap insertion, 65,536
JSON nodes, depth 32, 16 MiB response stream, 64 accepted connections per owner,
one active connection, five-second header inactivity, ten-second body read and
at most 120 seconds per operation, all narrowed by the original deadline.
Node admission must account for these retained buffers and the listener in
addition to CLI/image/workspace resources. These limits can refuse a larger
valid CLI interaction; refusal does not broaden the granted policy.

## Proof so far and remaining join

Seven focused cases passed in 0.05 seconds: exact cap and unchanged other settings,
changed selection/stateful/ambiguous/oversized JSON refusals, real loopback
credential substitution plus unchanged SSE, ambiguous HTTP refusal, active
upstream cancellation, invalid endpoint/key scope refusal, and server-side references/hosted-tool refusal. Strict all-target,
all-feature worker Clippy passed. No external provider request was made.

The independent runner lane completed the actual pinned Codex -> scoped
transport -> finite loopback peer case through the same physical host/output/stop
driver and the tightened policy. The peer observed the exact injected cap of
4,096 and a distinct upstream synthetic key. The final request advertised exactly
seven local functions and no hosted web search; the mapper explicitly supplies
`web_search="disabled"`. The request body contained none of the three synthetic
credentials. The lifecycle reached genuine physical Running and matching ECHILD
with paired terminal receipts; no external provider was called.

The captured body was 31,203 bytes: two implicit developer messages contained
17,119 and 2,550 text bytes, the implicit environment message contained 1,072,
the exact sealed stdin contained 26, and tool definitions encoded to 7,295.
These are one bounded fixture's measured bytes, not token counts or universal
CLI overhead. They demonstrate why final startup counting must include the
actual implicit context/tool configuration before production eligibility.
Evidence artifacts are in `/tmp/aq-codex-process-evidence-final`; the runner lane
owns its independently pinned executable/helper and physical lifecycle evidence.

Production still needs an owner-reviewed join from original InstalledMaterial to
provider credentials held only by this transport, publication of a distinct
fresh per-attempt local capability into the private Codex home under the existing
launch intent, and stop/recovery coordination. The current private auth publisher
derives the original provider key; it must not be silently repurposed as that
capability publisher. Effective CLI HTTP transport and provider semantics must be
qualified before changing the production refusal. Context counting, tool grants,
filesystem isolation, cumulative budgets and provider eligibility remain separate
qualification requirements.
