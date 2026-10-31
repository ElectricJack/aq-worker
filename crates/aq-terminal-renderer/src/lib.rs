//! Experimental abstract terminal model. It cannot attest a physical baseline,
//! mint a shared Snapshot or authorize a terminal operation.
mod codec;
mod model;
mod parser;
use aq_session_protocol::{Digest, OutputFrame, TerminalIdentity, Validate, canonical_digest};
use model::State;
pub use model::{Attributes, Cell, Color, Cursor, Modes, ScreenView};
use parser::Parser;
use unicode_width::UnicodeWidthChar;

pub const PROFILE: &str = "aq-vt-scalar-clip-v1";
pub const MAX_COLUMNS: usize = 240;
pub const MAX_ROWS: usize = 120;
pub const MAX_GRID_CELLS: usize = 12000;
pub const MAX_HISTORY_CELLS: usize = 12000;
pub const MAX_GLYPH_BYTES: usize = 64;
pub const MAX_CONTROL_BYTES: usize = 4096;
pub const MAX_CSI_BYTES: usize = 128;
pub const MAX_IMAGE_BYTES: usize = aq_session_protocol::MAX_SNAPSHOT_BYTES;
pub const MAX_FRAME_BYTES: usize = aq_session_protocol::MAX_FRAME_BYTES;
const MAX_WORK: usize = 2_000_000;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("terminal model is inconsistent")]
    Inconsistent,
    #[error("invalid exact identity, sequence, digest or event order")]
    Order,
    #[error("terminal model resource bound")]
    Bound,
    #[error("unsupported terminal semantics: {0}")]
    Unsupported(&'static str),
    #[error("invalid UTF-8 or terminal framing")]
    Framing,
    #[error("invalid bounded terminal image")]
    Image,
}
pub type Result<T> = std::result::Result<T, Error>;
#[derive(Clone, Copy)]
struct Work(usize);
impl Work {
    fn spend(&mut self, count: usize) -> Result<()> {
        self.0 = self.0.checked_sub(count).ok_or(Error::Bound)?;
        Ok(())
    }
}
/// An abstract premise, not evidence that a running tmux pane was ever blank.
/// Physical host adapters must prove their baseline separately before adoption.
pub struct AssumedBlank {
    pub terminal: TerminalIdentity,
    pub columns: u16,
    pub rows: u16,
}
/// Resize ordering is supplied by a trusted local sequencer, never inferred from
/// layout notices or inserted into literal OutputFrame bytes.
pub struct OrderedResize {
    pub ordinal: u64,
    pub after_output_sequence: u64,
    pub columns: u16,
    pub rows: u16,
}
pub struct Renderer {
    terminal: TerminalIdentity,
    state: State,
    parser: Parser,
    next_ordinal: u64,
    next_output: u64,
    resize_generation: u64,
    consistent: bool,
}
impl std::fmt::Debug for Renderer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Renderer")
            .field("profile", &PROFILE)
            .field("dimensions", &(self.state.columns, self.state.rows))
            .field("next_ordinal", &self.next_ordinal)
            .field("next_output", &self.next_output)
            .field("consistent", &self.consistent)
            .finish_non_exhaustive()
    }
}
impl Renderer {
    pub fn from_assumed_blank(baseline: AssumedBlank) -> Result<Self> {
        baseline.terminal.validate().map_err(|_| Error::Order)?;
        dimensions(baseline.columns as usize, baseline.rows as usize)?;
        Ok(Self {
            terminal: baseline.terminal,
            state: State::new(baseline.columns as usize, baseline.rows as usize),
            parser: Parser::default(),
            next_ordinal: 1,
            next_output: 1,
            resize_generation: 0,
            consistent: true,
        })
    }
    pub fn is_consistent(&self) -> bool {
        self.consistent
    }
    pub fn terminal(&self) -> &TerminalIdentity {
        &self.terminal
    }
    pub fn through_sequence(&self) -> u64 {
        self.next_output - 1
    }
    pub fn through_ordinal(&self) -> u64 {
        self.next_ordinal - 1
    }
    pub fn resize_generation(&self) -> u64 {
        self.resize_generation
    }
    pub fn screen(&self) -> ScreenView<'_> {
        self.state.view()
    }
    pub fn modes(&self) -> &Modes {
        &self.state.modes
    }
    pub fn title(&self) -> &str {
        &self.state.title
    }
    pub fn icon_name(&self) -> &str {
        &self.state.icon_name
    }
    pub fn bytes(&mut self, ordinal: u64, frame: &OutputFrame) -> Result<()> {
        self.apply(|this| {
            if ordinal != this.next_ordinal
                || frame.terminal != this.terminal
                || frame.sequence != this.next_output
                || frame.validate().is_err()
            {
                return Err(Error::Order);
            }
            let mut work = Work(MAX_WORK);
            for &byte in &frame.bytes {
                this.parser.byte(byte, &mut this.state, &mut work)?;
            }
            this.next_output = this.next_output.checked_add(1).ok_or(Error::Bound)?;
            this.next_ordinal = this.next_ordinal.checked_add(1).ok_or(Error::Bound)?;
            Ok(())
        })
    }
    pub fn resize(&mut self, event: OrderedResize) -> Result<()> {
        self.apply(|this| {
            if event.ordinal != this.next_ordinal
                || event.after_output_sequence != this.through_sequence()
            {
                return Err(Error::Order);
            }
            dimensions(event.columns as usize, event.rows as usize)?;
            this.state
                .resize(event.columns as usize, event.rows as usize)?;
            this.resize_generation = this.resize_generation.checked_add(1).ok_or(Error::Bound)?;
            this.next_ordinal = this.next_ordinal.checked_add(1).ok_or(Error::Bound)?;
            Ok(())
        })
    }
    pub fn gap(&mut self) {
        self.consistent = false;
    }
    fn apply(&mut self, f: impl FnOnce(&mut Self) -> Result<()>) -> Result<()> {
        if !self.consistent {
            return Err(Error::Inconsistent);
        }
        let result = f(self);
        if result.is_err() {
            self.consistent = false;
        }
        result
    }
    /// Complete abstract-model state, not a physical renderer attestation.
    pub fn image(&self) -> Result<StateImage> {
        if !self.consistent {
            return Err(Error::Inconsistent);
        }
        codec::encode(self)
    }
    pub fn restore(terminal: &TerminalIdentity, digest: &Digest, bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_IMAGE_BYTES || Digest::of(bytes) != *digest {
            return Err(Error::Image);
        }
        codec::decode(terminal, bytes)
    }
}
/// Opaque bounded codec bytes; Debug deliberately never prints terminal output.
pub struct StateImage {
    bytes: Vec<u8>,
    digest: Digest,
}
impl StateImage {
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub fn digest(&self) -> &Digest {
        &self.digest
    }
}
fn dimensions(columns: usize, rows: usize) -> Result<()> {
    if columns == 0
        || columns > MAX_COLUMNS
        || rows == 0
        || rows > MAX_ROWS
        || columns.checked_mul(rows).is_none_or(|n| n > MAX_GRID_CELLS)
    {
        return Err(Error::Bound);
    }
    Ok(())
}
fn scalar_width(c: char) -> Result<usize> {
    if c == '\u{200d}'
        || c == '\u{20e3}'
        || ('\u{fe00}'..='\u{fe0f}').contains(&c)
        || ('\u{e0100}'..='\u{e01ef}').contains(&c)
        || ('\u{1f1e6}'..='\u{1f1ff}').contains(&c)
        || ('\u{1f3fb}'..='\u{1f3ff}').contains(&c)
    {
        return Err(Error::Unsupported("compound Unicode"));
    }
    c.width().ok_or(Error::Unsupported("nonprinting scalar"))
}
