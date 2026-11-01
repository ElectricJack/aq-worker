# Inline read overlap qualification

Source `a39e990` passes135 default package tests and152 all-feature package tests,
with four declared actual-Git ignores in each run. The separately enabled pinned
actual-Git hydration/Prepare case passes. Strict all-target/all-feature Clippy and
formatting pass, and independent review found no blocker. Counts overlap: the
all-feature run includes the default tests and feature-gated material/guardian
and smoke-binary checks. [Exact manifest](WRK-08-artifact-overlap-qualification.json).

The all-feature tests execute in8.49seconds combined after build; the actual-Git
case takes3.57seconds. These are test durations, not product latency benchmarks.
The release test-support worker is frozen by digest in the manifest for a
separate daemon process check. Its build takes96seconds in an isolated target.
No component result is substituted for that separate assembled result.

The new peer barrier verifies exactly two simultaneous original input reads,
independent control progress, no partial completion and exactly two journal
publications. Previous stable-ID retry and all interrupted-write cases pass.
No wire, deadline, resource slot count or dependency changed. See the
[implementation boundary](WRK-08-artifact-overlap.md); cold preparation remains open.
