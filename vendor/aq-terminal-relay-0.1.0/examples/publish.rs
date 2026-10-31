#[path = "../tests/support/mod.rs"]
mod support;
fn write(dir: &std::path::Path, artifacts: std::collections::BTreeMap<String, String>) {
    std::fs::create_dir_all(dir).unwrap();
    for (name, bytes) in artifacts {
        std::fs::write(dir.join(name), bytes).unwrap();
    }
}
fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args.next().expect("destination directory required");
    write(std::path::Path::new(&dir), support::artifacts());
    if let Some(flag) = args.next() {
        assert_eq!(flag, "--crate-artifacts");
        assert!(args.next().is_none());
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        write(&root.join("artifacts"), support::artifacts());
        write(
            &root.join("../aq-terminal-relay-protocol/artifacts"),
            support::peer_artifacts(),
        );
    }
}
