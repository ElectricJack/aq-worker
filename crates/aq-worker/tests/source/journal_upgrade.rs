#[test]
fn source_v3_upgrade_preserves_retained_work_without_inventing_transfer_progress() {
    let (_peer, temp, rt, id) = upload_fixture();
    drop(rt);
    let file = temp.path().join("journal.json");
    let mut old: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    old["format_version"] = json!(3);
    let source = old["attempts"][id.to_string()]["source"]
        .as_object_mut()
        .unwrap();
    source.remove("upload");
    source.remove("download");
    source.insert("transfer".into(), Value::Null);
    std::fs::write(&file, serde_json::to_vec(&old).unwrap()).unwrap();
    let old_limits = || legacy_v3::Limits {
        capacity: limits().capacity,
        max_pending_results: 16,
        max_pending_bytes: 256 * 1024 * 1024,
        max_preparing: 2,
        minimum_free_disk_bytes: 0,
    };
    let before =
        legacy_v3::Journal::open(temp.path(), sample_offer().identity, old_limits()).unwrap();
    drop(before);
    let old: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    let new = runtime(temp.path());
    assert_upgraded_source(new.journal.state().attempts[&id].source.as_ref().unwrap());
    drop(new);
    let bytes = std::fs::read(&file).unwrap();
    let current: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(old["attempts"], current["attempts"]);
    assert_eq!(current["format_version"], 4);
    assert!(legacy_v3::Journal::open(temp.path(), sample_offer().identity, old_limits()).is_err());
    assert_eq!(std::fs::read(file).unwrap(), bytes);
}
fn assert_upgraded_source(source: &aq_worker::source::SourceState) {
    assert!(source.upload.is_none() && source.download.is_none() && source.transfer.is_none());
    assert!(source.exported.is_some() && source.checkpoint.is_some());
}
