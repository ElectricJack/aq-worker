# WRK16A continuation history and source proposals

Status: pure source qualified; native FORMAT12 insertion and physical qualification pending.
This checkpoint does not qualify physical parking or central retention.

The sibling `continuation` module owns bounded data and finite proposals for a
single synthetic local-parking transaction. It reuses CON03 directly and the
existing source-transfer state machine. It cannot construct live execution,
stopped-workspace, coordinator-hold, retention or resumed-launch authority.
Native journal/root versioning and actual fsync/host ownership remain with their
existing owners. FORMAT11 contains no continuation field; insertion requires an
explicit new FORMAT12 rather than a compatible-looking default field.

`ParkingIntent` retains the exact original CON03 suspension/checkpoint pair,
explicit fake-hold marker, execution-record/host-intent/spec digests, source and
workspace association, actual directory device/inode pins, original disk and
source limits, fixed distinct operation identities and intent revision. The
physical bridge must collect these from actual retained owners before fsync;
caller-provided matching JSON is insufficient. A separate stop anchor refers to
the actual native stopped revision and parking record revision. The live token
also retains the true private stop proof and exact committed record digest; a
history record never reconstructs that token.

The source history is capped at 256KiB inside the existing root journal limits.
Its only Git operations are checkpoint and export. Each original request is
persisted before exchange, and its decoded projection must retain the original
result. Lost-response checkpoint replay preserves the saved UUID/head/recipe,
even when that same operation has advanced Git's workspace generation. It must
not recompute a new request from a changed head. No source work is proposed
without a stopped anchor. Original directory pins and descriptor association
must be rechecked by the physical owner around every source read/exchange.

Disk accounting retains observed stopped-workspace bytes plus bounded actual
export bytes against the original reservation. Unknown measurements or either
pending Git operation cannot satisfy the completion predicate. Both checkpoint
and export must have validated terminal results. The public Git artifact UUID
and canonical manifest digest remain distinct from the transport UUID and raw
manifest byte digest. Only predecessor Checkpoint upload is represented; no
successor Base relabeling/import/launch is implemented.

`source` returns cloned history proposals; `transfer` constructs the existing
bounded upload projection and validates acknowledgments using the extracted
shared finite helpers. A caller must fsync the proposal through the retained
native owner before executing the corresponding I/O. These functions perform no
I/O and do not authenticate a coordinator response. A final transfer receipt is
still a claim until the actual authenticated coordinator owner verifies it.
The future fake coordinator must explicitly retain source and summary and
supply effects reconciliation; an empty operation list is never treated as
reconciliation. Protected disk remains charged after upload or report.

Parent-approved policy: Missing summary blocks retained/migratable suspension
and protected-disk release, but permits local checkpoint/export and freeing
proven idle compute. `settled_source_claim` requires both validated Git results
and bounded observed source bytes; native capacity additionally requires its
actual stopped proof/anchor and exact source association. The separate
`completed_source_claim` adds the Supplied summary prerequisite for migration.
Neither predicate establishes central retention or reconciled effects. Missing
is never silently converted to Supplied. No production capability, ordinary
Suspended result, or new launch is introduced.

Parent approved an explicit FORMAT12 candidate in a separate worker0.5 branch,
under the native schema owner's final ownership. Frozen worker0.4/FORMAT11 must
not be edited or reinterpreted.

Planned first gate: focused pure tests and affected strict checking only. Pure
vectors cover no-stop/no-result refusal, both required source results,
fixed-ID/head replay, budget uncertainty/overflow, projection origins, exact
predecessor/boot binding and no Base relabeling. This is separate from the later
explicitly authorized disposable actual tmux + pinned aq-git + synthetic
coordinator composition. There is no provider, canonical agent, SQL, public
coordinator route or real central enforced hold in this slice.

Executed first pure qualification using the parent-released short Target2 lease,
two jobs, offline, incremental0 and dev/test debug0:

- `cargo test -p aq-worker --offline --lib continuation::tests`:5 passed,
  164 filtered,0 failed/ignored;0.01s tests after11.14s compilation.
- `cargo clippy -p aq-worker --offline --lib -- -D warnings`: passed10.68s.
- Affected rustfmt and diff whitespace check passed.

Target2 was released immediately after these gates. This gate is against the
independent sibling modules on worker0.3.4 baseline; it does not qualify the
separate worker0.5/FORMAT12 integration, private live stopped-workspace bridge,
actual tmux/Git, coordinator exchange or capacity projection. The five tests use
explicitly synthetic history/summary/source records and do not mint proof.
