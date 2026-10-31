# AQ bounded terminal model candidate

This pure component models an explicitly assumed blank terminal. It does not
prove a physical baseline, own a process, grant access, emit a shared Snapshot,
resolve uncertain input, or advertise Codex/provider compatibility. There is no
transport, filesystem or terminal side effect in the library. Examples emit only
deterministic synthetic conformance bytes.

`aq-vt-scalar-clip-v1` consumes exact TerminalIdentity-bound OutputFrames and
separately ordered Resize events. Frame sequence and event ordinal are strict;
resize names the preceding output sequence. A gap, conflicting identity/order,
invalid digest, malformed input or unsupported effect permanently invalidates
this model. It never guesses a new baseline, replays a duplicate or resets an
epoch. Querying cells after failure is diagnostic; exporting an image refuses.

The profile owns main/alternate grids, finite scrollback, current/saved cursor
contexts and attributes, margins, tab stops, charsets, current title/icon,
wrap/origin/insert/newline, cursor/keypad/paste/mouse modes and partial UTF-8/CSI/
OSC state. Mouse modes/encodings retain independent bits so disabling a newer
mode does not erase an older enabled fallback. The binary codec preserves all
owned state, including hidden buffers and parser fragments.

The Unicode width table is exact unicode-width0.2.2, ambiguous width1. Split
UTF-8 scalars, CJK/wide characters and bounded zero-width combining marks are
supported. ZWJ, variation selectors, regional indicators, emoji modifiers and
keycaps refuse until a separately qualified compound/grapheme profile exists. REP after an appended zero-width scalar is also unqualified
and refuses, rather than repeating a stale base character.
Orphan combining marks and unsupported controls refuse rather than replacing,
dropping or normalising bytes. This is not a claim of arbitrary Unicode grapheme
or universal xterm compatibility. Zero-width format/bidi scalars and browser
cell presentation need separate physical/display qualification; retaining their
bytes under the scalar model is not evidence of matching a real terminal.

Supported controls include cursor positioning/motion; erase/insert/delete cells
and lines; bounded scroll regions; tabs; save/restore; SGR default/indexed/RGB,
bold/faint/italic/underline/double-underline/blink/inverse/hidden/strike; DEC ASCII
and line drawing; main/alternate47/1047/1049; application cursor/keypad; bracketed
paste; cursor visibility/style; mouse1000/1002/1003 and encoding1005/1006;
OSC0/1/2 titles, OSC10/11 colour queries and explicit CSI device/status queries. Unknown
modes/controls, graphics, clipboard, hyperlinks, synchronized-output2026 and
kitty keyboard negotiation are Unsupported. A real harness requiring any of
these is not qualified by this candidate. The checked synthetic CLI fixture
exercises ordinary logs, full-screen redraw, status/progress, scrolling/margins,
colours, tabs, modes, wide/combining text and main-buffer restoration; it is not a
provider recording.

Resize is the named upper-left clip profile, not inferred physical tmux reflow:
preserve the upper-left overlap, fill exposed cells with current attributes,
clear damaged wide pairs, clamp current/saved cursors, clear pending wrap and
soft-wrap flags, reset margins to the full screen, preserve original-width
retained history and add default tab stops in new columns. Actual host/renderer
resize agreement and a sequencer barrier remain prerequisites to runtime use.
No layout notice or capture_text establishes them.

Logical limits are240columns,120rows,12000cells per grid,12000retained history
cells,64UTF-8bytes per glyph,128CSI bytes,4096control-string bytes,512title/icon
bytes,64KiB input frame,32CSI parameters and196608encoded image bytes. Two million
charged work units per frame bound expanded operations; reset/alternate-grid
allocation is charged before mutation. Resized/history rows discard spare cell
capacity before long-term retention, so narrow rows cannot hide large wide-row
allocations from the cell quota. Glyph allocation slack, finite row/deque
metadata, codec scratch and allocator overhead are additional costs; this is not
an RSS/cgroup limit or a latency/scale claim. A valid large model may refuse image
export at the fixed byte ceiling; it never truncates state into apparent success.
Configured scrollback retention is not a full session transcript.

Image restore requires exact terminal identity and content digest, version/profile
and width policy. Bounded counts are checked before allocation; UTF-8, wide-cell
pairs, mode bits, cursor bounds, canonical RLE, parser state and trailing bytes
are validated. Every-prefix restore/continue comparisons and malformed-image
probes supplement independent expected-cell/control cases. See CODEC.md.

Reproduce vectors offline:

```
cargo run -p aq-terminal-renderer --example record_cli_stream --locked --offline --quiet > /tmp/synthetic-cli.ansi
cargo run -p aq-terminal-renderer --example export_fixture_image --locked --offline --quiet > /tmp/alternate-partial.aqri
cmp /tmp/synthetic-cli.ansi crates/aq-terminal-renderer/tests/fixtures/synthetic-cli.ansi
cmp /tmp/alternate-partial.aqri crates/aq-terminal-renderer/tests/fixtures/alternate-partial.aqri
```

Physical completeness still requires an independently qualified pre-exec blank
baseline, exact executable ownership, ordered resize/capture barriers and fresh
post-uncertainty capture tokens. Existing deadlines must not be renewed by
readiness. No library method mints those facts from this abstract state.
