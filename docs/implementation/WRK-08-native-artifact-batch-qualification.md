# Native artifact batch qualification

Source `de8fa62` passes149 package tests with four declared actual-Git ignores,
plus the separately enabled pinned actual-Git hydration/Prepare case. Strict
all-target/all-feature Clippy, formatting and source diff passed. Independent
review found no blocking issue. Exact source and log hashes are adjacent.

New tests prove two journal commits for the complete set, no extra metadata
commits or changed IDs on retry, preserved exact plans with no partial completion
after a mid-batch HTTP failure, and eight interrupted-write cases across plan and
completion. Reopen yields only an old or complete new state and keeps resources
held. Existing Prepare/hydration cases now collect their inputs through this
batch API; existing individual download cases still pass.

The optimization changes no deadline, public protocol or journal format. The
8MiB set limit is checked before network reads. It removes per-artifact snapshot
publication, but does not qualify cold preparation or claim a latency benchmark.
The separate real-daemon experiment retains its earlier two-second-window
failures; this component qualification does not overwrite that evidence.
