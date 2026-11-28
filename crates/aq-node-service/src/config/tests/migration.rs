use super::*;

fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}
fn backup_of(path: &Path) -> PathBuf {
    path.with_file_name(format!("service.toml.{BEFORE_MEASUREMENT_REMOVAL}"))
}
fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = (std::fs::read_dir(dir).unwrap())
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}
/// Loads `text` from a file and returns the result, the directory and the file's path.
fn migrate(text: &str) -> (Result<ServiceConfig>, tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("service.toml");
    write_private(&path, text.as_bytes()).unwrap();
    (ServiceConfig::load(&path), dir, path)
}
/// A machine updated remotely still has the configuration its installer wrote. The worker
/// rewrites it once: the measurement setting goes, its token file becomes the manual
/// sessions' token file, every other key keeps its value, the comments and the order
/// stay, the file as it was is kept (0600), and the result loads.
#[test]
fn an_old_measurement_setting_is_removed_once_and_its_token_file_carried() {
    let slugs = "\n[manual.codex]\nexecutable = \"/opt/aq/bin/codex\"\n";
    let section = "\n# set by Push logins\n[measurement]\nenabled = true\ntoken_file = \"/c/provider-auth/claude-token\"\nmodel = \"m\"\nrenew_before_ms = 14400000\n";
    let inline = "measurement = { enabled = true, token_file = \"/c/provider-auth/claude-token\", model = \"m\" }\n";
    let dotted = "measurement.token_file = \"/c/provider-auth/claude-token\"\n";
    for old in [
        // As the hosted installer leaves it: the section, then the manual settings.
        format!(
            "# the owner's note\nshutdown_grace_ms = 30000 # short\n{MINIMAL}{section}\n{MANUAL}{slugs}"
        ),
        format!("{MINIMAL}{MANUAL}{slugs}{section}"),
        // As a hand-written file has it: one line among the top-level keys.
        format!("{inline}{MINIMAL}\n{MANUAL}{slugs}"),
        format!("{dotted}{MINIMAL}\n{MANUAL}{slugs}"),
    ] {
        let (loaded, _dir, path) = migrate(&old);
        let loaded = loaded.unwrap();
        let manual = loaded.manual.as_ref().unwrap();
        assert_eq!(
            manual.claude_token_file,
            Path::new("/c/provider-auth/claude-token")
        );
        assert!(manual.codex.is_some());
        let new = std::fs::read_to_string(&path).unwrap();
        assert_eq!(mode(&path), 0o600);
        let migration = Migration {
            old: &old,
            new: &new,
        };
        migration.assert_settings();
        migration.assert_backup_survives_reload(&path);
    }
    // The exact text of the common case: the key lands in `[manual]`, before its tables.
    let (text, carried) =
        without_measurement(&format!("{MINIMAL}{section}\n{MANUAL}{slugs}")).unwrap();
    assert_eq!(carried, CarriedToken::Moved);
    assert_eq!(
        text,
        format!(
            "{MINIMAL}\n{MANUAL}claude_token_file = \"/c/provider-auth/claude-token\"\n{slugs}"
        )
    );
}
struct Migration<'a> {
    old: &'a str,
    new: &'a str,
}

impl Migration<'_> {
    fn assert_settings(&self) {
        let Self { old, new } = *self;
        // Apart from the two intended changes every key has the value it had.
        let mut expected: toml::Table = toml::from_str(old).unwrap();
        expected.remove("measurement").unwrap();
        expected["manual"].as_table_mut().unwrap().insert(
            "claude_token_file".into(),
            "/c/provider-auth/claude-token".into(),
        );
        assert_eq!(toml::from_str::<toml::Table>(new).unwrap(), expected);
        assert!(!new.contains("measurement"), "{new}");
        assert_eq!(
            new.contains("# the owner's note"),
            old.contains("# the owner's note")
        );
        assert_eq!(new.contains("30000 # short"), old.contains("30000 # short"));
    }

    fn assert_backup_survives_reload(&self, path: &Path) {
        let Self { old, new } = *self;
        let backup = backup_of(path);
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), old);
        assert_eq!(mode(&backup), 0o600);
        // A second start finds the new file and leaves it and the copy alone.
        ServiceConfig::load(path).unwrap();
        assert_eq!(std::fs::read_to_string(path).unwrap(), new);
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), old);
        assert_eq!(
            names(path.parent().unwrap()),
            ["service.toml", "service.toml.before-measurement-removal"]
        );
    }
}

