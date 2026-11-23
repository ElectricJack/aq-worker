//! Preserve terminal geometry while masking the credential shapes already guarded
//! by the snapshot adapter. Public prefixes can span frames/colour escapes; no
//! lookahead or delay is imposed on ordinary typing.
#[derive(Default)]
pub struct Redactor {
    suffix: Vec<u8>,
    masked: bool,
    escape: u8,
}
impl Redactor {
    pub fn frame(&mut self, bytes: &[u8]) -> Vec<u8> {
        bytes.iter().map(|&byte| self.redact_byte(byte)).collect()
    }

    fn redact_byte(&mut self, byte: u8) -> u8 {
        if self.escape != 0 || byte == 0x1b {
            self.escape = self.next_escape(byte);
            return byte;
        }
        if self.masked {
            match byte {
                b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' => return b'*',
                b'\r' | b'\n' => return byte,
                _ => self.masked = false,
            }
        }
        self.suffix.push(byte);
        if self.suffix.len() > 7 {
            self.suffix.remove(0);
        }
        if self.suffix.ends_with(b"aqa_") || self.suffix.ends_with(b"sk-ant-") {
            self.masked = true;
            self.suffix.clear();
        }
        byte
    }

    fn next_escape(&self, b: u8) -> u8 {
        match (self.escape, b) {
            (0, 0x1b) => 1,
            (1, b'[') => 2,
            (1, b']') => 3,
            (1, _) => 0,
            (2, 0x40..=0x7e) => 0,
            (2, _) => 2,
            (3, 7) => 0,
            (3, 27) => 4,
            (3, _) => 3,
            (4, b'\\') => 0,
            (4, _) => 3,
            _ => 0,
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn split_credentials_are_masked_without_rewriting_cursor_controls() {
        let sample=b"hello\x1b[3;6H sk-\x1b[32mant-abcdefghijklmnopqrstuvwxyz0123456789\x1b[0m! aqa_abcdefghijklmnopqrstuvwxyz0123456789.";
        for at in 0..sample.len() {
            let mut r = Redactor::default();
            let mut out = r.frame(&sample[..at]);
            out.extend(r.frame(&sample[at..]));
            assert!(out.starts_with(b"hello\x1b[3;6H sk-\x1b[32mant-"));
            assert!(
                out.windows(36)
                    .any(|w| w == b"************************************")
            );
            assert!(!out.windows(6).any(|w| w == b"abcdef"));
            assert_eq!(out.len(), sample.len());
        }
    }
    #[test]
    fn ordinary_keys_and_utf8_are_emitted_immediately() {
        let mut r = Redactor::default();
        for bytes in [b"a".as_slice(), b"s", b"\x1b[2;3H", "測試".as_bytes()] {
            assert_eq!(r.frame(bytes), bytes);
        }
    }
}
