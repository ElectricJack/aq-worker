use super::*;
fn params(bytes: &[u8]) -> Result<Vec<u16>> {
    if bytes.len() > MAX_CSI_BYTES {
        return Err(Error::Bound);
    }
    let mut values = Vec::new();
    for value in bytes.split(|b| *b == b';') {
        if values.len() == 32 {
            return Err(Error::Unsupported("CSI parameters"));
        }
        values.push(parse_parameter(value)?);
    }
    Ok(values)
}
fn parse_parameter(value: &[u8]) -> Result<u16> {
    if value.len() > 5 || !value.iter().all(u8::is_ascii_digit) {
        return Err(Error::Unsupported("CSI parameters"));
    }
    if value.is_empty() {
        return Ok(0);
    }
    std::str::from_utf8(value)
        .map_err(|_| Error::Framing)?
        .parse()
        .map_err(|_| Error::Bound)
}

pub(super) fn apply(
    bytes: &[u8],
    final_byte: u8,
    state: &mut State,
    work: &mut Work,
) -> Result<()> {
    match (bytes, final_byte) {
        ([b'?', rest @ ..], _) => Sequence::parse(final_byte, rest)?.dec_control(state, work),
        ([b'>', rest @ ..], b'c') => {
            params(rest)?;
            Ok(())
        }
        ([style @ .., b' '], b'q') => set_cursor_style(state, style),
        _ => {
            let sequence = Sequence::parse(final_byte, bytes)?;
            sequence.check_arity()?;
            sequence.execute(state, work)
        }
    }
}

fn set_cursor_style(state: &mut State, style: &[u8]) -> Result<()> {
    let values = params(style)?;
    let [value @ 0..=6] = values.as_slice() else {
        return Err(Error::Unsupported("cursor style"));
    };
    state.modes.cursor_style = *value as u8;
    Ok(())
}

struct Sequence {
    final_byte: u8,
    parameters: Vec<u16>,
}

impl Sequence {
    fn parse(final_byte: u8, bytes: &[u8]) -> Result<Self> {
        Ok(Self {
            final_byte,
            parameters: params(bytes)?,
        })
    }
    fn count(&self) -> usize {
        self.parameters[0].max(1) as usize
    }

    fn dec_control(&self, state: &mut State, work: &mut Work) -> Result<()> {
        let final_byte = self.final_byte;
        let p = &self.parameters;
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
                47 | 1047 | 1049 => state.alternate(set, *mode, work)?,
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
        Ok(())
    }

    fn erase_display(&self, state: &mut State, work: &mut Work) -> Result<()> {
        let mode = self.parameters[0];
        let cursor = state.active().cursor;
        let (row, col) = (cursor.row as usize, cursor.column as usize);
        match mode {
            0 => {
                state.erase(row, col..state.columns, work)?;
                for r in row + 1..state.rows {
                    state.erase(r, 0..state.columns, work)?;
                }
            }
            1 => {
                for r in 0..row {
                    state.erase(r, 0..state.columns, work)?;
                }
                state.erase(row, 0..col + 1, work)?;
            }
            2 => {
                for r in 0..state.rows {
                    state.erase(r, 0..state.columns, work)?;
                }
            }
            3 => {
                state.active_mut().history.clear();
                state.active_mut().history_cells = 0;
            }
            _ => return Err(Error::Unsupported("erase display")),
        }
        Ok(())
    }

    fn check_arity(&self) -> Result<()> {
        let max_params = if matches!(self.final_byte, b'H' | b'f' | b'r' | b'h' | b'l') {
            2
        } else {
            1
        };
        if self.final_byte != b'm' && self.parameters.len() > max_params {
            return Err(Error::Unsupported("CSI arity"));
        }
        Ok(())
    }

