//! The agent's own `Changes:` and `Outcome:` lines in its final message. Both
//! readings fail open (review R1-I1): an honest no-change run is never failed by
//! them, whatever words it used. aq-server's `aq_profiled_state::claim` reads the
//! conclusion with the same rules and the same table of cases.

/// The value of `key:` on one line, tolerant of case, a list bullet or quote
/// marker, and markdown emphasis or code around the key (`**Outcome:** blocked`).
pub fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    const MARKS: [char; 4] = ['*', '_', '`', ' '];
    let line = line.trim_start().trim_start_matches(['-', '>', ' ']);
    let line = line.trim_start_matches(MARKS);
    let head = line.get(..key.len())?;
    if !head.eq_ignore_ascii_case(key) {
        return None;
    }
    let rest = line[key.len()..].trim_start_matches(['*', '_', '`']);
    Some(rest.strip_prefix(':')?.trim_start_matches(MARKS).trim())
}

/// The agent's last `Changes:` line's value, up to its first `;`.
pub fn claimed_changes(text: &str) -> Option<&str> {
    let value = text.lines().rev().find_map(|l| field(l, "changes"))?;
    Some(value.split(';').next().unwrap_or_default().trim())
}

/// Owner ruling 4a: the agent's last `Outcome:` line says `blocked`. A missing or
/// unrecognised Outcome line is `done`.
pub fn blocked(text: &str) -> bool {
    text.lines()
        .rev()
        .find_map(|l| field(l, "outcome"))
        .is_some_and(said_blocked)
}
/// An `Outcome:` value whose first word is `blocked`.
pub fn said_blocked(value: &str) -> bool {
    first_word(value).eq_ignore_ascii_case("blocked")
}

/// A token without its wrapping backticks or emphasis and its trailing `.` `;` `,`.
fn bare(token: &str) -> &str {
    const WRAP: [char; 3] = ['*', '_', '`'];
    token
        .trim_matches(WRAP)
        .trim_end_matches(['.', ';', ','])
        .trim_matches(WRAP)
}
fn first_word(value: &str) -> &str {
    value
        .split_whitespace()
        .map(bare)
        .find(|t| !t.is_empty())
        .unwrap_or_default()
}
/// A file name `name.ext`: `ext` 1-8 alphanumerics with a letter; `name` at least two
/// characters, starting with two before any dot (`a.k.a`, `e.g` are words), of
/// letters, digits, `-`, `_` and `.`, and not a bare number or version.
fn file_name(segment: &str) -> bool {
    let Some((name, ext)) = segment.rsplit_once('.') else {
        return false;
    };
    let number = |s: &str| {
        s.trim_start_matches(['v', 'V'])
            .chars()
            .all(|c| c.is_ascii_digit() || c == '.')
    };
    name.chars().count() >= 2
        && name
            .split('.')
            .next()
            .is_some_and(|s| s.chars().count() >= 2)
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || "-_.".contains(c))
        && !number(name)
        && (1..=8).contains(&ext.len())
        && ext.chars().all(|c| c.is_ascii_alphanumeric())
        && ext.chars().any(|c| c.is_ascii_alphabetic())
}
/// A dotfile or dot-directory segment (`.github`, `.config`).
fn dot_name(segment: &str) -> bool {
    segment.strip_prefix('.').is_some_and(|rest| {
        rest.chars().count() >= 2
            && rest
                .chars()
                .all(|c| c.is_alphanumeric() || "-_.".contains(c))
    })
}
/// Review R3-I1: a path token is (a) a file name, (b) a slash path whose last
/// segment is a file name, (c) a slash path ending in `/` whose first segment has
/// two or more characters, or (d) a slash path with a segment containing `-`, `_`
/// or a digit, or a segment that is a file or dot name. Letter-only slash words
/// (`none/nil`, `and/or`, `CI/CD`, `I/O`) are not paths, and nothing with `://` is.
fn path_like(token: &str) -> bool {
    if token.contains("://")
        || !token
            .chars()
            .all(|c| c.is_alphanumeric() || "-_./".contains(c))
    {
        return false;
    }
    if !token.contains('/') {
        return file_name(token);
    }
    let segments: Vec<&str> = token.split('/').collect();
    let last = segments[segments.len() - 1];
    file_name(last)
        || (last.is_empty() && segments[0].chars().count() >= 2)
        || segments.iter().any(|s| {
            s.chars()
                .any(|c| c == '-' || c == '_' || c.is_ascii_digit())
                || (s.contains('.') && (file_name(s) || dot_name(s)))
        })
}

/// Review R2-I1, R3-I1: a `Changes:` value claims changes only when it consists
/// ENTIRELY of path tokens: separated by commas and/or whitespace, after a leading
/// list bullet, each optionally wrapped in backticks or emphasis and followed by a
/// trailing `.` `;` or `,`. Any other character or word (a parenthesised note, a
/// colon, a dash used as a separator, a word that is not a path) means no claim.
/// Prose is never interpreted: a missed claim only loses this consistency check
/// (the task then reads "Completed, no changes" and the full message is in its
/// comments), while a false claim fails an honest run.
pub fn names_paths(value: &str) -> bool {
    let value = value.trim();
    let value = ["- ", "* ", "+ "]
        .iter()
        .find_map(|b| value.strip_prefix(b))
        .unwrap_or(value);
    let tokens: Vec<&str> = value
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|t| !t.is_empty())
        .collect();
    !tokens.is_empty() && tokens.iter().all(|t| path_like(bare(t)))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    const CASES: &str = include_str!("claim_cases.txt");
    /// FNV-1a 64 of the shared table; the same constant is in the other repository.
    const CASES_FNV: u64 = 0xce2b9d1a7171d515;
    fn fnv(bytes: &[u8]) -> u64 {
        bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| {
            (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
        })
    }
    pub(crate) fn cases() -> Vec<(bool, &'static str)> {
        CASES
            .lines()
            .filter(|l| !l.starts_with('#'))
            .map(|l| {
                let (kind, value) = l.split_once('\t').expect("kind<TAB>value");
                assert!(kind == "claim" || kind == "none", "{l}");
                (kind == "claim", value)
            })
            .collect()
    }
    #[test]
    fn the_shared_claim_table_is_unchanged() {
        assert_eq!(
            fnv(CASES.as_bytes()),
            CASES_FNV,
            "update both repositories' tables together"
        );
        assert!(cases().len() >= 60);
    }
    #[test]
    fn a_changes_value_claims_changes_only_when_it_is_a_list_of_paths() {
        for (claims, value) in cases() {
            assert_eq!(names_paths(value), claims, "{value:?}");
        }
    }
    #[test]
    fn keys_are_read_through_case_and_markdown() {
        for line in [
            "Outcome: blocked",
            "outcome: Blocked.",
            "**Outcome:** blocked",
            "**Outcome**: blocked - no access",
            "- `Outcome:` blocked",
        ] {
            assert!(blocked(line), "{line}");
        }
        for text in [
            "Outcome: done",
            "Outcome: maybe",
            "no outcome line",
            "Outcomes: blocked",
        ] {
            assert!(!blocked(text), "{text}");
        }
        assert_eq!(
            claimed_changes("**Changes:** `none`; Checks: x"),
            Some("none`")
        );
        assert_eq!(
            claimed_changes("answer\nchanges: src/a.rs; Checks: x"),
            Some("src/a.rs")
        );
    }
}
