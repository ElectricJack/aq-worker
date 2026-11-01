# WRK-08 native hydration component qualification

Source: `f14694e2c5da546018526ccb2d66f146fed24696`, following the frozen
format-2 input/evidence checkpoint `ec41ad7` / `238c2ce`. The
[manifest](WRK-08-native-hydration-qualification.json) retains 28 revision-pinned
source hashes and nine tracked qualification/development logs. No public wire,
Cargo dependency, default runtime or runnable capability changes are included.

The final ordinary worker package run passed **142 tests**, with four declared
ignores. Thirteen ordinary cases are new: seven synthetic HTTP/socket pipeline
cases and six filesystem/hash cases. The source cursor fault case exercises four
replacement cuts. The four ordinary ignores consist of three previous actual-Git
fixtures and the new explicitly selected actual-Git case. The new case separately
passed **one test**. Strict all-target/all-feature Clippy, formatting and diff
checks passed. The final logs use the exact bytes committed at the source pin;
the manifest independently checks those bytes with `git show`.

The actual Git case selected an immutable copied executable with SHA256
`c2ff26f5e63008aec0fbd4082a547e87825e651d4aba0744c4b9fdf752d58c39`, producer
`2035654eeda57ded8880ea3f518ff0b4d324fa49`, and `/usr/bin/git` 2.43.0. It used
unchanged built-in artifact limits, a fresh repository containing a real file,
public central sync/checkpoint/export, HTTP source reads, actual worker import,
active retention inspection and exact clean workspace contents. Actual retention
release subsequently prevents another proof despite cached import/create
receipts. The HTTP coordinator, NativeOffer, sampled preparation clocks and other
input claims remain synthetic. This is neither an actual daemon Grant test nor
a provider/native-host/container qualification.

The [owner design and limitations](WRK-08-native-hydration-slice.md) remain part
of the evidence. Format 3 refuses formats 1/2 unchanged; reopen preserves holds
without producing fresh source proof or network authority. The configured real
Git defaults conservatively charge **21 GiB + 1 MiB** per attempt. The fixture's
32 GiB logical reservation proves admission arithmetic only. A production driver
must derive a compatible qualified Poll/Reserve disk envelope before use; a 2 MiB
fixture offer cannot hydrate under these defaults. Physical ENOSPC/headroom,
complete restart recovery, Prepare/launch/Started/settlement and latency closure
remain open.

The review map is:

- `native/journal/source.rs`: original verified spec admission, bounded ranges,
  exact finite Git requests, retained unknown intent and common snapshot commit.
- `native/hydration.rs`: bounded pipeline, read-before-proof reconciliation and
  privately constructed non-Serde `HydratedWorkspace`; no Prepare gate.
- `native/hydration/files.rs`: private staged members, exact tail replay,
  no-replace publication and bounded hash plus final inode/metadata checks.
- `native/journal/hydration_scenarios.rs`: actual request-before-I/O assertions,
  lost import ID, live retention/HEAD refusal, old format, four commit cuts,
  actual held response/control/cancellation, bounds and missing capability.

Retained development failures are explicitly superseded: insufficient fixture
logical reservation, missing newly created Git artifacts parent, captured
revision mismatch, test synchronization after cancellation and one strict lint
finding. A static review also identified hashing to an unbounded EOF after a
length sample. The final implementation consumes exactly the admitted length
plus one byte and checks final metadata/named inode; a growing-reader regression
proves that bound. None of the failed runs is counted as a passing qualification.

The passing diff check covers the source checkpoint. Exact raw cargo logs retain
their original trailing blank lines; staging those evidence files separately
reports EOF whitespace in `worker.log` and `actual-git.log`. Their bytes and
hashes are intentionally retained unchanged.
