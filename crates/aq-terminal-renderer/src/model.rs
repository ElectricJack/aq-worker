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
    fn push_history(&mut self, mut row: Row) {
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
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct State {
    pub columns: usize,
    pub rows: usize,
    pub main: Screen,
    pub alternate: Screen,
    pub alternate_active: bool,
    pub attributes: Attributes,
    pub modes: Modes,
    pub tabs: Vec<bool>,
    pub title: String,
    pub icon_name: String,
    pub charsets: [u8; 2],
    pub charset: u8,
    pub last_print: Option<char>,
    pub bells: u64,
}
impl State {
    pub(crate) fn new(columns: usize, rows: usize) -> Self {
        Self {
            columns,
            rows,
            main: Screen::new(columns, rows),
            alternate: Screen::new(columns, rows),
            alternate_active: false,
            attributes: Attributes::default(),
            modes: Modes::default(),
            tabs: (0..columns).map(|c| c > 0 && c % 8 == 0).collect(),
            title: String::new(),
            icon_name: String::new(),
            charsets: [0, 0],
            charset: 0,
            last_print: None,
            bells: 0,
        }
    }
    pub(crate) fn active(&self) -> &Screen {
        if self.alternate_active {
            &self.alternate
        } else {
            &self.main
        }
    }
    pub(crate) fn active_mut(&mut self) -> &mut Screen {
        if self.alternate_active {
            &mut self.alternate
        } else {
            &mut self.main
        }
    }
    pub(crate) fn view(&self) -> ScreenView<'_> {
        ScreenView {
            screen: self.active(),
            columns: self.columns,
            rows: self.rows,
            alternate: self.alternate_active,
        }
    }
    pub(crate) fn clear_wrap(&mut self) {
        self.active_mut().cursor.pending_wrap = false;
    }
    pub(crate) fn home(&mut self) {
        let row = if self.modes.origin {
            self.active().top
        } else {
            0
        };
        self.active_mut().cursor = Cursor {
            row: row as u16,
            column: 0,
            pending_wrap: false,
        };
    }
    pub(crate) fn save_dec(&mut self) {
        let saved = Saved {
            cursor: self.active().cursor,
            attributes: self.attributes,
            origin: self.modes.origin,
            auto_wrap: self.modes.auto_wrap,
            charsets: self.charsets,
            charset: self.charset,
        };
        self.active_mut().dec_saved = saved;
    }
    pub(crate) fn restore_dec(&mut self) {
        let saved = self.active().dec_saved.clone();
        let mut cursor = saved.cursor;
        cursor.column = cursor.column.min((self.columns - 1) as u16);
        cursor.row = cursor.row.min((self.rows - 1) as u16);
        self.active_mut().cursor = cursor;
        self.attributes = saved.attributes;
        self.modes.origin = saved.origin;
        self.modes.auto_wrap = saved.auto_wrap;
        self.charsets = saved.charsets;
        self.charset = saved.charset;
    }
    pub(crate) fn alternate(&mut self, enabled: bool, mode: u16, work: &mut Work) -> Result<()> {
        if matches!(mode, 1047 | 1049) {
            work.spend(self.columns * self.rows)?;
        }
        if enabled {
            if mode == 1049 {
                self.save_dec();
            }
            if matches!(mode, 1047 | 1049) {
                self.alternate = Screen::new(self.columns, self.rows);
            }
            self.alternate_active = true;
        } else {
            self.alternate_active = false;
            if mode == 1049 {
                self.restore_dec();
            }
            if matches!(mode, 1047 | 1049) {
                self.alternate = Screen::new(self.columns, self.rows);
            }
        }
        Ok(())
    }
    pub(crate) fn linefeed(&mut self, carriage: bool, work: &mut Work) -> Result<()> {
        let cursor = self.active().cursor;
        if cursor.row as usize == self.active().bottom {
            self.scroll_up(1, work)?;
        } else {
            self.active_mut().cursor.row = (cursor.row as usize + 1).min(self.rows - 1) as u16;
        }
        self.clear_wrap();
        if carriage || self.modes.new_line {
            self.active_mut().cursor.column = 0;
        }
        Ok(())
    }
    pub(crate) fn reverse_index(&mut self, work: &mut Work) -> Result<()> {
        if self.active().cursor.row as usize == self.active().top {
            self.scroll_down(1, work)?;
        } else {
            self.active_mut().cursor.row = self.active().cursor.row.saturating_sub(1);
        }
        self.clear_wrap();
        Ok(())
    }
    pub(crate) fn scroll_up(&mut self, count: usize, work: &mut Work) -> Result<()> {
        let columns = self.columns;
        let attrs = self.attributes;
        let main = !self.alternate_active;
        let (top, bottom) = (self.active().top, self.active().bottom);
        let count = count.min(bottom - top + 1);
        work.spend(count * (columns + bottom - top + 1))?;
        let rows = self.rows;
        let screen = self.active_mut();
        for _ in 0..count {
            let removed = screen.rows.remove(top);
            screen.rows.insert(bottom, Row::blank(columns, attrs));
            if main && top == 0 && bottom == rows - 1 {
                screen.push_history(removed);
            }
        }
        Ok(())
    }
    pub(crate) fn scroll_down(&mut self, count: usize, work: &mut Work) -> Result<()> {
        let columns = self.columns;
        let attrs = self.attributes;
        let (top, bottom) = (self.active().top, self.active().bottom);
        let count = count.min(bottom - top + 1);
        work.spend(count * (columns + bottom - top + 1))?;
        let screen = self.active_mut();
        for _ in 0..count {
            screen.rows.remove(bottom);
            screen.rows.insert(top, Row::blank(columns, attrs));
        }
        Ok(())
    }
    pub(crate) fn print(&mut self, scalar: char, work: &mut Work) -> Result<()> {
        work.spend(1)?;
        let width = scalar_width(scalar)?;
        if width > self.columns {
            return Err(Error::Unsupported("wide scalar exceeds row"));
        }
        if width > 2 {
            return Err(Error::Unsupported("scalar width"));
        }
        if width == 0 {
            let cursor = self.active().cursor;
            let mut column = if cursor.pending_wrap {
                cursor.column as usize
            } else {
                (cursor.column as usize)
                    .checked_sub(1)
                    .ok_or(Error::Unsupported("orphan combining mark"))?
            };
            let row = &mut self.active_mut().rows[cursor.row as usize];
            if row.cells[column].width == 0 {
                column = column.checked_sub(1).ok_or(Error::Framing)?;
            }
            let cell = &mut row.cells[column];
            if cell.glyph.is_empty() {
                return Err(Error::Unsupported("orphan combining mark"));
            }
            if cell.glyph.len() + scalar.len_utf8() > MAX_GLYPH_BYTES {
                return Err(Error::Bound);
            }
            cell.glyph.push(scalar);
            return Ok(());
        }
        if self.active().cursor.pending_wrap {
            if self.modes.auto_wrap {
                let row = self.active().cursor.row as usize;
                self.active_mut().rows[row].wrapped = true;
                self.linefeed(true, work)?;
            } else {
                self.clear_wrap();
            }
        }
        if width == 2 && self.active().cursor.column as usize + 1 == self.columns {
            if !self.modes.auto_wrap {
                return Err(Error::Unsupported("wide scalar at nonwrapping edge"));
            }
            let row = self.active().cursor.row as usize;
            self.active_mut().rows[row].wrapped = true;
            self.linefeed(true, work)?;
        }
        if self.modes.insert {
            self.edit_cells(width, true, work)?;
        }
        let attrs = self.attributes;
        let columns = self.columns;
        let cursor = self.active().cursor;
        let col = cursor.column as usize;
        let row = &mut self.active_mut().rows[cursor.row as usize];
        clear_pair(row, col, attrs);
        if width == 2 {
            clear_pair(row, col + 1, attrs);
        }
        row.cells[col] = Cell {
            glyph: scalar.to_string(),
            width: width as u8,
            attributes: attrs,
        };
        if width == 2 {
            row.cells[col + 1] = Cell {
                glyph: String::new(),
                width: 0,
                attributes: attrs,
            };
        }
        let next = col + width;
        let cursor = &mut self.active_mut().cursor;
        cursor.column = next.min(columns - 1) as u16;
        cursor.pending_wrap = next == columns;
        self.last_print = Some(scalar);
        Ok(())
    }
    pub(crate) fn erase(
        &mut self,
        row: usize,
        start: usize,
        end: usize,
        work: &mut Work,
    ) -> Result<()> {
        let attrs = self.attributes;
        work.spend(end - start + self.columns)?;
        let row = &mut self.active_mut().rows[row];
        for column in start..end {
            clear_pair(row, column, attrs);
            row.cells[column] = Cell::blank(attrs);
        }
        if start == 0 && end == row.cells.len() {
            row.wrapped = false;
        }
        repair(row, attrs);
        Ok(())
    }
    pub(crate) fn edit_cells(&mut self, count: usize, insert: bool, work: &mut Work) -> Result<()> {
        let columns = self.columns;
        let attrs = self.attributes;
        let cursor = self.active().cursor;
        let col = cursor.column as usize;
        let count = count.min(columns - col);
        work.spend(columns * 2)?;
        let row = &mut self.active_mut().rows[cursor.row as usize];
        if insert {
            for at in (col + count..columns).rev() {
                row.cells[at] = row.cells[at - count].clone();
            }
            for at in col..col + count {
                row.cells[at] = Cell::blank(attrs);
            }
        } else {
            for at in col..columns - count {
                row.cells[at] = row.cells[at + count].clone();
            }
            for at in columns - count..columns {
                row.cells[at] = Cell::blank(attrs);
            }
        }
        row.wrapped = false;
        repair(row, attrs);
        self.clear_wrap();
        Ok(())
    }
    pub(crate) fn edit_lines(&mut self, count: usize, insert: bool, work: &mut Work) -> Result<()> {
        let (top, bottom) = (self.active().top, self.active().bottom);
        let cursor = self.active().cursor.row as usize;
        if cursor < top || cursor > bottom {
            return Ok(());
        }
        let count = count.min(bottom - cursor + 1);
        let columns = self.columns;
        let attrs = self.attributes;
        work.spend(count * (columns + bottom - cursor + 1))?;
        let screen = self.active_mut();
        for _ in 0..count {
            if insert {
                screen.rows.remove(bottom);
                screen.rows.insert(cursor, Row::blank(columns, attrs));
            } else {
                screen.rows.remove(cursor);
                screen.rows.insert(bottom, Row::blank(columns, attrs));
            }
        }
        self.clear_wrap();
        Ok(())
    }
    pub(crate) fn resize(&mut self, columns: usize, rows: usize) -> Result<()> {
        dimensions(columns, rows)?;
        for screen in [&mut self.main, &mut self.alternate] {
            screen.rows.truncate(rows);
            for row in &mut screen.rows {
                row.cells.resize(columns, Cell::blank(self.attributes));
                row.cells.truncate(columns);
                row.wrapped = false;
                repair(row, self.attributes);
                row.cells = std::mem::take(&mut row.cells).into_boxed_slice().into_vec();
            }
            while screen.rows.len() < rows {
                screen.rows.push(Row::blank(columns, self.attributes));
            }
            screen.rows = std::mem::take(&mut screen.rows)
                .into_boxed_slice()
                .into_vec();
            screen.cursor.row = screen.cursor.row.min((rows - 1) as u16);
            screen.cursor.column = screen.cursor.column.min((columns - 1) as u16);
            screen.cursor.pending_wrap = false;
            for saved in [&mut screen.dec_saved.cursor, &mut screen.ansi_saved] {
                saved.row = saved.row.min((rows - 1) as u16);
                saved.column = saved.column.min((columns - 1) as u16);
                saved.pending_wrap = false;
            }
            screen.top = 0;
            screen.bottom = rows - 1;
        }
        let old = self.tabs.len();
        self.tabs.resize(columns, false);
        for col in old..columns {
            self.tabs[col] = col > 0 && col % 8 == 0;
        }
        self.columns = columns;
        self.rows = rows;
        Ok(())
    }
}
fn clear_pair(row: &mut Row, column: usize, attrs: Attributes) {
    if row.cells[column].width == 0 && column > 0 {
        row.cells[column - 1] = Cell::blank(attrs);
    }
    if row.cells[column].width == 2 && column + 1 < row.cells.len() {
        row.cells[column + 1] = Cell::blank(attrs);
    }
}
pub(crate) fn repair(row: &mut Row, attrs: Attributes) {
    for col in 0..row.cells.len() {
        if (row.cells[col].width == 2
            && (col + 1 == row.cells.len() || row.cells[col + 1].width != 0))
            || (row.cells[col].width == 0 && (col == 0 || row.cells[col - 1].width != 2))
        {
            row.cells[col] = Cell::blank(attrs);
        }
    }
}
pub struct ScreenView<'a> {
    screen: &'a Screen,
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn resized_rows_cannot_accumulate_hidden_spare_capacity_in_history() {
        let mut state = State::new(240, 50);
        for _ in 0..64 {
            state.resize(240, 50).unwrap();
            state.resize(1, 120).unwrap();
            state.main.cursor.row = 119;
            let mut work = Work(MAX_WORK);
            for _ in 0..120 {
                state.linefeed(false, &mut work).unwrap();
            }
        }
        assert_eq!(state.main.history_cells, 64 * 120);
        assert_eq!(
            state
                .main
                .history
                .iter()
                .map(|r| r.cells.capacity())
                .sum::<usize>(),
            state.main.history_cells
        );
    }
}
