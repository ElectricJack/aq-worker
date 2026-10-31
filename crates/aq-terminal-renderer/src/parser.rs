use crate::*;
#[derive(Clone, PartialEq, Eq, Default)]
pub(crate) enum Mode {
    #[default]
    Ground,
    Escape,
    Charset(u8),
    Csi(Vec<u8>),
    Osc {
        bytes: Vec<u8>,
        escaped: bool,
    },
}
#[derive(Clone, PartialEq, Eq, Default)]
pub(crate) struct Parser {
    pub mode: Mode,
    pub utf8: Vec<u8>,
}
impl Parser {
    pub(crate) fn byte(&mut self, byte: u8, state: &mut State, work: &mut Work) -> Result<()> {
        work.spend(1)?;
        if !self.utf8.is_empty() {
            if !(0x80..=0xbf).contains(&byte) {
                return Err(Error::Framing);
            }
            self.utf8.push(byte);
            if std::str::from_utf8(&self.utf8).is_err_and(|e| e.error_len().is_some()) {
                return Err(Error::Framing);
            }
            let expected = match self.utf8[0] {
                0xc2..=0xdf => 2,
                0xe0..=0xef => 3,
                0xf0..=0xf4 => 4,
                _ => return Err(Error::Framing),
            };
            if self.utf8.len() == expected {
                let text = std::str::from_utf8(&self.utf8).map_err(|_| Error::Framing)?;
                state.print(text.chars().next().ok_or(Error::Framing)?, work)?;
                self.utf8.clear();
            }
            return Ok(());
        }
        if matches!(self.mode, Mode::Ground) && byte >= 0x80 {
            if !(0xc2..=0xf4).contains(&byte) {
                return Err(Error::Framing);
            }
            self.utf8.push(byte);
            return Ok(());
        }
        if matches!(self.mode, Mode::Osc { .. }) {
            return self.osc_byte(byte, state);
        }
        if byte == 0x1b {
            self.mode = Mode::Escape;
            return Ok(());
        }
        if matches!(byte, 0x18 | 0x1a) {
            self.mode = Mode::Ground;
            return Ok(());
        }
        if byte < 0x20 || byte == 0x7f {
            return control(byte, state, work);
        }
        match std::mem::take(&mut self.mode) {
            Mode::Ground => state.print(mapped(byte as char, state), work),
            Mode::Escape => {
                match byte {
                    b'[' => self.mode = Mode::Csi(Vec::new()),
                    b']' => {
                        self.mode = Mode::Osc {
                            bytes: Vec::new(),
                            escaped: false,
                        }
                    }
                    b'(' => self.mode = Mode::Charset(0),
                    b')' => self.mode = Mode::Charset(1),
                    b'7' => state.save_dec(),
                    b'8' => state.restore_dec(),
                    b'D' => state.linefeed(false, work)?,
                    b'E' => state.linefeed(true, work)?,
                    b'M' => state.reverse_index(work)?,
                    b'=' => state.modes.application_keypad = true,
                    b'>' => state.modes.application_keypad = false,
                    b'c' => {
                        work.spend(state.columns * state.rows * 2)?;
                        *state = State::new(state.columns, state.rows);
                    }
                    _ => return Err(Error::Unsupported("ESC control")),
                }
                Ok(())
            }
            Mode::Charset(slot) => {
                state.charsets[slot as usize] = match byte {
                    b'B' => 0,
                    b'0' => 1,
                    _ => return Err(Error::Unsupported("character set")),
                };
                Ok(())
            }
            Mode::Csi(mut bytes) => {
                if (0x40..=0x7e).contains(&byte) {
                    csi(&bytes, byte, state, work)
                } else if (0x20..=0x3f).contains(&byte) && bytes.len() < MAX_CSI_BYTES {
                    bytes.push(byte);
                    self.mode = Mode::Csi(bytes);
                    Ok(())
                } else {
                    Err(Error::Framing)
                }
            }
            Mode::Osc { .. } => unreachable!("handled above"),
        }
    }
    fn osc_byte(&mut self, byte: u8, state: &mut State) -> Result<()> {
        let Mode::Osc { mut bytes, escaped } = std::mem::take(&mut self.mode) else {
            return Err(Error::Framing);
        };
        if (escaped && byte == b'\\') || (!escaped && byte == 7) {
            return osc(&bytes, state);
        }
        if escaped {
            return Err(Error::Unsupported("OSC embedded escape"));
        }
        if byte == 0x1b {
            self.mode = Mode::Osc {
                bytes,
                escaped: true,
            };
            return Ok(());
        }
        if byte < 0x20 || byte == 0x7f {
            return Err(Error::Framing);
        }
        if bytes.len() == MAX_CONTROL_BYTES {
            return Err(Error::Bound);
        }
        bytes.push(byte);
        self.mode = Mode::Osc {
            bytes,
            escaped: false,
        };
        Ok(())
    }
}
fn control(byte: u8, state: &mut State, work: &mut Work) -> Result<()> {
    match byte {
        0 | 0x7f => (),
        7 => state.bells = state.bells.checked_add(1).ok_or(Error::Bound)?,
        8 => {
            let cursor = &mut state.active_mut().cursor;
            if cursor.pending_wrap {
                cursor.pending_wrap = false;
            } else {
                cursor.column = cursor.column.saturating_sub(1);
            }
        }
        9 => {
            work.spend(state.columns)?;
            let col = state.active().cursor.column as usize;
            let next = (col + 1..state.columns)
                .find(|c| state.tabs[*c])
                .unwrap_or(state.columns - 1);
            state.active_mut().cursor.column = next as u16;
            state.clear_wrap();
        }
        10..=12 => state.linefeed(false, work)?,
        13 => {
            state.active_mut().cursor.column = 0;
            state.clear_wrap();
        }
        14 => state.charset = 1,
        15 => state.charset = 0,
        _ => return Err(Error::Unsupported("C0 control")),
    }
    Ok(())
}
fn mapped(c: char, state: &State) -> char {
    if state.charsets[state.charset as usize] != 1 {
        return c;
    }
    match c {
        '_' => ' ',
        '`' => '◆',
        'a' => '▒',
        'b' => '␉',
        'c' => '␌',
        'd' => '␍',
        'e' => '␊',
        'f' => '°',
        'g' => '±',
        'h' => '␤',
        'i' => '␋',
        'j' => '┘',
        'k' => '┐',
        'l' => '┌',
        'm' => '└',
        'n' => '┼',
        'o' => '⎺',
        'p' => '⎻',
        'q' => '─',
        'r' => '⎼',
        's' => '⎽',
        't' => '├',
        'u' => '┤',
        'v' => '┴',
        'w' => '┬',
        'x' => '│',
        'y' => '≤',
        'z' => '≥',
        '{' => 'π',
        '|' => '≠',
        '}' => '£',
        '~' => '·',
        _ => c,
    }
}
fn params(bytes: &[u8]) -> Result<Vec<u16>> {
    if bytes.len() > MAX_CSI_BYTES {
        return Err(Error::Bound);
    }
    let mut values = Vec::new();
    for value in bytes.split(|b| *b == b';') {
        if values.len() == 32 || value.len() > 5 || !value.iter().all(u8::is_ascii_digit) {
            return Err(Error::Unsupported("CSI parameters"));
        }
        values.push(if value.is_empty() {
            0
        } else {
            std::str::from_utf8(value)
                .map_err(|_| Error::Framing)?
                .parse()
                .map_err(|_| Error::Bound)?
        });
    }
    Ok(values)
}
fn csi(bytes: &[u8], final_byte: u8, state: &mut State, work: &mut Work) -> Result<()> {
    let (private, bytes) = match bytes.first() {
        Some(b'?') => (true, &bytes[1..]),
        Some(b'>') if final_byte == b'c' => {
            params(&bytes[1..])?;
            return Ok(());
        }
        _ => (false, bytes),
    };
    if final_byte == b'q' && bytes.last() == Some(&b' ') && !private {
        let p = params(&bytes[..bytes.len() - 1])?;
        if p.len() != 1 || p[0] > 6 {
            return Err(Error::Unsupported("cursor style"));
        }
        state.modes.cursor_style = p[0] as u8;
        return Ok(());
    }
    let p = params(bytes)?;
    let first = p[0];
    let n = first.max(1) as usize;
    if private {
        if final_byte == b'n' && p.as_slice() == [6] {
            return Ok(());
        }
        if !matches!(final_byte, b'h' | b'l') {
            return Err(Error::Unsupported("DEC control"));
        }
        let set = final_byte == b'h';
        for mode in p {
            match mode {
                1 => state.modes.application_cursor = set,
                6 => {
                    state.modes.origin = set;
                    state.home();
                }
                7 => {
                    state.modes.auto_wrap = set;
                    state.clear_wrap();
                }
                25 => state.modes.cursor_visible = set,
                47 | 1047 | 1049 => state.alternate(set, mode, work)?,
                1048 => {
                    if set {
                        state.save_dec();
                    } else {
                        state.restore_dec();
                    }
                }
                1000 => set_bit(&mut state.modes.mouse_modes, 1, set),
                1002 => set_bit(&mut state.modes.mouse_modes, 2, set),
                1003 => set_bit(&mut state.modes.mouse_modes, 4, set),
                1005 => set_bit(&mut state.modes.mouse_encodings, 1, set),
                1006 => set_bit(&mut state.modes.mouse_encodings, 2, set),
                2004 => state.modes.bracketed_paste = set,
                _ => return Err(Error::Unsupported("DEC mode")),
            }
        }
        return Ok(());
    }
    let max_params = if matches!(final_byte, b'H' | b'f' | b'r' | b'h' | b'l') {
        2
    } else {
        1
    };
    if final_byte != b'm' && p.len() > max_params {
        return Err(Error::Unsupported("CSI arity"));
    }
    let cursor = state.active().cursor;
    let (row, col) = (cursor.row as usize, cursor.column as usize);
    let (top, bottom) = if state.modes.origin {
        (state.active().top, state.active().bottom)
    } else {
        (0, state.rows - 1)
    };
    match final_byte {
        b'A' => state.active_mut().cursor.row = row.saturating_sub(n).max(top) as u16,
        b'B' | b'e' => state.active_mut().cursor.row = (row + n).min(bottom) as u16,
        b'C' | b'a' => state.active_mut().cursor.column = (col + n).min(state.columns - 1) as u16,
        b'D' => state.active_mut().cursor.column = col.saturating_sub(n) as u16,
        b'E' => {
            state.active_mut().cursor.row = (row + n).min(bottom) as u16;
            state.active_mut().cursor.column = 0;
        }
        b'F' => {
            state.active_mut().cursor.row = row.saturating_sub(n).max(top) as u16;
            state.active_mut().cursor.column = 0;
        }
        b'G' | b'`' => state.active_mut().cursor.column = (n - 1).min(state.columns - 1) as u16,
        b'd' => state.active_mut().cursor.row = (top + n - 1).min(bottom) as u16,
        b'H' | b'f' => {
            state.active_mut().cursor.row = (top + n - 1).min(bottom) as u16;
            state.active_mut().cursor.column = p
                .get(1)
                .copied()
                .unwrap_or(1)
                .max(1)
                .saturating_sub(1)
                .min((state.columns - 1) as u16);
        }
        b'J' => match first {
            0 => {
                state.erase(row, col, state.columns, work)?;
                for r in row + 1..state.rows {
                    state.erase(r, 0, state.columns, work)?;
                }
            }
            1 => {
                for r in 0..row {
                    state.erase(r, 0, state.columns, work)?;
                }
                state.erase(row, 0, col + 1, work)?;
            }
            2 => {
                for r in 0..state.rows {
                    state.erase(r, 0, state.columns, work)?;
                }
            }
            3 => {
                state.active_mut().history.clear();
                state.active_mut().history_cells = 0;
            }
            _ => return Err(Error::Unsupported("erase display")),
        },
        b'K' => match first {
            0 => state.erase(row, col, state.columns, work)?,
            1 => state.erase(row, 0, col + 1, work)?,
            2 => state.erase(row, 0, state.columns, work)?,
            _ => return Err(Error::Unsupported("erase line")),
        },
        b'X' => state.erase(row, col, (col + n).min(state.columns), work)?,
        b'@' => state.edit_cells(n, true, work)?,
        b'P' => state.edit_cells(n, false, work)?,
        b'L' => state.edit_lines(n, true, work)?,
        b'M' => state.edit_lines(n, false, work)?,
        b'S' => state.scroll_up(n, work)?,
        b'T' => state.scroll_down(n, work)?,
        b'r' => {
            let t = n - 1;
            let b = p
                .get(1)
                .copied()
                .filter(|n| *n > 0)
                .unwrap_or(state.rows as u16) as usize
                - 1;
            if t > b || b >= state.rows || (t == b && state.rows != 1) {
                return Err(Error::Unsupported("scroll margins"));
            }
            state.active_mut().top = t;
            state.active_mut().bottom = b;
            state.home();
        }
        b's' => state.active_mut().ansi_saved = cursor,
        b'u' => state.active_mut().cursor = state.active().ansi_saved,
        b'g' => match first {
            0 => state.tabs[col] = false,
            3 => state.tabs.fill(false),
            _ => return Err(Error::Unsupported("tab clear")),
        },
        b'I' | b'Z' => {
            work.spend(state.columns * 2)?;
            let mut at = col;
            for _ in 0..n.min(state.columns) {
                at = if final_byte == b'I' {
                    (at + 1..state.columns)
                        .find(|c| state.tabs[*c])
                        .unwrap_or(state.columns - 1)
                } else {
                    (0..at).rev().find(|c| state.tabs[*c]).unwrap_or(0)
                };
            }
            state.active_mut().cursor.column = at as u16;
        }
        b'h' | b'l' => {
            for mode in p {
                let set = final_byte == b'h';
                match mode {
                    4 => state.modes.insert = set,
                    20 => state.modes.new_line = set,
                    _ => return Err(Error::Unsupported("ANSI mode")),
                }
            }
        }
        b'm' => return sgr(&p, &mut state.attributes),
        b'b' => {
            if n > 4096 {
                return Err(Error::Bound);
            }
            let c = state
                .last_print
                .ok_or(Error::Unsupported("repeat without character"))?;
            for _ in 0..n {
                state.print(c, work)?;
            }
        }
        b'n' if p.len() == 1 && matches!(first, 5 | 6) => return Ok(()),
        b'c' if p.as_slice() == [0] => return Ok(()),
        _ => return Err(Error::Unsupported("CSI control")),
    }
    if !matches!(final_byte, b's' | b'u' | b'b') {
        state.clear_wrap();
    }
    Ok(())
}
fn sgr(p: &[u16], a: &mut Attributes) -> Result<()> {
    let mut at = 0;
    while at < p.len() {
        match p[at] {
            0 => *a = Attributes::default(),
            1 => a.flags |= 1,
            2 => a.flags |= 2,
            3 => a.flags |= 4,
            4 => a.flags = (a.flags & !256) | 8,
            5 => a.flags |= 16,
            7 => a.flags |= 32,
            8 => a.flags |= 64,
            9 => a.flags |= 128,
            21 => a.flags = (a.flags & !8) | 256,
            22 => a.flags &= !3,
            23 => a.flags &= !4,
            24 => a.flags &= !(8 | 256),
            25 => a.flags &= !16,
            27 => a.flags &= !32,
            28 => a.flags &= !64,
            29 => a.flags &= !128,
            30..=37 => a.foreground = Color::Indexed((p[at] - 30) as u8),
            40..=47 => a.background = Color::Indexed((p[at] - 40) as u8),
            90..=97 => a.foreground = Color::Indexed((p[at] - 90 + 8) as u8),
            100..=107 => a.background = Color::Indexed((p[at] - 100 + 8) as u8),
            39 => a.foreground = Color::Default,
            49 => a.background = Color::Default,
            38 | 48 => {
                let foreground = p[at] == 38;
                let color = match p.get(at + 1) {
                    Some(5) => {
                        let color = u8::try_from(*p.get(at + 2).ok_or(Error::Framing)?)
                            .map_err(|_| Error::Bound)?;
                        at += 2;
                        Color::Indexed(color)
                    }
                    Some(2) => {
                        let values = p.get(at + 2..at + 5).ok_or(Error::Framing)?;
                        let mut rgb = [0u8; 3];
                        for (to, from) in rgb.iter_mut().zip(values) {
                            *to = u8::try_from(*from).map_err(|_| Error::Bound)?;
                        }
                        at += 4;
                        Color::Rgb(rgb[0], rgb[1], rgb[2])
                    }
                    _ => return Err(Error::Unsupported("SGR colour encoding")),
                };
                if foreground {
                    a.foreground = color;
                } else {
                    a.background = color;
                }
            }
            _ => return Err(Error::Unsupported("SGR attribute")),
        }
        at += 1;
    }
    Ok(())
}
fn osc(bytes: &[u8], state: &mut State) -> Result<()> {
    let value = std::str::from_utf8(bytes).map_err(|_| Error::Framing)?;
    let (code, value) = value.split_once(';').ok_or(Error::Framing)?;
    match code {
        "0" | "1" | "2" => {
            if value.chars().any(char::is_control) {
                return Err(Error::Framing);
            }
            if value.len() > 512 {
                return Err(Error::Bound);
            }
            if code != "1" {
                state.title = value.into();
            }
            if code != "2" {
                state.icon_name = value.into();
            }
        }
        "10" | "11" if value == "?" => (),
        _ => return Err(Error::Unsupported("OSC control")),
    }
    Ok(())
}

fn set_bit(bits: &mut u8, flag: u8, enabled: bool) {
    if enabled {
        *bits |= flag;
    } else {
        *bits &= !flag;
    }
}
