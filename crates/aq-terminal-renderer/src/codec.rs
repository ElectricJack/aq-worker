use crate::*;
use model::{Row, Saved, Screen};
use parser::Mode;
use std::collections::VecDeque;
const MAGIC: &[u8] = b"AQRI\x01";
const WIDTH: &[u8] = b"unicode-width=0.2.2;ambiguous=1;compound=refuse";
fn require_image(condition: bool) -> Result<()> {
    condition.then_some(()).ok_or(Error::Image)
}

fn validate_modes(modes: &Modes) -> Result<()> {
    require_image(modes.cursor_style <= 6 && modes.mouse_modes <= 7 && modes.mouse_encodings <= 3)
}
struct Writer(Vec<u8>);
impl Writer {
    fn parser(&mut self, parser: &Parser) -> Result<()> {
        match &parser.mode {
            Mode::Ground => self.byte(0)?,
            Mode::Escape => self.byte(1)?,
            Mode::Charset(slot) => {
                if *slot > 1 {
                    return Err(Error::Image);
                }
                self.put(&[2, *slot])?;
            }
            Mode::Csi(bytes) => {
                if bytes.iter().any(|b| !(0x20..=0x3f).contains(b)) {
                    return Err(Error::Image);
                }
                self.byte(3)?;
                self.blob(bytes, MAX_CSI_BYTES)?;
            }
            Mode::Osc { bytes, escaped } => {
                if bytes.iter().any(|b| *b < 0x20 || *b == 0x7f) {
                    return Err(Error::Image);
                }
                self.byte(4)?;
                self.flag(*escaped)?;
                self.blob(bytes, MAX_CONTROL_BYTES)?;
            }
        }
        validate_utf8_pending(parser)?;
        self.blob(&parser.utf8, 3)?;
        Ok(())
    }
    fn modes(&mut self, modes: &Modes) -> Result<()> {
        for bit in [
            modes.auto_wrap,
            modes.origin,
            modes.insert,
            modes.new_line,
            modes.application_cursor,
            modes.application_keypad,
            modes.bracketed_paste,
            modes.cursor_visible,
        ] {
            self.flag(bit)?;
        }
        validate_modes(modes)?;
        self.put(&[modes.cursor_style, modes.mouse_modes, modes.mouse_encodings])?;
        Ok(())
    }
    fn last_print(&mut self, last_print: Option<char>) -> Result<()> {
        self.flag(last_print.is_some())?;
        if let Some(c) = last_print {
            if !matches!(scalar_width(c), Ok(1 | 2)) {
                return Err(Error::Image);
            }
            self.put(&(c as u32).to_le_bytes())?;
        }
        Ok(())
    }
    fn put(&mut self, bytes: &[u8]) -> Result<()> {
        if self
            .0
            .len()
            .checked_add(bytes.len())
            .is_none_or(|n| n > MAX_IMAGE_BYTES)
        {
            return Err(Error::Bound);
        }
        self.0.extend_from_slice(bytes);
        Ok(())
    }
    fn byte(&mut self, b: u8) -> Result<()> {
        self.put(&[b])
    }
    fn flag(&mut self, b: bool) -> Result<()> {
        self.byte(u8::from(b))
    }
    fn short(&mut self, n: usize) -> Result<()> {
        self.put(&u16::try_from(n).map_err(|_| Error::Bound)?.to_le_bytes())
    }
    fn long(&mut self, n: u64) -> Result<()> {
        self.put(&n.to_le_bytes())
    }
    fn blob(&mut self, b: &[u8], max: usize) -> Result<()> {
        if b.len() > max {
            return Err(Error::Bound);
        }
        self.short(b.len())?;
        self.put(b)
    }
    fn color(&mut self, color: Color) -> Result<()> {
        match color {
            Color::Default => self.byte(0),
            Color::Indexed(i) => {
                self.byte(1)?;
                self.byte(i)
            }
            Color::Rgb(r, g, b) => {
                self.byte(2)?;
                self.put(&[r, g, b])
            }
        }
    }
    fn attrs(&mut self, attrs: Attributes) -> Result<()> {
        if !valid_flags(attrs.flags as usize) {
            return Err(Error::Image);
        }
        self.color(attrs.foreground)?;
        self.color(attrs.background)?;
        self.short(attrs.flags as usize)
    }
    fn cursor(&mut self, cursor: Cursor, columns: usize, rows: usize) -> Result<()> {
        validate_cursor(cursor, columns, rows)?;
        self.short(cursor.row as usize)?;
        self.short(cursor.column as usize)?;
        self.flag(cursor.pending_wrap)
    }
    fn saved(&mut self, saved: &Saved, columns: usize, rows: usize) -> Result<()> {
        self.cursor(saved.cursor, columns, rows)?;
        self.attrs(saved.attributes)?;
        self.flag(saved.origin)?;
        self.flag(saved.auto_wrap)?;
        self.charsets(saved.charsets, saved.charset)
    }
    fn charsets(&mut self, sets: [u8; 2], active: u8) -> Result<()> {
        if sets.iter().any(|v| *v > 1) || active > 1 {
            return Err(Error::Image);
        }
        self.put(&[sets[0], sets[1], active])
    }
    fn cell(&mut self, cell: &Cell) -> Result<()> {
        self.byte(cell.width)?;
        self.blob(cell.glyph.as_bytes(), MAX_GLYPH_BYTES)?;
        self.attrs(cell.attributes)
    }
    fn row(&mut self, row: &Row) -> Result<()> {
        validate_row(row)?;
        self.short(row.cells.len())?;
        self.flag(row.wrapped)?;
        let mut runs: Vec<(usize, &Cell)> = Vec::new();
        for cell in &row.cells {
            if let Some((count, old)) = runs.last_mut()
                && *old == cell
            {
                *count += 1;
                continue;
            }
            runs.push((1, cell));
        }
        self.short(runs.len())?;
        for (count, cell) in runs {
            self.short(count)?;
            self.cell(cell)?;
        }
        Ok(())
    }
    fn screen(
        &mut self,
        screen: &Screen,
        size: (usize, usize),
        history_limit: usize,
    ) -> Result<()> {
        let (columns, rows) = size;
        require_image(
            screen.rows.len() == rows && screen.rows.iter().all(|r| r.cells.len() == columns),
        )?;
        require_image((screen.top..rows).contains(&screen.bottom))?;
        self.cursor(screen.cursor, columns, rows)?;
        self.saved(&screen.dec_saved, columns, rows)?;
        self.cursor(screen.ansi_saved, columns, rows)?;
        self.short(screen.top)?;
        self.short(screen.bottom)?;
        self.short(screen.rows.len())?;
        for row in &screen.rows {
            self.row(row)?;
        }
        let actual = screen.history.iter().map(|r| r.cells.len()).sum::<usize>();
        if actual != screen.history_cells || actual > history_limit {
            return Err(Error::Image);
        }
        self.short(screen.history.len())?;
        for row in &screen.history {
            self.row(row)?;
        }
        Ok(())
    }
}
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}
impl<'a> Reader<'a> {
    fn parser(&mut self) -> Result<Parser> {
        let mode = match self.byte()? {
            0 => Mode::Ground,
            1 => Mode::Escape,
            2 => {
                let slot = self.byte()?;
                if slot > 1 {
                    return Err(Error::Image);
                }
                Mode::Charset(slot)
            }
            3 => {
                let b = self.blob(MAX_CSI_BYTES)?;
                if b.iter().any(|v| !(0x20..=0x3f).contains(v)) {
                    return Err(Error::Image);
                }
                Mode::Csi(b.to_vec())
            }
            4 => {
                let escaped = self.flag()?;
                let b = self.blob(MAX_CONTROL_BYTES)?;
                if b.iter().any(|v| *v < 0x20 || *v == 0x7f) {
                    return Err(Error::Image);
                }
                Mode::Osc {
                    bytes: b.to_vec(),
                    escaped,
                }
            }
            _ => return Err(Error::Image),
        };
        let parser = Parser {
            mode,
            utf8: self.blob(3)?.to_vec(),
        };
        validate_utf8_pending(&parser)?;
        Ok(parser)
    }
    fn modes(&mut self) -> Result<Modes> {
        let modes = Modes {
            auto_wrap: self.flag()?,
            origin: self.flag()?,
            insert: self.flag()?,
            new_line: self.flag()?,
            application_cursor: self.flag()?,
            application_keypad: self.flag()?,
            bracketed_paste: self.flag()?,
            cursor_visible: self.flag()?,
            cursor_style: self.byte()?,
            mouse_modes: self.byte()?,
            mouse_encodings: self.byte()?,
        };
        validate_modes(&modes)?;
        Ok(modes)
    }
    fn last_print(&mut self) -> Result<Option<char>> {
        let last_print = if self.flag()? {
            let c = char::from_u32(u32::from_le_bytes(
                self.take(4)?.try_into().map_err(|_| Error::Image)?,
            ))
            .ok_or(Error::Image)?;
            if !matches!(scalar_width(c), Ok(1 | 2)) {
                return Err(Error::Image);
            }
            Some(c)
        } else {
            None
        };
        Ok(last_print)
    }
    fn title(&mut self) -> Result<String> {
        let title = self.text(512)?;
        if title.chars().any(char::is_control) {
            return Err(Error::Image);
        }
        Ok(title)
    }
    fn state(&mut self) -> Result<State> {
        let columns = self.short()?;
        let rows = self.short()?;
        dimensions(columns, rows).map_err(|_| Error::Image)?;
        let alternate_active = self.flag()?;
        let attributes = self.attrs()?;
        let modes = self.modes()?;
        let mut tabs = Vec::with_capacity(columns);
        for _ in 0..columns {
            tabs.push(self.flag()?);
        }
        let title = self.title()?;
        let icon_name = self.title()?;
        let (charsets, charset) = self.charsets()?;
        let last_print = self.last_print()?;
        let bells = self.long()?;
        let main = self.screen(columns, rows, true)?;
        let alternate = self.screen(columns, rows, false)?;
        Ok(State {
            columns,
            rows,
            main,
            alternate,
            alternate_active,
            attributes,
            modes,
            tabs,
            title,
            icon_name,
            charsets,
            charset,
            last_print,
            bells,
        })
    }
    fn header(&mut self, terminal: &TerminalIdentity) -> Result<()> {
        require_image(self.take(MAGIC.len())? == MAGIC)?;
        require_image(self.blob(64)? == PROFILE.as_bytes())?;
        require_image(self.blob(128)? == WIDTH)?;
        let digest = self.take(64)?;
        if digest
            != canonical_digest(terminal)
                .map_err(|_| Error::Image)?
                .as_str()
                .as_bytes()
        {
            return Err(Error::Image);
        }
        Ok(())
    }
    fn expand_cells(&mut self, count: usize, columns: usize) -> Result<Vec<Cell>> {
        let mut cells: Vec<Cell> = Vec::with_capacity(columns);
        for _ in 0..count {
            let run = self.short()?;
            if run == 0 || run > columns - cells.len() {
                return Err(Error::Image);
            }
            let cell = self.cell()?;
            if cells.last().is_some_and(|old| *old == cell) {
                return Err(Error::Image);
            }
            cells.extend(std::iter::repeat_n(cell, run));
        }
        if cells.len() != columns {
            return Err(Error::Image);
        }
        Ok(cells)
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.at.checked_add(n).ok_or(Error::Image)?;
        let bytes = self.bytes.get(self.at..end).ok_or(Error::Image)?;
        self.at = end;
        Ok(bytes)
    }
    fn byte(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn flag(&mut self) -> Result<bool> {
        match self.byte()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(Error::Image),
        }
    }
    fn short(&mut self) -> Result<usize> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().map_err(|_| Error::Image)?) as usize)
    }
    fn long(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(
            self.take(8)?.try_into().map_err(|_| Error::Image)?,
        ))
    }
    fn blob(&mut self, max: usize) -> Result<&'a [u8]> {
        let n = self.short()?;
        if n > max {
            return Err(Error::Image);
        }
        self.take(n)
    }
    fn text(&mut self, max: usize) -> Result<String> {
        Ok(std::str::from_utf8(self.blob(max)?)
            .map_err(|_| Error::Image)?
            .into())
    }
    fn color(&mut self) -> Result<Color> {
        match self.byte()? {
            0 => Ok(Color::Default),
            1 => Ok(Color::Indexed(self.byte()?)),
            2 => Ok(Color::Rgb(self.byte()?, self.byte()?, self.byte()?)),
            _ => Err(Error::Image),
        }
    }
    fn attrs(&mut self) -> Result<Attributes> {
        let foreground = self.color()?;
        let background = self.color()?;
        let flags = self.short()?;
        if !valid_flags(flags) {
            return Err(Error::Image);
        }
        Ok(Attributes {
            foreground,
            background,
            flags: flags as u16,
        })
    }
    fn cursor(&mut self, columns: usize, rows: usize) -> Result<Cursor> {
        let cursor = Cursor {
            row: self.short()? as u16,
            column: self.short()? as u16,
            pending_wrap: self.flag()?,
        };
        validate_cursor(cursor, columns, rows)?;
        Ok(cursor)
    }
    fn charsets(&mut self) -> Result<([u8; 2], u8)> {
        let a = self.byte()?;
        let b = self.byte()?;
        let c = self.byte()?;
        if [a, b, c].into_iter().any(|value| value > 1) {
            return Err(Error::Image);
        }
        Ok(([a, b], c))
    }
    fn saved(&mut self, columns: usize, rows: usize) -> Result<Saved> {
        let cursor = self.cursor(columns, rows)?;
        let attributes = self.attrs()?;
        let origin = self.flag()?;
        let auto_wrap = self.flag()?;
        let (charsets, charset) = self.charsets()?;
        Ok(Saved {
            cursor,
            attributes,
            origin,
            auto_wrap,
            charsets,
            charset,
        })
    }
    fn cell(&mut self) -> Result<Cell> {
        let width = self.byte()?;
        let glyph = self.text(MAX_GLYPH_BYTES)?;
        let attributes = self.attrs()?;
        Ok(Cell {
            glyph,
            width,
            attributes,
        })
    }
    fn row(
        &mut self,
        expected_columns: Option<usize>,
        remaining_history: Option<usize>,
    ) -> Result<Row> {
        let columns = self.short()?;
        require_image((1..=MAX_COLUMNS).contains(&columns))?;
        require_image(expected_columns.is_none_or(|c| c == columns))?;
        require_image(remaining_history.is_none_or(|remaining| columns <= remaining))?;
        let wrapped = self.flag()?;
        let count = self.short()?;
        if count == 0 || count > columns {
            return Err(Error::Image);
        }
        let cells = self.expand_cells(count, columns)?;
        let row = Row { cells, wrapped };
        validate_row(&row)?;
        Ok(row)
    }
    fn history(&mut self, limit: usize) -> Result<(VecDeque<Row>, usize)> {
        let count = self.short()?;
        if count > limit {
            return Err(Error::Image);
        }
        let mut old = VecDeque::new();
        let mut history_cells = 0;
        for _ in 0..count {
            let row = self.row(None, Some(limit - history_cells))?;
            history_cells += row.cells.len();
            old.push_back(row);
        }
        Ok((old, history_cells))
    }

    fn screen(&mut self, columns: usize, rows: usize, history: bool) -> Result<Screen> {
        let cursor = self.cursor(columns, rows)?;
        let dec_saved = self.saved(columns, rows)?;
        let ansi_saved = self.cursor(columns, rows)?;
        let top = self.short()?;
        let bottom = self.short()?;
        let count = self.short()?;
        if !(top..rows).contains(&bottom) || count != rows {
            return Err(Error::Image);
        }
        let mut grid = Vec::with_capacity(rows);
        for _ in 0..rows {
            grid.push(self.row(Some(columns), None)?);
        }
        let limit = if history { MAX_HISTORY_CELLS } else { 0 };
        let (old, history_cells) = self.history(limit)?;
        Ok(Screen {
            rows: grid,
            cursor,
            dec_saved,
            ansi_saved,
            top,
            bottom,
            history: old,
            history_cells,
        })
    }
}
pub(crate) fn encode(renderer: &Renderer) -> Result<StateImage> {
    let s = &renderer.state;
    dimensions(s.columns, s.rows)?;
    let mut w = Writer(Vec::new());
    w.put(MAGIC)?;
    w.blob(PROFILE.as_bytes(), 64)?;
    w.blob(WIDTH, 128)?;
    w.put(
        canonical_digest(&renderer.terminal)
            .map_err(|_| Error::Image)?
            .as_str()
            .as_bytes(),
    )?;
    w.long(renderer.next_ordinal)?;
    w.long(renderer.next_output)?;
    w.long(renderer.resize_generation)?;
    w.short(s.columns)?;
    w.short(s.rows)?;
    w.flag(s.alternate_active)?;
    w.attrs(s.attributes)?;
    w.modes(&s.modes)?;
    if s.tabs.len() != s.columns {
        return Err(Error::Image);
    }
    for bit in &s.tabs {
        w.flag(*bit)?;
    }
    w.blob(s.title.as_bytes(), 512)?;
    w.blob(s.icon_name.as_bytes(), 512)?;
    w.charsets(s.charsets, s.charset)?;
    w.last_print(s.last_print)?;
    w.long(s.bells)?;
    w.screen(&s.main, (s.columns, s.rows), MAX_HISTORY_CELLS)?;
    w.screen(&s.alternate, (s.columns, s.rows), 0)?;
    w.parser(&renderer.parser)?;
    let digest = Digest::of(&w.0);
    Ok(StateImage { bytes: w.0, digest })
}
pub(crate) fn decode(terminal: &TerminalIdentity, bytes: &[u8]) -> Result<Renderer> {
    terminal.validate().map_err(|_| Error::Image)?;
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(Error::Image);
    }
    let mut r = Reader { bytes, at: 0 };
    r.header(terminal)?;
    let next_ordinal = r.long()?;
    let next_output = r.long()?;
    let resize_generation = r.long()?;
    if next_output == 0 || next_ordinal.checked_sub(next_output) != Some(resize_generation) {
        return Err(Error::Image);
    }
    let state = r.state()?;
    let parser = r.parser()?;
    if r.at != bytes.len() {
        return Err(Error::Image);
    }
    let renderer = Renderer {
        terminal: terminal.clone(),
        state,
        parser,
        next_ordinal,
        next_output,
        resize_generation,
        consistent: true,
    };
    if encode(&renderer).map_err(|_| Error::Image)?.bytes != bytes {
        return Err(Error::Image);
    }
    Ok(renderer)
}
fn validate_cursor(cursor: Cursor, columns: usize, rows: usize) -> Result<()> {
    require_image((cursor.row as usize) < rows && (cursor.column as usize) < columns)?;
    require_image(!cursor.pending_wrap || cursor.column as usize + 1 == columns)
}
fn validate_row(row: &Row) -> Result<()> {
    if !(1..=MAX_COLUMNS).contains(&row.cells.len()) {
        return Err(Error::Image);
    }
    for (at, cell) in row.cells.iter().enumerate() {
        validate_cell(row, at, cell)?;
    }
    Ok(())
}

