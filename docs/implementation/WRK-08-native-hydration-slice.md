# Native source hydration owner, still no Prepare gate

This slice follows the frozen format-2 input checkpoint `ec41ad7`. Native journal
format 3 adds one bounded source plan per held attempt. Formats 1 and 2 are
unsupported historical layouts: opening refuses them unchanged, before pending
cleanup. There is no automatic migration, restart reattachment or capacity
release. Every reopened format-3 journal remains stop-and-preserve.

`InputPipeline::hydrate_source` requires the actual verified bytes of the original
retained typed execution spec and its completed independent input journal record.
It validates the spec against the exact original NativeOffer, original retained
artifacts and captured preparation. It never accepts a caller-built source
admission as a substitute. Git root/socket and storage capabilities come from the
trusted configured GitClient; this initial profile requires an already registered
exact worker repository binding, and performs no remote clone, registration or
arbitrary JSON proxy operation.

Before HTTP source discovery, retain its operation identity, exact spec and
preparation, root and conservative disk charge. Freeze the first authenticated
TransferManifest, then journal each exact member/offset/length intent before its
read. The streaming profile accepts at most 512 ranges of 256 KiB, counting both
members separately, with a manifest at most 1 MiB. Refuse impossible geometry
before the first range. One current intent plus monotonic committed cursors and
range count stays bounded; failed reads retry that same intent. No private
material bytes enter these records.

Bytes are staged under the configured Git root's private `.native-inputs/UUID`
owner directory, with an immutable marker and exclusive owner lock. The lock is
retained by the resulting opaque proof. Descriptor-relative no-follow opens,
regular/single-link/private-file checks and named inode rechecks refuse changed
roots, owner markers, symlinks and missing prefixes. A tail written before its
cursor commit is compared exactly before extending it; it is never overwritten
or silently truncated. Incomplete/empty markers and foreign published inputs
remain held. File and directory sync precede the matching journal cursor.

Whole raw manifest and bundle sizes/digests are verified against the authenticated
manifest, and the typed public Git manifest must match repository identity,
binding generation, transfer, SHA-1 base/checkpoint and bundle pins. The verified
metadata is committed before publishing the completed incoming directory with
no-replace rename and both directory syncs. Bundle hashing reads exactly the
admitted length plus one EOF probe; final size/mtime/ctime and named fd checks
refuse concurrent mutation or replacement. A growing-reader regression proves
the byte ceiling rather than relying on a preceding length sample. No blanket
cleanup follows failure.
The public input files remain retained after import; reclamation is later work.

The only finite Git requests are `repo.show`, `artifact.import`,
`workspace.create`, `retention.inspect` and `workspace.inspect`. Every exact
request/ID is durable before exchange on the bounded blocking lane. Lost import
or create responses retain the same immutable request; no replacement ID is
invented. Cached mutation results are historical receipts. Read inspections
always exchange again: an old active receipt cannot establish current retention.
Fresh handle identity/state/ref and exact workspace identity/generation/base,
allocating operation, lifecycle, clean HEAD and confined real path are required
before creating the non-Serde, privately constructed `HydratedWorkspace`.
Its original binding, local observation/deadline and retained staging owner are
process-local observations, not launch authority. No journal JSON or receipt can
reconstruct this type on restart.

The existing storage permit stays inside each blocking job through asynchronous
cancellation. Source network work uses the bulk lane; the NativeClient control
and renewal lanes stay separate. Cancelling an async caller leaves the pending
range/attempt and all capacity held. It does not constitute a central stop report
or release unknown Git state. Long synchronous Git calls remain bounded by the
existing adapter's socket deadlines; control admission cannot interrupt them.

## Capacity and assembly limits

For advertised Git bundle bound B and expanded bound E, the conservative charge
is `(E + B + 1 MiB) + E`: workspace, retained pool and artifact staging. The
unchanged real built-ins B=1 GiB and E=10 GiB therefore require **21 GiB + 1 MiB**
per attempt. `GitClient::native_reservation_bytes` exposes this logical bound and
refuses missing native inspection capabilities. `Journal::reserve` must be given
at least that charge; source admission refuses an insufficient original charge.
A fixture's 32 GiB logical reservation is not proof of physical capacity or an
allocation of that many real bytes. ENOSPC, shared disk contention, power loss
and complete physical headroom are not qualified here.

The eventual production driver must use this configured qualified profile before
Poll/Reserve and advertise genuinely available capacity; an offer produced from
a 2 MiB available-disk fixture is not a compatible real worker admission envelope.
A smaller profile requires explicit centrally pinned Git limits/qualification,
not lowering limits or overstating disk in this owner. The 512-range streaming
subset is also narrower than the default Git maximum bundle size.

Prepare remains withheld: a later join must retain all original inline bytes and
actual material pins, exact two linked source receipts, final startup and fresh
owner evidence under current preparation. No runnable capability, host hook,
process launch, initial grant or result settlement is added. Later launch intent
must precede **all** host/adapter startup hooks. Granted-without-Started remains
held until its separate versioned closure/settlement owner exists.

## Qualification boundary

Synthetic HTTP/socket tests exercise actual private filesystem/journal code,
request-before-I/O, lost import replay, failed-range identity, changed live
retention/HEAD, geometry refusal, held bulk/control separation, async cancellation
and four cursor-commit fault cuts. These are synthetic peers, not a daemon grant
qualification. Existing journal tests continue to qualify the common replacement
primitive separately.

An explicitly selected pinned aq-git executable test uses its public CLI/socket
with real `/usr/bin/git`, a fresh local repository and file, central
sync/checkpoint/export, HTTP member delivery, actual worker import/retention and
clean confined workspace contents. A later actual retention release must prevent
another hydration proof despite cached import/create receipts. The coordinator,
NativeOffer, preparation clocks and other retained input claims remain synthetic.
No provider, native host, launch/Prepare or supported-container claim follows.

The subsequent [Prepare input join](WRK-08-native-prepare-join.md) consumes the
opaque hydration owner and repeats current retention/workspace and bounded source
file verification immediately before recording Prepare. Its format4 remains an
explicit new layout; the format3 qualification above retains its original pin.
