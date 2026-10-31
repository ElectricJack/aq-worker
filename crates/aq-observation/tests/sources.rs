use aq_observation::{lifecycle::Text, source::*};
use std::{
    io::Write,
    os::unix::fs::{PermissionsExt, symlink},
};
use uuid::Uuid;
fn file(temp: &tempfile::TempDir) -> SourceFile {
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let permit = SourceFile::inspect(
        Uuid::from_u128(1),
        temp.path(),
        "native.jsonl",
        Text::new("expected-native-key").unwrap(),
    )
    .unwrap();
    SourceFile::open(permit).unwrap()
}
#[test]
fn scoped_reader_keeps_partial_bytes_and_refuses_rotation_missing_and_truncation() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("native.jsonl");
    std::fs::write(&path, b"{}\npartial").unwrap();
    let source = file(&temp);
    let ReadOutcome::Ready(chunk) = source.read(&Cursor::default(), 256).unwrap() else {
        panic!()
    };
    let cursor = chunk.cursor_after(3).unwrap();
    assert_eq!(cursor.offset, 3);
    let ReadOutcome::Ready(chunk) = source.read(&cursor, 256).unwrap() else {
        panic!()
    };
    assert_eq!(chunk.bytes, b"partial");
    std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&path)
        .unwrap()
        .write_all(b"x")
        .unwrap();
    assert!(matches!(
        source.read(&cursor, 256).unwrap(),
        ReadOutcome::Changed(SourceChange::Truncated)
    ));
    std::fs::rename(&path, temp.path().join("old.jsonl")).unwrap();
    std::fs::write(&path, b"other native source").unwrap();
    assert!(matches!(
        source.read(&cursor, 256).unwrap(),
        ReadOutcome::Changed(SourceChange::Rotated)
    ));
    std::fs::remove_file(&path).unwrap();
    assert!(matches!(
        source.read(&cursor, 256).unwrap(),
        ReadOutcome::Changed(SourceChange::Missing)
    ));
}
#[test]
fn bounded_anchor_detects_rewrite_and_symlink_never_reads_target() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("native.jsonl");
    std::fs::write(&path, b"first record\n").unwrap();
    let source = file(&temp);
    let ReadOutcome::Ready(chunk) = source.read(&Cursor::default(), 256).unwrap() else {
        panic!()
    };
    let cursor = chunk.cursor_after(chunk.bytes.len()).unwrap();
    std::fs::write(&path, b"other record\n").unwrap();
    assert!(matches!(
        source.read(&cursor, 256).unwrap(),
        ReadOutcome::Changed(SourceChange::AnchorChanged)
    ));
    std::fs::remove_file(&path).unwrap();
    let unrelated = temp.path().join("unrelated");
    std::fs::write(&unrelated, b"not admitted").unwrap();
    symlink(&unrelated, &path).unwrap();
    assert!(source.read(&Cursor::default(), 256).is_err());
    assert!(
        SourceFile::inspect(
            Uuid::from_u128(1),
            temp.path(),
            "../unrelated",
            Text::new("key").unwrap()
        )
        .is_err()
    );
}
