use crate::*;
mod screen;
pub use screen::{Attributes, Cell, Color, Cursor, Modes, ScreenView};
pub(crate) use screen::{Row, Saved, Screen};
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
        let retain_history = main && (top, bottom) == (0, self.rows - 1);
        let screen = self.active_mut();
        for _ in 0..count {
            let removed = screen.rows.remove(top);
            screen.rows.insert(bottom, Row::blank(columns, attrs));
            if retain_history {
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
            return self.combine(scalar);
        }
        self.wrap_before_print(width, work)?;
        if self.modes.insert {
            self.edit_cells(width, true, work)?;
        }
        self.place_glyph(scalar, width);
        Ok(())
    }

    fn combine(&mut self, scalar: char) -> Result<()> {
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
        // tmux3.4 does not repeat the base after a combining scalar. Until
        // all such REP semantics are qualified, refuse rather than invent
        // repetitions from a stale last-print base.
        self.last_print = None;
        Ok(())
    }

    fn wrap_before_print(&mut self, width: usize, work: &mut Work) -> Result<()> {
        match (self.active().cursor.pending_wrap, self.modes.auto_wrap) {
            (true, true) => self.wrap_to_next_line(work)?,
            (true, false) => self.clear_wrap(),
            _ => (),
        }
        if width != 2 || self.active().cursor.column as usize + 1 != self.columns {
            return Ok(());
        }
        if !self.modes.auto_wrap {
            return Err(Error::Unsupported("wide scalar at nonwrapping edge"));
        }
        self.wrap_to_next_line(work)
    }

    fn wrap_to_next_line(&mut self, work: &mut Work) -> Result<()> {
        let row = self.active().cursor.row as usize;
        self.active_mut().rows[row].wrapped = true;
        self.linefeed(true, work)
    }

    fn place_glyph(&mut self, scalar: char, width: usize) {
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
    }
    pub(crate) fn erase(
        &mut self,
        row: usize,
        columns: std::ops::Range<usize>,
        work: &mut Work,
    ) -> Result<()> {
        let attrs = self.attributes;
        let std::ops::Range { start, end } = columns;
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
        let missing_pair = match row.cells[col].width {
            2 => row.cells.get(col + 1).is_none_or(|next| next.width != 0),
            0 => col
                .checked_sub(1)
                .is_none_or(|previous| row.cells[previous].width != 2),
            _ => false,
        };
        if missing_pair {
            row.cells[col] = Cell::blank(attrs);
        }
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
