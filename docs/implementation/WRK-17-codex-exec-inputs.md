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

### Fixed tool environment and sandbox helper dependency

The mapper retains `shell_environment_policy.inherit="none"` and explicitly
sets only `shell_environment_policy.set.PATH="/usr/bin:/bin"`. This is distinct
from the outer CLI's PATH: Codex builds tool and sandbox-helper environments
from its shell policy. The fixed value lets the selected pinned Codex 0.166
profile discover the trusted OS sandbox helper without inheriting operator
HOME, credentials, shell state or arbitrary PATH entries. Login shells and both
shell snapshot features remain disabled. The generated override is parsed as
TOML in a focused test which checks the exact closed policy and sole set value.

The selected Linux Codex tool-execution profile depends on **bwrap**. The
qualification OS image contains bwrap 0.9.0 at `/usr/bin/bwrap`, mounted with
the trusted readonly OS image. The observed system executable is 72,160 bytes,
root-owned mode 0755, SHA256
`e318903862396f96de3df57264e0158682b952fd3fb53ac23d876413e7b30f71`;
its available flags include `--as-pid-1`, `--perms` and `--ro-bind-fd`.
`NamedExecManifest` independently pins Codex, the host helper and tmux; it does
**not** independently pin bwrap. Bwrap currently belongs to the selected
readonly OS environment's provisioning/qualification evidence. Its exact image
provenance, supported options and availability must remain part of that qualification;
an arbitrary executable found through an operator or workspace PATH is not
acceptable. This is a dependency of this Codex sandbox profile, not a required
external service or a mandatory implementation dependency of AQ's proposed
rootless capsule. There is no unsandboxed fallback if discovery or setup fails.

The preceding actual controlled tool attempt reached a real tool result but
failed inside Codex's sandbox helper with exit 101 because its empty tool
environment could not discover bwrap. It did not prove successful tool
execution. This source-only PATH correction does not qualify the next actual
attempt or relax production admission. See official OpenAI documentation on
[shell environment policy](https://learn.chatgpt.com/docs/config-file/config-advanced#shell-environment-policy)
for the separate `inherit` and explicit `set` controls. The pinned 0.166 schema
defines `set` as a string-valued map; that exact-version contract remains the
implementation reference.

Exact-version implementation references: Codex
[`rust-v0.166.0` shell environment construction](https://github.com/openai/codex/blob/rust-v0.166.0/codex-rs/protocol/src/shell_environment.rs),
[bwrap lookup](https://github.com/openai/codex/blob/rust-v0.166.0/codex-rs/sandboxing/src/bwrap.rs)
and [Linux sandbox launcher](https://github.com/openai/codex/blob/rust-v0.166.0/codex-rs/linux-sandbox/src/launcher.rs).

Source-only validation for this correction: eight focused Codex mapper/auth/
JSONL/offline Python cases passed in 0.05s; the generated-policy test parses
the actual override TOML. No actual Codex or provider process was run.
Strict affected all-target/all-feature Clippy also passed in 22.72s.

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
semantics, effective tool authorization and sandbox resource strength still
require qualification. The concrete native host now implements intent-bound
private auth publication and kernel process ownership, as described below. The production runner must remain ineligible until those
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

The [explicit native exec profile](WRK-17-native-exec-profile.md) implements
large-ELF installation accounting, immutable preflight image reuse, exact sealed
stdin, intent-bound private auth publication and matching ECHILD cleanup. The
old small-ELF profile and frozen archives retain their identities. Use
`CodexExecProfile::prepare_native` with the separately admitted installation to
avoid rehashing a large executable during each attempt.

The [scoped Responses transport candidate](WRK-17-responses-output-bound.md)
records the frozen output-cap interpretation, exact-version config/catalog
finding, bounded cap injection and remaining production qualification. The closed
mapper explicitly disables hosted web search; actual request observation must
verify the effective tool set. Product packaging must snapshot the final source
and exact component pins. Production execution remains refused until the
remaining concrete provider/limit/owner join is qualified.
