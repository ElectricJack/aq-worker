# Explicit native exec installation and host profile

This is a separately named input/image/physical-host candidate for large native
CLIs. It does not widen the existing generic 256 MiB installation or observed
4 MiB small-ELF policy, and it does not establish model/provider eligibility.
Frozen component archives are unchanged. Before product consumption, package this
new host source as a new artifact/version and pin the actual helper digest.

## Installation, memory and startup clock

`NativeExecManifest` is bounded to 16 KiB and independently digest-pinned. Its
closed profile is `codex-0.166-exec-stdin-v1`; the nested installation keeps exact
adapter/runtime/qualification/renderer and executable/helper/tmux references.
Declared executable bytes must equal the actual sealed image size. Helpers/tmux
retain independently verified local file pins with a 64 MiB per-file bound.

`NativeExecPool` accepts at most 512 MiB per executable and at most 1 GiB / two
images in one explicitly admitted pool. It hashes/copies actual source bytes into
an immutable executable memfd, verifies the exact digest/header/architecture, and
charges their actual size once. Images and pool clones retain that charge; there
is no eviction or implicit budget reset. Node admission must account for this
pool in addition to per-attempt resources and any surviving old owners.

Do this installed preflight **before registration/poll/admission**. The actual
pinned Codex 0.166.0 ELF is 319,346,888 bytes. In the unoptimized qualification
binary its first seal took 7,036 ms; cached verification took 50 microseconds.
These are one local installation/cache observation, not production latency or
fleet qualification. The first load must not consume a short execution Grant.

NativeExecInstallation owns that immutable image and shares the charged pool.
The native-specific Codex input mapper reuses it, rechecking the small local
pins/configuration inputs rather than rehashing 319 MiB on every attempt. The
old mutable-file mapper and generic installation still perform their original
checks. No second material store or journal was introduced; private handoffs
continue to borrow the existing InstalledMaterial owner and exact receipt.

## New physical host boundary

The host profile is `linux-tmux34-sealed-native-exec-stdin-v1`. Its construction
requires an opaque NativeExecImage from the separately budgeted pool. Native
stdin mode refuses an interactive terminal input policy. The old profile keeps
its original identity, image limits and PTY stdin behavior; its serialized image
identity omits the additive profile field.

The closed observed gate still records blank-baseline permission durably before
release. Immediately before exec, the host verifies the final retained startup
against its exact descriptor, writes it to a non-executable anonymous memfd and
applies immutable write/grow/shrink/seal seals. It rewinds that held descriptor,
sets FD0, and uses the exact held executable FD through execveat. It does not
append another prompt or reopen a mutable payload path as stdin.

Native executable FD checks use exact device/inode/size plus mandatory immutable
seals, after the pool's full byte hash. A live descriptor prevents identity reuse;
write seals prevent changing those originally verified bytes. The old small-ELF
verification continues full hashing. After verification and stdin preparation,
the actual exec boundary rechecks the original permit, hard wall deadline, stop
request, guardian and original server liveness. No network wait or launch fallback
is introduced.

A physical Running receipt establishes actual live kernel FD-exec evidence.
Codex JSONL ThreadStarted/TurnStarted observations remain a separate higher-level
readiness input; physical Running alone does not prove model/tool availability,
faithful context counting, budget enforcement or a supported provider.

## Validation

- Final host unit suite: 20 passed, two explicit ignores (0.19 s); the actual
  Codex installation case below is invoked separately.
- Final adapter/input suite: 13 passed (0.05 s), including declared
  profile/size mismatch, independent digest,
  file replacement and preflight image reuse after changing the mutable program
  path. No actual provider executes.
- Actual new-profile tmux test: a static ELF larger than the old 4 MiB limit
  remains sealed after its mutable source is replaced, receives a payload larger
  than 4 KiB through immutable stdin, reports its exact byte count/hash, enters
  physical Running and stops with kernel ECHILD (0.52 s).
