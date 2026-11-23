mod csi;
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
            return self.continue_utf8(byte, state, work);
        }
        match (&self.mode, byte) {
            (Mode::Osc { .. }, _) => self.osc_byte(byte, state),
            (Mode::Ground, 0xc2..=0xf4) => {
                self.utf8.push(byte);
                Ok(())
            }
            (Mode::Ground, 0x80..=0xff) => Err(Error::Framing),
            (_, 0x1b) => {
                self.mode = Mode::Escape;
                Ok(())
            }
            (_, 0x18 | 0x1a) => {
                self.mode = Mode::Ground;
                Ok(())
            }
            (_, 0..=0x1f | 0x7f) => control(byte, state, work),
            _ => self.control_or_print(byte, state, work),
        }
    }

    fn continue_utf8(&mut self, byte: u8, state: &mut State, work: &mut Work) -> Result<()> {
        if !(0x80..=0xbf).contains(&byte) {
            return Err(Error::Framing);
        }
        self.utf8.push(byte);
        match std::str::from_utf8(&self.utf8) {
            Ok(text) => {
                state.print(text.chars().next().ok_or(Error::Framing)?, work)?;
                self.utf8.clear();
            }
            Err(error) if error.error_len().is_none() => (),
            Err(_) => return Err(Error::Framing),
        }
        Ok(())
    }

    fn escape(&mut self, byte: u8, state: &mut State, work: &mut Work) -> Result<()> {
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

    fn control_or_print(&mut self, byte: u8, state: &mut State, work: &mut Work) -> Result<()> {
        match std::mem::take(&mut self.mode) {
            Mode::Ground => state.print(mapped(byte as char, state), work),
            Mode::Escape => self.escape(byte, state, work),
            Mode::Charset(slot) => {
                state.charsets[slot as usize] = match byte {
                    b'B' => 0,
                    b'0' => 1,
                    _ => return Err(Error::Unsupported("character set")),
                };
                Ok(())
            }
            Mode::Csi(bytes) if (0x40..=0x7e).contains(&byte) => {
                csi::apply(&bytes, byte, state, work)
            }
            Mode::Csi(mut bytes)
                if (0x20..=0x3f).contains(&byte) && bytes.len() < MAX_CSI_BYTES =>
            {
                bytes.push(byte);
                self.mode = Mode::Csi(bytes);
                Ok(())
            }
            Mode::Csi(_) => Err(Error::Framing),
            Mode::Osc { .. } => unreachable!("handled above"),
        }
    }
    fn osc_byte(&mut self, byte: u8, state: &mut State) -> Result<()> {
        let Mode::Osc { mut bytes, escaped } = std::mem::take(&mut self.mode) else {
            return Err(Error::Framing);
        };
        self.mode = match (escaped, byte) {
            (true, b'\\') | (false, 7) => return osc(&bytes, state),
            (true, _) => return Err(Error::Unsupported("OSC embedded escape")),
            (false, 0x1b) => Mode::Osc {
                bytes,
                escaped: true,
            },
            (false, 0..=0x1f | 0x7f) => return Err(Error::Framing),
            _ if bytes.len() == MAX_CONTROL_BYTES => return Err(Error::Bound),
            _ => {
                bytes.push(byte);
                Mode::Osc {
                    bytes,
                    escaped: false,
                }
            }
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
fn osc(bytes: &[u8], state: &mut State) -> Result<()> {
    let value = std::str::from_utf8(bytes).map_err(|_| Error::Framing)?;
    let (code, value) = value.split_once(';').ok_or(Error::Framing)?;
    match code {
        "0" | "1" | "2" => set_title(state, code, value),
        "10" | "11" if value == "?" => Ok(()),
        _ => Err(Error::Unsupported("OSC control")),
    }
}

fn set_title(state: &mut State, code: &str, value: &str) -> Result<()> {
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
    Ok(())
}
