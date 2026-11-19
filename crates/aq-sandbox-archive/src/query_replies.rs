//! Drops terminal replies to the sandboxed program's own queries from the pane input.
//!
//! Claude Code asks its terminal who it is (DA1 `CSI c`, XTVERSION `CSI > 0 q`) and for its
//! colours (OSC 10/11/12). tmux answers on the pane's input, which this driver relays to the
//! sandbox. The relay adds latency, so an answer can arrive after the program stopped waiting
//! for it, and the program then takes it for typed text (`^[P>|tmux 3.4^[\^[[?1;2;4c` in the
//! prompt). No keyboard produces these sequences, so they are dropped; the program's own
//! timeout already handles the missing answer.

/// Longest reply kept while waiting for the rest of it; anything longer is passed through.
const MAX_HELD: usize = 256;

#[derive(Default)]
pub struct ReplyFilter {
    held: Vec<u8>,
}

enum Scan {
    /// A whole reply of this many bytes.
    Reply(usize),
    /// Could still become a reply: wait for more input.
    Incomplete,
    /// Not a reply.
    No,
}

/// `bytes` starts with ESC. Recognises DA1/DA2 replies (`ESC [ ?|> params c`), XTVERSION
/// (`ESC P > | text ESC \`) and OSC 10/11/12 colour replies (`ESC ] 1x ; text BEL|ESC \`).
fn scan(bytes: &[u8]) -> Scan {
    let at = |i: usize| bytes.get(i).copied();
    match (at(1), at(2)) {
        (Some(b'['), Some(b'?' | b'>')) => {
            for (i, &b) in bytes.iter().enumerate().skip(3) {
                match b {
                    b'0'..=b'9' | b';' => continue,
                    b'c' => return Scan::Reply(i + 1),
                    _ => return Scan::No,
                }
            }
            Scan::Incomplete
        }
        (Some(b'P'), Some(b'>')) => match at(3) {
            None => Scan::Incomplete,
            Some(b'|') => terminated(bytes, 4, false),
            Some(_) => Scan::No,
        },
        (Some(b']'), Some(b'1')) => match (at(3), at(4)) {
            (None, _) | (Some(b'0'..=b'2'), None) => Scan::Incomplete,
            (Some(b'0'..=b'2'), Some(b';')) => terminated(bytes, 5, true),
            _ => Scan::No,
        },
        (None, _) | (Some(b'[' | b'P' | b']'), None) => Scan::Incomplete,
        _ => Scan::No,
    }
}

/// The sequence ends with ST (`ESC \`), or BEL when `bel` is allowed.
fn terminated(bytes: &[u8], from: usize, bel: bool) -> Scan {
    let mut i = from;
    while i < bytes.len() {
        match bytes[i] {
            0x07 if bel => return Scan::Reply(i + 1),
            0x1b => {
                return match bytes.get(i + 1) {
                    Some(b'\\') => Scan::Reply(i + 2),
                    Some(_) => Scan::No,
                    None => Scan::Incomplete,
                };
            }
            _ => i += 1,
        }
    }
    Scan::Incomplete
}

impl ReplyFilter {
    /// The bytes of `input` to relay. A lone ESC or an arrow key passes at once; only a
    /// prefix that already reads as one of the replies above (three bytes or more) is held
    /// until the next read decides it.
    pub fn feed(&mut self, input: &[u8]) -> Vec<u8> {
        self.held.extend_from_slice(input);
        let (mut out, mut i) = (Vec::with_capacity(self.held.len()), 0);
        while i < self.held.len() {
            if self.held[i] != 0x1b {
                out.push(self.held[i]);
                i += 1;
                continue;
            }
            match scan(&self.held[i..]) {
                Scan::Reply(n) => i += n,
                Scan::Incomplete if self.held.len() - i >= 3 && self.held.len() - i <= MAX_HELD => {
                    let rest = self.held.split_off(i);
                    self.held = rest;
                    return out;
                }
                _ => {
                    out.push(0x1b);
                    i += 1;
                }
            }
        }
        self.held.clear();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_the_replies_seen_in_a_claude_prompt_and_keeps_keys() {
        let mut f = ReplyFilter::default();
        assert_eq!(f.feed(b"\x1bP>|tmux 3.4\x1b\\\x1b[?1;2;4c"), b"");
        assert_eq!(f.feed(b"hello\x1b[>0;95;0c!"), b"hello!");
        assert_eq!(f.feed(b"\x1b]11;rgb:0000/0000/0000\x07x"), b"x");
        // Keys: Escape alone, arrows, a bracketed paste and Alt+letter pass unchanged.
        for key in [
            &b"\x1b"[..],
            b"\x1b[A",
            b"\x1b[200~hi\x1b[201~",
            b"\x1bb",
            b"\x1b[1;5C",
            b"\x1bOP",
        ] {
            assert_eq!(f.feed(key), key, "{key:?}");
        }
    }

    #[test]
    fn a_reply_split_across_reads_is_still_dropped() {
        let mut f = ReplyFilter::default();
        assert_eq!(f.feed(b"a\x1bP>|tm"), b"a");
        assert_eq!(f.feed(b"ux 3.4\x1b"), b"");
        assert_eq!(f.feed(b"\\b\x1b[?1;"), b"b");
        assert_eq!(f.feed(b"2c"), b"");
    }

    #[test]
    fn something_that_only_starts_like_a_reply_passes() {
        let mut f = ReplyFilter::default();
        assert_eq!(f.feed(b"\x1b[?25h"), b"\x1b[?25h");
        assert_eq!(f.feed(b"\x1bPx"), b"\x1bPx");
        let long = [b"\x1bP>|".as_slice(), &[b'x'; 300]].concat();
        assert_eq!(f.feed(&long), long);
    }
}
