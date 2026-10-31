# Observation reserve correction: combined worker checkpoint

Source `946e13f` integrates experimental observation store0.0.5 with the
qualified terminal bridge and existing graph client. The clean combined workspace
passed 294 tests with 29 declared optional tests ignored; strict
all-target/all-feature workspace lint and formatting passed. The
[qualification inventory](WRK-15-reserve-integration.json) binds exact source,
commands and raw logs. Test-process durations sum to
142.94 seconds; this is not an
independent total wall-clock measurement.

The correction reserves metadata/delivery, routine control and boot-fence capacity
inside the configured logical limit. Normal records and parser history cannot
consume those margins. File-change gaps hold their source until acknowledged;
repeated boots do not duplicate already-held sources. Exact reserved monitor
provenance is refused at ordinary ingress. Old/missing/unknown format markers
refuse before opening redb, without implicit migration, pruning or reseeding.

Independent read-only review found no further blocker in that logical-capacity
scope. The original0.0.4 failure, unchanged-default-limit recovery probe, fault
campaign and exact extracted-package qualification have separate component
records. Physical-cap or ENOSPC progress remains unguaranteed. The production
worker does not yet adopt this experimental store.

The earlier [terminal integration](WRK-14-integration.md) retains its explicit
tmux/HTTP evidence; those ignored host-specific tests were not rerun here. This
checkpoint does not qualify a released native harness or full terminal output.
