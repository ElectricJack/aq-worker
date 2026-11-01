# Test-only native preparation process

`native-prepare-smoke` is an explicit `test-support` binary. It assembles real
native worker input owners against a separately supplied daemon and Git socket,
then stops at initial Grant. It is not a production driver, a host or a provider
launcher; no normal CLI command or default activation is added.

The first stdin line is bounded to 32 KiB and contains exactly `worker`
(WorkerConfig), `root`, `git_socket`, `git_root`, `runtime`, `qualification`,
`capabilities`, `features` and `renderer`. These are trusted local fixture inputs;
node/CF credentials never enter stdout or the journal. The existing root must be
canonical, private and owned; it may already contain the fixture-owned Git
service. Journal and material children must be brand new. Failed initialization
preserves partial/unknown state rather than deleting or reseeding it.

After fsyncing its new boot/journal, the binary emits one newline JSON frame
`{"phase":"ready","identity":...}` and flushes. It awaits a second complete
line, at most 1 KiB, containing exactly `{"continue":true}`. This lets the daemon
fixture admit a fresh synthetic inventory observation for the actual new boot
before registering and producing any offer. It sends Register once and Poll
once (sequence 1, limit 1); no automatic network mutation retries are performed.

The logical fixture envelope is two slots, 64 GiB disk, 64 GiB memory and 120,000
CPU millis. These are explicit test inputs, not measured physical capacity or a
qualified production resource profile. Poll uses the journal's available
projection; reservation charges the configured GitClient's conservative storage
bound, including the default 21 GiB + 1 MiB. An incompatible larger Git profile
is refused before polling. Production capacity preflight remains separate work.

The process reserves from the actual poll delivery deadline, persists exact
Accept and preparation, fetches every original inline artifact, installs all
actual private material deliveries, hydrates the original typed execution spec
and revalidates the opaque input join. Hydration consumes its verified spec, so
one extra spec read reuses the exact previously planned download requests; no
receipt reconstructs missing bytes. PreparedInputs retains actual verified
startup/input bytes, material owners and exclusive source ownership through the
actual NativeClient.prepare call.

On success stdout emits `phase: granted`, the exact durable Prepare `request`,
actual InitialGrantDelivery as `grant`, confined `workspace_path`, and material
receipt metadata. Output is bounded to 256 KiB. It emits no material bytes,
credentials or arbitrary remote errors. Failures return a generic `refused` phase
and fixed internal stage, then exit unsuccessfully without cleanup or a new ID.

The Grant is not persisted and cannot be resumed. No host hook, process start,
Started report, renewal, stop, result settlement or task completion occurs.
Granted task/source holds remain held; losing the response is not proof that no
grant issued. Runtime/harness/inventory qualification claims supplied by the
fixture remain explicitly synthetic even when transport, files and Git are real.
Cross-process actual-daemon qualification belongs to the daemon fixture and is
separate from this binary's framing/private-scope unit tests.
