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

Next integration remains concrete: stopped-writer-aware publication of derived
private Codex auth/configuration, actual installed mapping/implicit-output/counting
qualification, and joining the production pipeline's opaque grant to this host
owner. The runner must stay ineligible until those owners join; neither a manifest,
input recipe, offline peer nor this physical-host test grants execution authority.

Final strict all-target/all-feature Clippy passed for aq-worker and aq-session-host
in 10.03 s; default/no-default-feature worker library check passed in 3.93 s.
