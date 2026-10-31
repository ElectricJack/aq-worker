# WRK-14 strict abstract terminal model

Source `715c4b3e32f8d2f0ea0c7f4cb9644f395dd97103` adds the pure unpublished
`aq-terminal-renderer0.1.0` candidate after reviewed design `a257fcc`. It owns no
process, transport, authorization or physical baseline and cannot mint the shared
Snapshot. Host/rendered startup code is unchanged by this source slice. Runtime,
Codex/provider compatibility, browser presentation, resize/reflow agreement and
physical completeness remain unqualified.

The named scalar-width/clip profile owns both grids, finite scrollback, current
and saved cursor/attributes, margins, tabs, charsets, title/icon, wrap/origin/
insert/newline/cursor/keypad/paste/mouse flags and partial UTF-8/CSI/OSC parser
state. Exact terminal identity, output sequence and separate Bytes/Resize ordinal
are mandatory. Invalid/gapped/unsupported input permanently refuses consistency.
The AQRI binary codec restores all owned state with strict count checks before
allocation, terminal/profile/content binding, canonical RLE and no trailing bytes
or implicit defaults. It is not an active-screen ANSI dump or capture_text.

Clean-target qualification passed15tests, strict all-target/all-feature Clippy,
formatting and exact regeneration of both checked vectors. Tests use no services,
network or providers. The pure test process duration was0.52s; no total wall-time
or scalability guarantee follows. Exact unicode-width0.2.2 was materialized before
locked/offline qualification. Its revision and ambiguous-width1 policy are part
of the codec; compound Unicode and unmodelled terminal effects refuse. Zero-width
format/bidi behavior and browser cell rendering still require physical/display
qualification. No real-harness capability is advertised.

The484byte deterministic synthetic CLI stream covers logs, alternate-screen
redraw, progress/status, RGB/index attributes, tabs/margins, DEC line drawing,
wide/single-scalar emoji/combining accents and return to the original main buffer.
It is a generated conformance trace, not an actual Codex/provider or PTY capture.
Every prefix restores and continues identically, including hidden buffers and
partial escapes/UTF-8. Separate expected-cell cases avoid relying solely on
self-roundtrip. The checked alternate/partial-colour binary image and hashes
reproduce exactly. See the crate README and CODEC.md for the bounded repertoire,
resize semantics, state fields and binary layout.

Other cases exercise identity/sequence/digest conflict, explicit gaps, unsupported
controls, Unicode refusal, damaged wide pairs after clipping, overlapping mouse
modes, title/icon partial UTF-8, finite history, truncated/malformed/oversized
images, and large valid state refusing export instead of truncating. Reset and
alternate-grid reallocation are charged before expanded work. Logical grid,
history, glyph, parser, parameter, frame and image ceilings are explicit; this is
not an RSS/cgroup or hard scheduling claim.

## Retained quota regression and build isolation correction

Self-review found that narrow rows could retain wide Vec<Cell> spare capacity
through repeated resize/scroll cycles, hiding allocations from a logical history
quota. Both resized grids and long-term history now compact cell storage. A
64cycle regression checks retained capacity against charged cells. An isolated
fault variant removing compaction fails that same test with772480slots against
7680charged cells. Removing only the history guard still passes because the
independent resize guard remains. These are explicitly injected negative variants,
not claimed historical releases; their exact source hashes, source files and
lockfile are retained. The corrected source passes.

The first negative probe mistakenly reused the normal target directory. Cargo
then reused the same-version negative test binary in a normal qualification run,
reproducing its capacity failure despite the corrected source. That failed run
and depinfo were retained; no source/archive/pass checkpoint was frozen from it.
After cleaning only renderer artifacts, normal source passed again. Final
qualification used fresh `/tmp/wrk14-renderer-clean-target` with
`CARGO_INCREMENTAL=0`; the negative variant used its own distinct target and still
failed as expected. Future fault variants must not share qualification targets.

The source inventory has15hashes; `WRK-14-renderer-model.json` hashes the retained
artifact bundle, including raw preliminary/final logs, clean vectors, negative
sources and exact frozen source. No standalone archive has yet been frozen or
published. The next host baseline/API work must start from current main with
host0.1.3 and preserve exact RenderedStartup bytes and original deadlines.