fn validate_cell(row: &Row, at: usize, cell: &Cell) -> Result<()> {
    if cell.glyph.len() > MAX_GLYPH_BYTES || !valid_flags(cell.attributes.flags as usize) {
        return Err(Error::Image);
    }
    match cell.width {
        0 => validate_continuation(cell, at.checked_sub(1).map(|i| &row.cells[i]))?,
        2 if row.cells.get(at + 1).is_none_or(|next| next.width != 0) => {
            return Err(Error::Image);
        }
        1 | 2 => validate_glyph(cell)?,
        _ => return Err(Error::Image),
    }

    Ok(())
}

fn validate_continuation(cell: &Cell, previous: Option<&Cell>) -> Result<()> {
    let lead = previous.ok_or(Error::Image)?;
    require_image(cell.glyph.is_empty() && lead.width == 2 && lead.attributes == cell.attributes)
}

fn validate_glyph(cell: &Cell) -> Result<()> {
    let mut chars = cell.glyph.chars();
    let width = match chars.next() {
        Some(c) => scalar_width(c).map_err(|_| Error::Image)?,
        None => 1,
    };
    if width == cell.width as usize && chars.all(|c| scalar_width(c) == Ok(0)) {
        Ok(())
    } else {
        Err(Error::Image)
    }
}

fn valid_flags(flags: usize) -> bool {
    flags <= 511 && flags & (8 | 256) != (8 | 256)
}

fn validate_utf8_pending(parser: &Parser) -> Result<()> {
    if parser.utf8.is_empty() {
        return Ok(());
    }
    if !matches!(parser.mode, Mode::Ground) || parser.utf8.len() > 3 {
        return Err(Error::Image);
    }
    match std::str::from_utf8(&parser.utf8) {
        Err(e) if e.error_len().is_none() && e.valid_up_to() == 0 => Ok(()),
        _ => Err(Error::Image),
    }
}