- Existing small-ELF observed positive process test also passes with its original
  deadlines. No existing test deadline or old profile limit was widened.
- An explicit ignored installation check loaded/sealed the actual independently
  pinned 319 MiB Codex ELF and proved accounting/cache/seal behavior without
  executing it. It requires AQ_TEST_NATIVE_CODEX_ELF and
  AQ_TEST_NATIVE_CODEX_SHA256; no binary path is guessed by product code.

Raw local logs: `/tmp/aq-native-exec-host-final-process.log`,
`/tmp/aq-native-exec-legacy-process.log`, `/tmp/aq-native-exec-codex-image.log`,
`/tmp/aq-native-exec-host-unit-final.log`,
`/tmp/aq-native-exec-install-final-tests.log`,
`/tmp/aq-native-exec-final-clippy.log`.

## Retained native launch and private home

`NativeCodexHostProfile` joins the separately verified installation/mapper to the
production pipeline's opaque durable execution. The pipeline resolves its own
retained startup, workspace, original installed provider material, actual Grant
and original deadlines; callers cannot supply reconstructed secret bytes or
substitute a fresh offer. Native and explicit synthetic launches share the
existing launch registry, outer intent/history and physical stop owner. The
registry must admit a newly reserved ID only after that reservation is durable;
a failed post-fsync registry handoff keeps the durable reservation held.

Codex auth is one closed derived input, not a second material store. A nonserializable
owner clones the original InstalledMaterial directory descriptor, pins its exact
record digest and material digest/byte count, and derives auth in zeroizing memory.
The host reserves a new private `codex-home/auth.json` with owner-only permissions.
Its existing launch intent binds the source process/descriptor, original record,
new directory/file identities and derived auth digest; it never contains the key.
The authority must persist that intent before any auth bytes are published.
Publication rechecks the original retained files, writes/syncs the exact auth, and
keeps the source descriptor alive. The helper repeats the original-source and
derived-file checks immediately before the final permit/liveness check and exec.
No alternate credential journal or secret-bearing argv/environment is introduced.

Private auth cleanup requires an opaque matching guardian/configuration proof with
kernel ECHILD. It verifies the same home/auth identities and exact derived bytes,
then removes only that auth file. Unknown generated home files remain; an empty
matching home can be removed. Replaced/symlinked/changed auth refuses cleanup. The
original MaterialStore input is never deleted here. A post-grant failure with no
genuine Running still retains its intent and holds for existing recovery; it does
not invent Started, successful closure or an unrelated cleanup authority.

Focused offline validation of this increment:

- Three private filesystem cases passed in 0.02 s: empty reservation/secret-free
  metadata, changed original record/key refusal, private permissions and symlink
  replacement refusal.
- Two actual native-profile tmux cases passed in 1.15 s. The >4 MiB static peer
  receives exact sealed stdin and private auth after durable intent; no live
  stopped proof is available. After physical ECHILD, cleanup preserves unknown
  generated home files and the original material, and empty-home cleanup is
  idempotent. Changing the original record after publication prevents exec and
  Started; only its actual stopped proof permits derived auth cleanup.

Actual installed mapping, implicit-output/counting and provider qualification
remain separate. In particular `max_output_tokens` is compared to the exact model
selection but this CLI mapping does not enforce that output bound; a runtime
qualification must refuse unsupported promises. The separately tested
[scoped Responses transport candidate](WRK-17-responses-output-bound.md) supplies
that request parameter without treating the context reservation as enforcement;
its production credential/owner join and provider qualification remain gated.
Physical Running is kernel exec
evidence, not a Codex ThreadStarted/TurnStarted or provider readiness assertion.
Neither a manifest, input recipe, offline peer nor these tests grants execution
eligibility. Frozen package archives require a fresh source snapshot for this
new host code before downstream consumption.

Final strict all-target/all-feature Clippy passed for aq-worker and aq-session-host
in 10.03 s; default/no-default-feature worker library check passed in 3.93 s.
