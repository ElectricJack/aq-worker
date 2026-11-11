# Multiple projects on one Claude docker-local worker

A result adapter retains `permission_digest` as its independently installed
primary plan. Operators may install up to 31 additional distinct, exact plan
digests in `additional_permission_digests` on the Claude docker-local profile.
An omitted or empty list preserves existing canonical definition bytes. Other
adapter profiles reject a nonempty list.

Each entry hashes the whole canonical manifest permission plan, including
requested, effective, required, and omitted permissions. Install a new immutable
adapter definition and matching runtime/harness qualification references when
changing this list. Keep every project's catalog permissions and ceiling scoped
to that project and repository. The list grants no authority: admission, retained
execution scope, material delivery, and live grants still control execution.

The result channel pins the actual admitted plan from the retained original
manifest. Result installation checks that same plan after validating it against
the installed adapter. An unlisted plan, duplicate entry, or changed permission
field is refused. Sharing a worker does not require sharing project permissions.

Idle polls reuse the acknowledged capability snapshot until it is within five
seconds of expiry. Replacing it on every work check invalidated an offer whose
repository export lasted across two checks. The same scoped snapshot remains
subject to its original deadline and server authority checks; this is not a TTL
extension. A new snapshot is published before expiry or after an attempt.

The finite source download range count covers the advertised default Git
profile: a 1 GiB bundle plus a 1 MiB manifest, transferred in 256 KiB ranges
(4100 total). The older 512-range limit silently capped source near 128 MiB,
rejecting aq-server's real ~166 MB bundle despite the configured profile.
The Git byte limits, manifest association checks, deadline and disk reservation
remain enforced.

Source API1 status can advertise `aq-source-max-range: 1048576`. New workers
negotiate 1 MiB ranges only when that header is present, and fall back to the
original 256 KiB range size otherwise. Older workers keep using 256 KiB with a
newer server. The selected range size is durable per download; legacy journals
retain their original default and canonical bytes. All transfer bindings,
whole-file digest checks, grant/retention fences and bounded response limits
remain in place. The larger ranges reduce repeated control and journal work
for real repository bundles.

Whole-bundle verification streams up to the configured 1 GiB profile instead
of retaining the old 128 MiB scratch ceiling. The generic exact-stream helper
lives in aq-node-service; its regression streams 129 MiB without allocation and
also rejects truncated, growing and over-limit input. Durable staging tests use
real 1 MiB chunks and refuse a chunk one byte beyond the negotiated maximum.

After source hydration, the same retained scoped-presence producer publishes a
fresh inventory before execution preparation. A real checkout can outlast the
initial inventory's 30-second validity. This obtains a new receipt from the
server under the existing compatibility-epoch checks; it does not restamp or
extend the original report. Production collection fixtures cover both frozen
and registry-refresh profiles using the real producer and fake HTTP owners.
