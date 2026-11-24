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
    value
        .split_whitespace()
        .map(ClaimToken)
        .map(ClaimToken::bare)
        .find(|t| !t.0.is_empty())
        .is_some_and(|word| word.0.eq_ignore_ascii_case("blocked"))
}

struct ClaimToken<'a>(&'a str);

impl ClaimToken<'_> {
    /// Strip Markdown delimiters and trailing punctuation from a claim token.
    fn bare(self) -> Self {
        const WRAP: [char; 3] = ['*', '_', '`'];
        Self(
            self.0
                .trim_matches(WRAP)
                .trim_end_matches(['.', ';', ','])
                .trim_matches(WRAP),
        )
    }
    /// Review R3-I1: a path token is (a) a file name, (b) a slash path whose last
    /// segment is a file name, (c) a slash path ending in `/` whose first segment has
    /// two or more characters, or (d) a slash path with a segment containing `-`, `_`
    /// or a digit, or a segment that is a file or dot name. Letter-only slash words
    /// (`none/nil`, `and/or`, `CI/CD`, `I/O`) are not paths, and nothing with `://` is.
    fn is_path(&self) -> bool {
        let token = self.0;
        if !token.chars().all(|c| c == '/' || name_character(c)) {
            return false;
        }
        if !token.contains('/') {
            return self.is_file_name();
        }
        let mut segments = token.split('/');
        let first = segments.next().unwrap_or_default();
        (token.ends_with('/') && first.chars().count() >= 2)
            || std::iter::once(first)
                .chain(segments)
                .map(ClaimToken)
                .any(|segment| segment.identifies_path())
    }

    /// Require an extension of 1–8 ASCII alphanumerics with at least one letter.
    fn is_file_name(&self) -> bool {
        let Some((stem, extension)) = self.0.rsplit_once('.') else {
            return false;
        };
        let stem = ClaimToken(stem);
        let extension = ClaimToken(extension);
        stem.valid_stem() && extension.valid_extension()
    }

    fn valid_stem(&self) -> bool {
        let first = self.0.split('.').next().unwrap_or_default();
        let version = self
            .0
            .trim_start_matches(['v', 'V'])
            .chars()
            .all(|c| c.is_ascii_digit() || c == '.');
        first.chars().count() >= 2 && self.0.chars().all(name_character) && !version
    }

    fn valid_extension(&self) -> bool {
        (1..=8).contains(&self.0.len())
            && self.0.chars().all(|c| c.is_ascii_alphanumeric())
            && self.0.chars().any(|c| c.is_ascii_alphabetic())
    }

    fn is_dot_name(&self) -> bool {
        self.0
            .strip_prefix('.')
            .is_some_and(|rest| rest.chars().count() >= 2 && rest.chars().all(name_character))
    }

    fn identifies_path(&self) -> bool {
        self.0
            .chars()
            .any(|c| matches!(c, '-' | '_') || c.is_ascii_digit())
            || self.is_file_name()
            || self.is_dot_name()
    }
}

fn name_character(c: char) -> bool {
    c.is_alphanumeric() || "-_.".contains(c)
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
    let mut tokens = value
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|t| !t.is_empty())
        .map(ClaimToken)
        .map(ClaimToken::bare)
        .peekable();
    tokens.peek().is_some() && tokens.all(|t| t.is_path())
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
