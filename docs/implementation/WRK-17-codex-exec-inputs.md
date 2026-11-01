# Codex exec input mapping — first WRK-17 increment

This production-available component maps an exact validated NativeOffer to a
closed Codex CLI 0.166.0 exec recipe. It is an input component, not an installed
executor, runtime eligibility, physical launch or provider qualification.

The mapper independently checks the retained manifest digest, selected model,
harness, permission digest, installed adapter/renderer identity and exact final
startup descriptor/payload. It revalidates the installed executable pins. The
recipe uses `--no-daemon exec`, `--ignore-user-config`, `--ignore-rules`,
`--ephemeral`, JSONL output, explicit model/workspace and a single stdin `-`.
Derived overrides disable project instruction loading, mark project config
untrusted, remove additional write roots, exclude temporary write roots, and
remove shell environment/profile inheritance. The supplied CODEX_HOME must be
canonical, private and initially empty. This component does not create that home
or install auth files; that belongs to the eventual stopped-writer-aware private
configuration owner.

The startup owner has no Serialize/Debug and is consumed by a single exact-byte
write. It cannot automatically retry an uncertain partial stdin write. API-key
auth derivation reads the existing borrowed InstalledMaterial and checks its
actual provider/credential/launch association, returning zeroizing private JSON
bytes. No prompt or provider secret enters argv/public command metadata.

The JSONL parser is bounded to 32 KiB per record/feed and fails closed on malformed,
oversized or out-of-order records. ThreadStarted then TurnStarted are actual
harness-event observations, not kernel exec evidence or a claim that the provider
accepted/completed work. Empty, failed and partial streams cannot count as a
completed turn. Item content is ignored rather than interpreted as control events.

The current input mapping supports OpenAI API-key material, the exact pinned CLI
version, low/medium/high reasoning, a noninteractive session and no spending
budget. It refuses unsupported mandatory capability, model/harness/permissions
changes and malformed payload. Max-output enforcement, implicit prompt/counting
semantics, effective tool authorization, sandbox resource strength, real CLI
standalone ownership and protected auth publication still require the concrete
qualified executor. The production runner must remain ineligible until those
requirements are established; flags alone do not prove isolation.

Validation: 10 adapter tests passed in 0.04 s (6 new Codex tests plus 4 retained
input-boundary tests). A real offline subprocess receives the prepared argv and
exact stdin, with no provider invocation. Other cases cover tampering/settings,
private mode, budget refusal, auth JSON quoting and event order/bounds/failure.
Strict affected library and all-target/all-feature lint are recorded separately.
Logs: `/tmp/aq-codex-adapter-final-tests.log`,
`/tmp/aq-codex-adapter-final-clippy.log`.

Authoritative interface references were inspected directly: installed `codex
--help` / `codex exec --help`, official [CLI reference](https://learn.chatgpt.com/docs/cli/reference),
[noninteractive events](https://learn.chatgpt.com/docs/non-interactive-mode) and
[configuration reference](https://learn.chatgpt.com/docs/config-file/config-reference).
No operator authentication files or provider services were inspected.

Next host seam is explicit: the observed native Codex installation is about
319 MiB, exceeding the old InstalledAdapter 256 MiB and observed small-ELF 4 MiB
profiles. New named installation/image/observation profiles must account for its
actual bytes and exact stdin before WRK-18 can join a concrete host executor.
The existing small-ELF profile and frozen archives remain unchanged.
