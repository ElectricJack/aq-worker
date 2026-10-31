use crate::*;
use model::{Row, Saved, Screen};
use parser::Mode;
use std::collections::VecDeque;
const MAGIC: &[u8] = b"AQRI\x01";
const WIDTH: &[u8] = b"unicode-width=0.2.2;ambiguous=1;compound=refuse";
struct Writer(Vec<u8>);
impl Writer {
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
        if attrs.flags > 511 || attrs.flags & (8 | 256) == (8 | 256) {
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
        columns: usize,
        rows: usize,
        allow_history: bool,
    ) -> Result<()> {
        if screen.rows.len() != rows
            || screen.rows.iter().any(|r| r.cells.len() != columns)
            || screen.top > screen.bottom
            || screen.bottom >= rows
        {
            return Err(Error::Image);
        }
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
        if actual != screen.history_cells
            || actual > MAX_HISTORY_CELLS
            || (!allow_history && actual != 0)
        {
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
        if flags > 511 || flags & (8 | 256) == (8 | 256) {
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
        if a > 1 || b > 1 || c > 1 {
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
        if columns == 0
            || columns > MAX_COLUMNS
            || expected_columns.is_some_and(|c| c != columns)
            || remaining_history.is_some_and(|n| columns > n)
        {
            return Err(Error::Image);
        }
        let wrapped = self.flag()?;
        let count = self.short()?;
        if count == 0 || count > columns {
            return Err(Error::Image);
        }
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
        let row = Row { cells, wrapped };
        validate_row(&row)?;
        Ok(row)
    }
    fn screen(&mut self, columns: usize, rows: usize, history: bool) -> Result<Screen> {
        let cursor = self.cursor(columns, rows)?;
        let dec_saved = self.saved(columns, rows)?;
        let ansi_saved = self.cursor(columns, rows)?;
        let top = self.short()?;
        let bottom = self.short()?;
        let count = self.short()?;
        if top > bottom || bottom >= rows || count != rows {
            return Err(Error::Image);
        }
        let mut grid = Vec::with_capacity(rows);
        for _ in 0..rows {
            grid.push(self.row(Some(columns), None)?);
        }
        let count = self.short()?;
        if count > MAX_HISTORY_CELLS || (!history && count != 0) {
            return Err(Error::Image);
        }
        let mut old = VecDeque::new();
        let mut history_cells = 0;
        for _ in 0..count {
            let row = self.row(None, Some(MAX_HISTORY_CELLS - history_cells))?;
            history_cells += row.cells.len();
            old.push_back(row);
        }
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
    for bit in [
        s.modes.auto_wrap,
        s.modes.origin,
        s.modes.insert,
        s.modes.new_line,
        s.modes.application_cursor,
        s.modes.application_keypad,
        s.modes.bracketed_paste,
        s.modes.cursor_visible,
    ] {
        w.flag(bit)?;
    }
    if s.modes.cursor_style > 6 || s.modes.mouse_modes > 7 || s.modes.mouse_encodings > 3 {
        return Err(Error::Image);
    }
    w.put(&[
        s.modes.cursor_style,
        s.modes.mouse_modes,
        s.modes.mouse_encodings,
    ])?;
    if s.tabs.len() != s.columns {
        return Err(Error::Image);
    }
    for bit in &s.tabs {
        w.flag(*bit)?;
    }
    w.blob(s.title.as_bytes(), 512)?;
    w.blob(s.icon_name.as_bytes(), 512)?;
    w.charsets(s.charsets, s.charset)?;
    w.flag(s.last_print.is_some())?;
    if let Some(c) = s.last_print {
        if !matches!(scalar_width(c), Ok(1 | 2)) {
            return Err(Error::Image);
        }
        w.put(&(c as u32).to_le_bytes())?;
    }
    w.long(s.bells)?;
    w.screen(&s.main, s.columns, s.rows, true)?;
    w.screen(&s.alternate, s.columns, s.rows, false)?;
    match &renderer.parser.mode {
        Mode::Ground => w.byte(0)?,
        Mode::Escape => w.byte(1)?,
        Mode::Charset(slot) => {
            if *slot > 1 {
                return Err(Error::Image);
            }
            w.put(&[2, *slot])?;
        }
        Mode::Csi(bytes) => {
            if bytes.iter().any(|b| !(0x20..=0x3f).contains(b)) {
                return Err(Error::Image);
            }
            w.byte(3)?;
            w.blob(bytes, MAX_CSI_BYTES)?;
        }
        Mode::Osc { bytes, escaped } => {
            if bytes.iter().any(|b| *b < 0x20 || *b == 0x7f) {
                return Err(Error::Image);
            }
            w.byte(4)?;
            w.flag(*escaped)?;
            w.blob(bytes, MAX_CONTROL_BYTES)?;
        }
    }
    validate_utf8_pending(&renderer.parser)?;
    w.blob(&renderer.parser.utf8, 3)?;
    let digest = Digest::of(&w.0);
    Ok(StateImage { bytes: w.0, digest })
}
pub(crate) fn decode(terminal: &TerminalIdentity, bytes: &[u8]) -> Result<Renderer> {
    terminal.validate().map_err(|_| Error::Image)?;
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(Error::Image);
    }
    let mut r = Reader { bytes, at: 0 };
    if r.take(MAGIC.len())? != MAGIC
        || r.blob(64)? != PROFILE.as_bytes()
        || r.blob(128)? != WIDTH
        || r.take(64)?
            != canonical_digest(terminal)
                .map_err(|_| Error::Image)?
                .as_str()
                .as_bytes()
    {
        return Err(Error::Image);
    }
    let next_ordinal = r.long()?;
    let next_output = r.long()?;
    let resize_generation = r.long()?;
    if next_output == 0 || next_ordinal.checked_sub(next_output) != Some(resize_generation) {
        return Err(Error::Image);
    }
    let columns = r.short()?;
    let rows = r.short()?;
    dimensions(columns, rows).map_err(|_| Error::Image)?;
    let alternate_active = r.flag()?;
    let attributes = r.attrs()?;
    let modes = Modes {
        auto_wrap: r.flag()?,
        origin: r.flag()?,
        insert: r.flag()?,
        new_line: r.flag()?,
        application_cursor: r.flag()?,
        application_keypad: r.flag()?,
        bracketed_paste: r.flag()?,
        cursor_visible: r.flag()?,
        cursor_style: r.byte()?,
        mouse_modes: r.byte()?,
        mouse_encodings: r.byte()?,
    };
    if modes.cursor_style > 6 || modes.mouse_modes > 7 || modes.mouse_encodings > 3 {
        return Err(Error::Image);
    }
    let mut tabs = Vec::with_capacity(columns);
    for _ in 0..columns {
        tabs.push(r.flag()?);
    }
    let title = r.text(512)?;
    if title.chars().any(char::is_control) {
        return Err(Error::Image);
    }
    let icon_name = r.text(512)?;
    if icon_name.chars().any(char::is_control) {
        return Err(Error::Image);
    }
    let (charsets, charset) = r.charsets()?;
    let last_print = if r.flag()? {
        let c = char::from_u32(u32::from_le_bytes(
            r.take(4)?.try_into().map_err(|_| Error::Image)?,
        ))
        .ok_or(Error::Image)?;
        if !matches!(scalar_width(c), Ok(1 | 2)) {
            return Err(Error::Image);
        }
        Some(c)
    } else {
        None
    };
    let bells = r.long()?;
    let main = r.screen(columns, rows, true)?;
    let alternate = r.screen(columns, rows, false)?;
    let mode = match r.byte()? {
        0 => Mode::Ground,
        1 => Mode::Escape,
        2 => {
            let slot = r.byte()?;
            if slot > 1 {
                return Err(Error::Image);
            }
            Mode::Charset(slot)
        }
        3 => {
            let b = r.blob(MAX_CSI_BYTES)?;
            if b.iter().any(|v| !(0x20..=0x3f).contains(v)) {
                return Err(Error::Image);
            }
            Mode::Csi(b.to_vec())
        }
        4 => {
            let escaped = r.flag()?;
            let b = r.blob(MAX_CONTROL_BYTES)?;
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
        utf8: r.blob(3)?.to_vec(),
    };
    validate_utf8_pending(&parser)?;
    if r.at != bytes.len() {
        return Err(Error::Image);
    }
    let renderer = Renderer {
        terminal: terminal.clone(),
        state: State {
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
        },
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
    if cursor.row as usize >= rows
        || cursor.column as usize >= columns
        || (cursor.pending_wrap && cursor.column as usize + 1 != columns)
    {
        return Err(Error::Image);
    }
    Ok(())
}
fn validate_row(row: &Row) -> Result<()> {
    if row.cells.is_empty() || row.cells.len() > MAX_COLUMNS {
        return Err(Error::Image);
    }
    for (at, cell) in row.cells.iter().enumerate() {
        if cell.glyph.len() > MAX_GLYPH_BYTES
            || cell.attributes.flags > 511
            || cell.attributes.flags & (8 | 256) == (8 | 256)
        {
            return Err(Error::Image);
        }
        if cell.width == 0 {
            if !cell.glyph.is_empty()
                || at == 0
                || row.cells[at - 1].width != 2
                || row.cells[at - 1].attributes != cell.attributes
            {
                return Err(Error::Image);
            }
        } else {
            if !matches!(cell.width, 1 | 2)
                || (cell.width == 2 && (at + 1 == row.cells.len() || row.cells[at + 1].width != 0))
            {
                return Err(Error::Image);
            }
            if cell.glyph.is_empty() {
                if cell.width != 1 {
                    return Err(Error::Image);
                }
            } else {
                let mut chars = cell.glyph.chars();
                if scalar_width(chars.next().ok_or(Error::Image)?).map_err(|_| Error::Image)?
                    != cell.width as usize
                    || chars.any(|c| scalar_width(c) != Ok(0))
                {
                    return Err(Error::Image);
                }
            }
        }
    }
    Ok(())
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
