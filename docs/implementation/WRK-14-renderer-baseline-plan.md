# WRK-14 renderer and baseline proposal (for review)

The reader cutoff and incremental-decoder checkpoints remain separate. This
proposal does not call the current terminal complete. Frozen public0.1.0
contracts remain unchanged; daemon integration needs separately versioned proof
where current literal OutputFrame/ResizeReceipt cannot express ordering.

## Pure engine first

Add an independent small `aq-terminal-renderer` candidate crate. It consumes an
exact TerminalIdentity and explicitly ordered Bytes/Resize events. Byte events
validate existing frame digest/epoch/strict sequence. A local event ordinal
orders both kinds; a resize names the exact preceding output watermark. Gaps,
changed identity, unsupported semantics or unknown controls permanently make
this engine inconsistent until an independently proved baseline replaces it.
No implicit blank reset or capture_text recovery exists.

The initial profile is deliberately explicit and bounded, not a promise to
emulate every xterm feature. State includes both grids, finite scrollback, cursor
and saved contexts, margins, tab stops, current/cell attributes and colours,
autowrap/origin/insert/newline, main/alternate selection, cursor/keypad/paste/mouse
modes and incremental UTF-8/control parser state. ANSI cursor/erase/scroll/insert,
SGR8/16/256/RGB, DEC line drawing and main/alternate-screen transitions are
qualified as one named profile. Unknown mode/control/OSC/DCS effects refuse
completeness rather than silently disappearing.

Suggested absolute limits:240columns,120rows,12000cells per grid,12000scrollback
cells,64UTF-8bytes per cell,128bytes CSI state,4096bytes control-string state,
64KiB input frame,196608byte encoded image (the shared snapshot ceiling).
Quotas are checked before allocation or mutation. Encoded export may refuse a
large valid internal state; it must not truncate it into an apparently complete
image. Limits and codec/width table identity form the renderer artifact digest.

Use exact unicode-width0.2.2 for a declared scalar-width profile (ambiguous width1).
The initial conservative profile supports UTF-8 split scalars, CJK/wide characters
and bounded zero-width combining marks, and refuses ZWJ, variation-selector,
regional-indicator/emoji-modifier/keycap sequences until their cluster semantics
are separately qualified. This is a named incomplete feature coverage, not an
assertion that scalar width equals arbitrary Unicode grapheme width. The API
must surface Unsupported, not replace or drop glyphs. A later grapheme profile
must version its semantics and test against the actual host/client pair.

An AQ-owned binary codec serializes the complete owned engine state, including
partial parser state, both buffers and modes, using bounded lengths/checked
counts and no implicit defaults. It rejects wrong version/profile/terminal,
trailing bytes, invalid UTF-8, cell-width inconsistency and oversized counts
before allocation. RLE blank/identical cells keeps ordinary full-screen state
within the fixed image ceiling. This is not ANSI output intended for blind
injection into a terminal. Snapshot/live equality is tested by restoring each
stream-prefix image and comparing subsequent state with uninterrupted execution.

Pure-engine construction accepts an explicitly assumed blank baseline only for
its abstract model; it cannot mint the shared Snapshot or a runtime completeness
attestation. That requires the separately verified host gate below. Initial
resize algorithms and soft-wrap rules must be named and independently compared
against actual tmux before host completeness can be advertised. Unsupported
resize cases refuse consistency; they are not guessed from layout notices.

Dependency review: asciinema/avt0.18 stores a single char per cell; upstream
vt100 exposes a useful grid but formatted active-screen output is not evidence
of serializing every private parser/buffer mode, and fixed combining storage can
silently omit marks. Neither should be called a complete snapshot by wrapping
capture/formatted text. A small owned strict-profile state machine makes missing
coverage explicit; it does not silently claim universal terminal compatibility.
Primary sources inspected: https://github.com/asciinema/avt/blob/main/src/cell.rs
and https://github.com/doy/vt100-rust/blob/main/src/cell.rs .

## Pre-harness host baseline (review before launch API edits)

A new additive observed-launch API must accept the exact terminal identity and
closed observation profile in the sealed configuration digest. The existing
LaunchIntent/GuardianIdentity/HostObservation bytes stay unchanged. Its private
pane helper stops at a readiness gate before native harness exec. A new private
guardian-owned observation sequencer attaches while the gate is closed; no human
or worker input effect is allowed pre-Running. Fresh server/pane, exact measured
dimensions, fixed configuration/defaults and zero harness bytes establish a
qualified blank baseline; a control-client attach ACK by itself does not.

The trusted caller drains the local baseline/output while awaiting actual
Running. It cannot register a fake Started to obtain daemon access: current
TerminalActive requires Started. The gate consumes the existing remaining grant
TTL and immutable wall deadline; readiness never renews/restarts either. Missing
ready/lease, expiry/cancel or unsupported profile stops/refuses exec. Actual
record_started remains after baseline gate and harness exec, preserving logical
pregrant guardian versus physical nonce semantics. Granted-but-never-Started
recovery is a separate daemon qualification gate; NeverGranted is not applicable.

The guardian sequencer must order resize with byte-drain fences. Current
%layout-change, capture_text and resize ACK cannot prove this. Propose a new
versioned private/public proof: exact physical binding, event ordinal, prior
output watermark, resize generation/dimensions and post-resize byte boundary.
If a safe tmux mechanism cannot establish the boundary, resize invalidates
renderer completeness rather than fabricating an order. Do not encode synthetic
resize ANSI as literal harness OutputFrame bytes.

Captures get explicit newly minted request IDs after input uncertainty; a quiet
output watermark is not freshness. An independently verified capture barrier
returns exact engine state plus output/event/resize watermark and profile. Only
then can the existing Snapshot envelope carry that codec, with newer relay proof
capabilities where needed. Lost observer/unknown range never permits same-epoch
reattachment without a new qualified producer/baseline handoff.

## Required qualification before runtime adoption

Pure finite matrices cover split UTF-8/escapes at every boundary, wide/combining
cell edits, alternate/main restore, attributes/cursor/margins, scroll/resize,
partial-state encode/restore continuation, unknown controls, all memory/count
ceilings, malformed codec and sequence conflict. Actual synthetic host tests
then prove observation-before-exec, no startup-byte changes, same TTL/deadline,
producer death/stall and fail-closed baseline loss. Actual screen/cursor oracle
comparisons may use capture_text only as a test oracle, never a production state
source. Full model/provider/human attach qualification and new relay proof schema
are separate; no live providers or deployment are used for this work.

## First fixture repertoire and practical gate

The first engine qualification target is the synthetic native interactive TUI,
not an untested provider. Recorded deterministic streams will cover a full-screen
alternate buffer with header/body/status redraw, cursor visibility and style,
bracketed paste/application cursor modes, progress-line erase, scrolling region,
tabs, SGR index/RGB, CJK/single-scalar emoji/combining accents, save/restore and
return to the original main buffer. These checked streams include ordinary CLI
scrolling and multiline redraw, not only isolated escape microtests. Unsupported
counts/reasons are asserted so a supposedly supported stream cannot silently
fall back. Provider-specific captures remain a later offline qualification gate;
no live provider is started to manufacture evidence.

Modern input/transport-only controls (e.g. bounded mouse reporting, bracketed
paste and keyboard modes) must be represented or explicitly refused, not ignored
as if they never occurred. Synchronised rendering/kitty keyboard/OSC hyperlinks
need an explicit supported model or typed Unsupported until added. The initial
profile cannot be advertised for a real harness whose recorded stream requires
an unsupported feature. Exact resize/reflow semantics remain independently
qualified against the selected actual host before any complete runtime snapshot.