/// Without manual sessions nothing needs the token file; a token file `[manual]` already
/// names is kept, whatever the old setting said.
#[test]
fn the_measurement_setting_is_removed_without_manual_sessions_or_with_their_token_named() {
    for old in [
        "[measurement]\nenabled = false\nrenew_before_ms = 14400000\n",
        "[measurement]\nenabled = true\ntoken_file = \"/t\"\nmodel = \"m\"\n",
        "[measurement]\n",
    ] {
        let (loaded, _dir, path) = migrate(&format!("{MINIMAL}{old}"));
        assert!(loaded.unwrap().manual.is_none());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), MINIMAL);
        assert!(backup_of(&path).exists());
    }
    let named = format!(
        "{MINIMAL}{MANUAL}claude_token_file = \"/mine\"\n[measurement]\ntoken_file = \"/t\"\n"
    );
    let (loaded, _dir, path) = migrate(&named);
    assert_eq!(
        loaded.unwrap().manual.unwrap().claude_token_file,
        Path::new("/mine")
    );
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        format!("{MINIMAL}{MANUAL}claude_token_file = \"/mine\"\n")
    );
}
/// Manual sessions with no token file anywhere: no path is invented. The setting is
/// removed, the file is whole, and the load fails naming the key that is needed.
#[test]
fn manual_sessions_without_any_token_file_fail_naming_the_key() {
    for old in [
        "[measurement]\nenabled = false\n",
        "[measurement]\ntoken_file = \"\"\n",
    ] {
        let (loaded, dir, path) = migrate(&format!("{MINIMAL}{MANUAL}{old}"));
        let e = format!("{:#}", loaded.unwrap_err());
        assert!(
            e.contains("missing field `claude_token_file`") && e.contains(path.to_str().unwrap()),
            "{e}"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            format!("{MINIMAL}{MANUAL}")
        );
        assert_eq!(
            names(dir.path()),
            ["service.toml", "service.toml.before-measurement-removal"]
        );
    }
}
/// A file without the setting is not touched: same bytes, same file, no copy. Any other
/// unknown key is still refused, and so is a document that is not TOML.
#[test]
fn a_file_without_the_measurement_setting_is_not_rewritten() {
    use std::os::unix::fs::MetadataExt;
    let text = format!("# kept\n{MINIMAL}{MANUAL}claude_token_file = \"/t\"\n");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("service.toml");
    write_private(&path, text.as_bytes()).unwrap();
    let before = std::fs::metadata(&path).unwrap();
    ServiceConfig::load(&path).unwrap();
    let after = std::fs::metadata(&path).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
    assert_eq!(
        (before.ino(), before.mtime(), before.mtime_nsec()),
        (after.ino(), after.mtime(), after.mtime_nsec())
    );
    assert_eq!(names(dir.path()), ["service.toml"]);
    let e = format!(
        "{:#}",
        load_text(&format!("{MINIMAL}[other]\n")).unwrap_err()
    );
    assert!(e.contains("unknown field `other`"), "{e}");
    let e = format!(
        "{:#}",
        load_text(&format!("{MINIMAL}{MANUAL}token_file = \"/t\"\n")).unwrap_err()
    );
    assert!(e.contains("unknown field `token_file`"), "{e}");
    let (broken, dir, _path) = migrate("[measurement\nslots = ");
    assert!(broken.is_err());
    assert_eq!(names(dir.path()), ["service.toml"]);
}
/// The service and the terminal relays load the same file and may start together: every
/// one of them loads, and what is left is one whole file and one copy of the old one.
#[test]
fn programs_starting_together_leave_one_whole_file() {
    let old = format!("{MINIMAL}[measurement]\ntoken_file = \"/t\"\n{MANUAL}");
    for _ in 0..20 {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("service.toml");
        write_private(&path, old.as_bytes()).unwrap();
        let start = std::sync::Barrier::new(8);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    start.wait();
                    let loaded = ServiceConfig::load(&path).unwrap();
                    assert_eq!(loaded.manual.unwrap().claude_token_file, Path::new("/t"));
                });
            }
        });
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            format!("{MINIMAL}{MANUAL}claude_token_file = \"/t\"\n")
        );
        assert_eq!(std::fs::read_to_string(backup_of(&path)).unwrap(), old);
        assert_eq!(
            names(dir.path()),
            ["service.toml", "service.toml.before-measurement-removal"]
        );
    }
}
/// A directory the worker cannot write to: the error names the file, which is unchanged.
#[test]
fn a_file_that_cannot_be_rewritten_is_refused_naming_it() {
    use std::os::unix::fs::PermissionsExt;
    if unsafe { libc::geteuid() } == 0 {
        return; // root writes anywhere
    }
    let old = format!("{MINIMAL}[measurement]\nenabled = false\n");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("service.toml");
    write_private(&path, old.as_bytes()).unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
    let e = format!("{:#}", ServiceConfig::load(&path).unwrap_err());
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_rewrite_error(&e, &path);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), old);
    assert_eq!(names(dir.path()), ["service.toml"]);
}
fn assert_rewrite_error(e: &str, path: &Path) {
    assert!(
            e.contains(&format!(
                "{} still has the measurement setting, which was removed, and could not be rewritten without it",
                path.display()
            )),
            "{e}"
        );
    assert!(e.contains("Permission denied"), "{e}");
}
