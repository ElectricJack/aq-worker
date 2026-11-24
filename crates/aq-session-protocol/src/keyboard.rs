/// A command is a complete keyboard packet. A bare Escape is represented as
/// an explicit key so an interrupted CSI prefix cannot become that key.
pub fn complete_terminal_input(bytes: &[u8], escape_key: bool) -> bool {
    if escape_key {
        return bytes == b"\x1b";
    }
    let mut rest = bytes;
    while let Some(start) = rest.iter().position(|&b| b == 0x1b) {
        let Some(end) = escape_end(&rest[start + 1..]) else {
            return false;
        };
        rest = &rest[start + 1 + end..];
    }
    !bytes.is_empty()
}

fn escape_end(bytes: &[u8]) -> Option<usize> {
    match *bytes.first()? {
        b'[' | b'O' => control_sequence_end(bytes),
        prefix @ (b']' | b'P' | b'X' | b'^' | b'_') => string_end(bytes, prefix),
        0x20..=0x2f => {
            let end = bytes.iter().position(|b| !(0x20..=0x2f).contains(b))?;
            matches!(bytes[end], 0x30..=0x7e).then_some(end + 1)
        }
        0x1b => None,
        0x00..=0x7f => Some(1), // Alt plus an ASCII key.
        first => utf8_key_end(bytes, first),
    }
}

fn control_sequence_end(bytes: &[u8]) -> Option<usize> {
    let mut end = 1;
    while matches!(bytes.get(end), Some(0x30..=0x3f)) {
        end += 1;
    }
    while matches!(bytes.get(end), Some(0x20..=0x2f)) {
        end += 1;
    }
    if !matches!(bytes.get(end), Some(0x40..=0x7e)) {
        return None;
    }
    // Legacy X10 mouse reports have three coordinate bytes after CSI M.
    if bytes.starts_with(b"[M") {
        return bytes
            .get(..5)
            .filter(|v| !v[2..].contains(&0x1b))
            .map(|_| 5);
    }
    Some(end + 1)
}

fn string_end(bytes: &[u8], prefix: u8) -> Option<usize> {
    for (i, &byte) in bytes.iter().enumerate().skip(1) {
        if prefix == b']' && byte == 7 {
            return Some(i + 1);
        }
        if byte == 0x1b {
            return (bytes.get(i + 1) == Some(&b'\\')).then_some(i + 2);
        }
    }
    None
}

fn utf8_key_end(bytes: &[u8], first: u8) -> Option<usize> {
    let width = match first {
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => return None,
    };
    std::str::from_utf8(bytes.get(..width)?).ok().map(|_| width)
}