    fn execute(&self, state: &mut State, work: &mut Work) -> Result<()> {
        let final_byte = self.final_byte;
        let p = self.parameters.as_slice();
        let first = p[0];
        let n = self.count();
        let cursor = state.active().cursor;
        let (row, col) = (cursor.row as usize, cursor.column as usize);
        match final_byte {
            b'A' | b'B' | b'e' | b'C' | b'a' | b'D' | b'E' | b'F' | b'G' | b'`' | b'd' | b'H'
            | b'f' => self.move_cursor(state),
            b'J' => self.erase_display(state, work)?,
            b'K' => match first {
                0 => state.erase(row, col..state.columns, work)?,
                1 => state.erase(row, 0..col + 1, work)?,
                2 => state.erase(row, 0..state.columns, work)?,
                _ => return Err(Error::Unsupported("erase line")),
            },
            b'X' => state.erase(row, col..(col + n).min(state.columns), work)?,
            b'@' => state.edit_cells(n, true, work)?,
            b'P' => state.edit_cells(n, false, work)?,
            b'L' => state.edit_lines(n, true, work)?,
            b'M' => state.edit_lines(n, false, work)?,
            b'S' => state.scroll_up(n, work)?,
            b'T' => state.scroll_down(n, work)?,
            b'r' => self.scroll_margins(state)?,
            b's' => state.active_mut().ansi_saved = cursor,
            b'u' => state.active_mut().cursor = state.active().ansi_saved,
            b'g' => match first {
                0 => state.tabs[col] = false,
                3 => state.tabs.fill(false),
                _ => return Err(Error::Unsupported("tab clear")),
            },
            b'I' | b'Z' => self.tab(state, work)?,
            b'h' | b'l' => self.ansi_modes(state)?,
            b'm' => return sgr(p, &mut state.attributes),
            b'b' => self.repeat(state, work)?,
            b'n' if p.len() == 1 && matches!(first, 5 | 6) => return Ok(()),
            b'c' if p == [0] => return Ok(()),
            _ => return Err(Error::Unsupported("CSI control")),
        }
        if !matches!(final_byte, b's' | b'u' | b'b') {
            state.clear_wrap();
        }
        Ok(())
    }
    fn scroll_margins(&self, state: &mut State) -> Result<()> {
        let p = &self.parameters;
        let t = self.count() - 1;
        let b = p
            .get(1)
            .copied()
            .filter(|n| *n > 0)
            .unwrap_or(state.rows as u16) as usize
            - 1;
        let single_line = t == b && state.rows != 1;
        if !(t..state.rows).contains(&b) || single_line {
            return Err(Error::Unsupported("scroll margins"));
        }
        state.active_mut().top = t;
        state.active_mut().bottom = b;
        state.home();
        Ok(())
    }
    fn ansi_modes(&self, state: &mut State) -> Result<()> {
        let set = self.final_byte == b'h';
        let p = &self.parameters;
        for mode in p {
            match mode {
                4 => state.modes.insert = set,
                20 => state.modes.new_line = set,
                _ => return Err(Error::Unsupported("ANSI mode")),
            }
        }
        Ok(())
    }
    fn repeat(&self, state: &mut State, work: &mut Work) -> Result<()> {
        let n = self.count();
        if n > 4096 {
            return Err(Error::Bound);
        }
        let c = state
            .last_print
            .ok_or(Error::Unsupported("repeat without character"))?;
        for _ in 0..n {
            state.print(c, work)?;
        }
        Ok(())
    }
    fn move_cursor(&self, state: &mut State) {
        let p = self.parameters.as_slice();
        let final_byte = self.final_byte;
        let n = self.count();
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
            b'C' | b'a' => {
                state.active_mut().cursor.column = (col + n).min(state.columns - 1) as u16
            }
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
            _ => unreachable!("cursor movement dispatch"),
        }
    }

    fn tab(&self, state: &mut State, work: &mut Work) -> Result<()> {
        let final_byte = self.final_byte;
        let n = self.count();
        work.spend(state.columns * 2)?;
        let mut at = state.active().cursor.column as usize;
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
        Ok(())
    }
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
fn set_bit(bits: &mut u8, flag: u8, enabled: bool) {
    if enabled {
        *bits |= flag;
    } else {
        *bits &= !flag;
    }
}
