use crate::*;
use std::collections::VecDeque;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Color {
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Attributes {
    pub foreground: Color,
    pub background: Color,
    pub flags: u16,
}
impl Default for Attributes {
    fn default() -> Self {
        Self {
            foreground: Color::Default,
            background: Color::Default,
            flags: 0,
        }
    }
}
#[derive(Clone, PartialEq, Eq)]
pub struct Cell {
    pub(crate) glyph: String,
    pub(crate) width: u8,
    pub(crate) attributes: Attributes,
}
impl std::fmt::Debug for Cell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cell")
            .field("glyph_bytes", &self.glyph.len())
            .field("width", &self.width)
            .field("attributes", &self.attributes)
            .finish()
    }
}
impl Cell {
    pub fn glyph(&self) -> &str {
        &self.glyph
    }
    pub fn width(&self) -> u8 {
        self.width
    }
    pub fn attributes(&self) -> Attributes {
        self.attributes
    }
    pub(crate) fn blank(attributes: Attributes) -> Self {
        Self {
            glyph: String::new(),
            width: 1,
            attributes,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Cursor {
    pub row: u16,
    pub column: u16,
    pub pending_wrap: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Modes {
    pub auto_wrap: bool,
    pub origin: bool,
    pub insert: bool,
    pub new_line: bool,
    pub application_cursor: bool,
    pub application_keypad: bool,
    pub bracketed_paste: bool,
    pub cursor_visible: bool,
    pub cursor_style: u8,
    pub mouse_modes: u8,
    pub mouse_encodings: u8,
}
impl Default for Modes {
    fn default() -> Self {
        Self {
            auto_wrap: true,
            origin: false,
            insert: false,
            new_line: false,
            application_cursor: false,
            application_keypad: false,
            bracketed_paste: false,
            cursor_visible: true,
            cursor_style: 0,
            mouse_modes: 0,
            mouse_encodings: 0,
        }
    }
}
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Row {
    pub cells: Vec<Cell>,
    pub wrapped: bool,
}
impl Row {
    pub(crate) fn blank(columns: usize, attributes: Attributes) -> Self {
        Self {
            cells: vec![Cell::blank(attributes); columns],
            wrapped: false,
        }
    }
}
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Saved {
    pub cursor: Cursor,
    pub attributes: Attributes,
    pub origin: bool,
    pub auto_wrap: bool,
    pub charsets: [u8; 2],
    pub charset: u8,
}
impl Default for Saved {
    fn default() -> Self {
        Self {
            cursor: Cursor::default(),
            attributes: Attributes::default(),
            origin: false,
            auto_wrap: true,
            charsets: [0, 0],
            charset: 0,
        }
    }
}
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Screen {
    pub rows: Vec<Row>,
    pub cursor: Cursor,
    pub dec_saved: Saved,
    pub ansi_saved: Cursor,
    pub top: usize,
    pub bottom: usize,
    pub history: VecDeque<Row>,
    pub history_cells: usize,
}
impl Screen {
    pub(crate) fn new(columns: usize, rows: usize) -> Self {
        Self {
            rows: (0..rows)
                .map(|_| Row::blank(columns, Attributes::default()))
                .collect(),
            cursor: Cursor::default(),
            dec_saved: Saved::default(),
            ansi_saved: Cursor::default(),
            top: 0,
            bottom: rows - 1,
            history: VecDeque::new(),
            history_cells: 0,
        }
    }
    pub(super) fn push_history(&mut self, mut row: Row) {
        // Resize can leave large spare cell capacity behind a short row. Never
        // let many such rows bypass the retained-history cell quota.
        row.cells = row.cells.into_boxed_slice().into_vec();
        while self.history_cells + row.cells.len() > MAX_HISTORY_CELLS {
            let old = self
                .history
                .pop_front()
                .expect("bounded history accounting");
            self.history_cells -= old.cells.len();
        }
        self.history_cells += row.cells.len();
        self.history.push_back(row);
    }
}
pub struct ScreenView<'a> {
    pub(super) screen: &'a Screen,
    pub columns: usize,
    pub rows: usize,
    pub alternate: bool,
}
impl<'a> ScreenView<'a> {
    pub fn cursor(&self) -> Cursor {
        self.screen.cursor
    }
    pub fn cell(&self, row: usize, column: usize) -> Option<&'a Cell> {
        self.screen.rows.get(row).and_then(|r| r.cells.get(column))
    }
    pub fn line(&self, row: usize) -> Option<&'a [Cell]> {
        self.screen.rows.get(row).map(|r| r.cells.as_slice())
    }
    pub fn line_wrapped(&self, row: usize) -> Option<bool> {
        self.screen.rows.get(row).map(|r| r.wrapped)
    }
    pub fn history(&self) -> impl Iterator<Item = (&'a [Cell], bool)> {
        self.screen
            .history
            .iter()
            .map(|r| (r.cells.as_slice(), r.wrapped))
    }
}
