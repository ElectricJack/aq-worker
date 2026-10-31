# AQRI candidate codec1

All integers use unsigned little-endian encoding. There is no padding, unknown
field, implicit default, floating value or extensible trailing area. Future
semantics require a new version/profile. `blob(max)` is u16 byte length followed
by exactly that many bytes; its maximum is checked before allocation. UTF-8 text
is strict; parser OSC fragments are bounded raw bytes that may end mid-scalar.
`bool` is exactly one byte0or1. The whole image is at most196608bytes.

Header: bytes `41 51 52 49 01`; blob64 profile
`aq-vt-scalar-clip-v1`; blob128 width policy
`unicode-width=0.2.2;ambiguous=1;compound=refuse`;64ASCII bytes of the canonical
shared TerminalIdentity SHA-256. Next are u64 next event ordinal, next output
sequence and resize generation. Sequence is positive and ordinal minus sequence
must equal resize generation. The outer content digest is supplied separately
and checked before decode; it does not authenticate a producer or a baseline.

State: u16 columns, u16 rows, bool alternate-active; current Attributes; eight
bools auto-wrap/origin/insert/newline/application-cursor/application-keypad/
bracketed-paste/cursor-visible; u8 cursor style0..6, mouse-mode bitset0..7,
mouse-encoding bitset0..3; one bool tab stop per column; blob512 UTF-8 title and
blob512 icon name; three u8 G0/G1/active charset values0..1; bool last-print
presence plus u32 scalar when present; u64 bell count; main Screen then alternate
Screen. Mode bits1000/1002/1003 are1/2/4; encoding1005/1006 are1/2. Charset0is
ASCII and1is DEC line drawing.

Attributes: foreground Color, background Color, u16 flags0..511. Colour tags:
u8 zero=default; one followed by u8 palette index; two followed by RGBu8triplet.
Flags are bold1, faint2, italic4, underline8, blink16, inverse32, hidden64,
strike128, double-underline256. Single/double underline cannot coexist.

Screen: current Cursor; DEC Saved context; ANSI saved Cursor; u16 top/bottom
margins; u16 row count (exactly dimensions.rows); that many Rows; u16 history row
count; that many Rows, with a total cell count<=12000. Alternate history is empty.
A Cursor is u16 row, u16 column, bool pending-wrap. It is in range; pending-wrap
requires the final column. Saved context is Cursor, Attributes, bool origin,
bool auto-wrap, three charset bytes.

Row: u16 cell count1..240 (grid rows exactly dimensions.columns), bool soft-wrap,
u16 run count1..cell count; then runs. Each run is positive u16 repeat count,
u8 cell width0/1/2, blob64 UTF-8 glyph and Attributes. Runs sum exactly to row
length and adjacent identical runs are forbidden. Width0is an empty continuation
of a preceding width2cell with identical attributes. Width2has a following
continuation. An empty ordinary cell has width1. Otherwise its first scalar has
that exact declared width and all remaining supported scalars have width0.

Parser: u8 mode (0ground,1escape,2charset,3CSI,4OSC). Charset includes u8 slot0..1;
CSI includes blob128 ASCII parameter/intermediate bytes20..3f; OSC includes bool
pending ESC terminator and blob4096 raw bytes (no C0orDEL). Then blob3 pending
UTF-8 prefix, permitted only in ground state and required to be genuinely
incomplete rather than invalid or complete. UTF-8 parser prefix is independent
of raw partial UTF-8 inside an OSC fragment.

Decoding refuses trailing bytes and re-encodes the bounded state to verify exact
canonical bytes. Cell/history counts and every variable length are checked before
allocation. A decoder cannot recover an invalid image through defaults. The
included alternate-partial vector contains both screens, nondefault modes and a
partial RGB SGR; continuing it proves more than an active-screen text dump.
