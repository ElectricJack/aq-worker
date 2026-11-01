# Durable native input component qualification

Frozen source `ec41ad77ee3ef1a3b53d2ac1ab4325cb1993b715` passes 129 worker
all-target/all-feature tests, with three existing actual-Git cases explicitly
ignored. The focused journal run passes 22 cases, including seven new input
scenarios; it is a subset, not extra package coverage. Strict all-target/all-feature
Clippy and formatting pass. Source and tracked log hashes are in the adjacent
JSON, including labelled development compilation/lint corrections.

Actual synthetic HTTP handlers inspect the fsynced published journal before
replying. A two-chunk artifact is planned atomically before I/O; failed and
completed-download retries keep exact request IDs. Dropping the RAM artifact and
forcing a server refusal proves a durable receipt cannot reconstruct its bytes.
Only a new verified download returns VerifiedArtifact; no Prepare gate exists.

Private material publication-ACK loss replays the same operation and original
installation. The independent journal pin survives a postrename write fault,
reopens only as historical evidence and rejects a self-consistent local metadata
replacement. Journal inspection finds neither private material bytes nor node
credentials. Pre-I/O journal uncertainty prevents HTTP entirely. Format 1 refusal
preserves both published and pending bytes. Changed payload/ID associations,
metadata bounds, expiry and bulk/control separation are exercised.

No real provider, daemon/native driver, Git hydration or runnable registration is
qualified here. The legacy Git owner is unchanged; its earlier explicit real-Git
qualification remains separate evidence. Logical snapshot/operation bounds do not
prove physical headroom, ENOSPC, power-loss durability or history-insensitive
throughput. All uncertain holds remain charged; no blanket cleanup or format
migration is provided. See the [slice plan](WRK-08-native-preparation-slice.md).
