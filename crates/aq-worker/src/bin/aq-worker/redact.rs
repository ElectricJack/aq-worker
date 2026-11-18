//! Preserve terminal geometry while masking the credential shapes already guarded
//! by the snapshot adapter. Public prefixes can span frames/colour escapes; no
//! lookahead or delay is imposed on ordinary typing.
#[derive(Default)]
pub(super) struct Redactor {
    suffix: Vec<u8>,
    masked: bool,
    escape: u8,
}
impl Redactor {
    pub fn frame(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(bytes.len());
        for &b in bytes {
            if self.escape != 0 {
                out.push(b);
                self.escape = match (self.escape, b) {
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
                };
                continue;
            }
            if b == 27 {
                self.escape = 1;
                out.push(b);
                continue;
            }
            if self.masked {
                if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' {
                    out.push(b'*');
                    continue;
                }
                if b == b'\r' || b == b'\n' {
                    out.push(b);
                    continue;
                }
                self.masked = false;
            }
            out.push(b);
            self.suffix.push(b);
            if self.suffix.len() > 7 {
                self.suffix.remove(0);
            }
            if self.suffix.ends_with(b"aqa_") || self.suffix.ends_with(b"sk-ant-") {
                self.masked = true;
                self.suffix.clear();
            }
        }
        out
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
