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

fn bare(token: &str) -> &str {
    token
        .trim_matches([
            '*', '_', '`', '\'', '"', '(', ')', '[', ']', '<', '>', '{', '}',
        ])
        .trim_end_matches(['.', ',', ':', ';', '!', '?'])
        .trim_matches(['*', '_', '`', '\'', '"', '(', ')', '[', ']'])
}
fn first_word(value: &str) -> &str {
    value
        .split_whitespace()
        .map(bare)
        .find(|t| !t.is_empty())
        .unwrap_or_default()
}
/// A token that names a file or directory: `dir/name`, `dir/`, or `name.ext`.
fn path_like(token: &str) -> bool {
    let t = bare(token);
    if t.contains('/') {
        let lower = t.to_ascii_lowercase();
        return t
            .split('/')
            .any(|s| s.chars().filter(|c| c.is_alphanumeric()).count() >= 2)
            && !["and/or", "either/or", "yes/no"].contains(&lower.as_str());
    }
    let Some((name, ext)) = t.rsplit_once('.') else {
        return false;
    };
    name.chars().count() >= 2
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || "-_.".contains(c))
        && (1..=10).contains(&ext.len())
        && ext.chars().all(|c| c.is_ascii_alphanumeric())
        && ext.chars().any(|c| c.is_ascii_alphabetic())
}
const NEGATIONS: [&str; 13] = [
    "none",
    "no",
    "nothing",
    "n/a",
    "na",
    "-",
    "nil",
    "null",
    "unchanged",
    "read-only",
    "readonly",
    "zero",
    "0",
];

/// Review R1-I1: a `Changes:` value claims changes only when it names at least one
/// path-like token and does not start with a negation. Anything else is no claim.
pub fn names_paths(value: &str) -> bool {
    let first = first_word(value).to_ascii_lowercase();
    !NEGATIONS.contains(&first.as_str())
        && value
            .split(|c: char| c.is_whitespace() || c == ',')
            .any(path_like)
}

#[cfg(test)]
pub(crate) const CASES: [(&str, bool); 24] = [
    ("none", false),
    ("None.", false),
    ("no", false),
    ("nothing", false),
    ("n/a", false),
    ("N/A", false),
    ("no files changed", false),
    ("No changes needed", false),
    ("", false),
    ("(none)", false),
    ("-", false),
    ("`none`", false),
    ("**none**", false),
    ("read-only investigation", false),
    ("unchanged.", false),
    ("none (read the Node.js docs)", false),
    ("e.g. nothing", false),
    ("version 1.2 bumped", false),
    ("src/lib.rs", true),
    ("`src/lib.rs`, `src/api.rs`", true),
    ("**README.md**.", true),
    ("Cargo.toml and docs/status.md", true),
    ("updated crates/aq/src/x.rs (added a test)", true),
    ("Makefile.", false),
];

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_changes_value_claims_changes_only_when_it_names_a_path() {
        for (value, claims) in CASES {
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
