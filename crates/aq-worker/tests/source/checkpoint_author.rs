fn git_identity(path: &Path) -> String {
    use std::process::Command;
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["show", "--no-patch", "--format=%an <%ae>|%cn <%ce>"])
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn assert_checkpoint_author(rt: &Runtime<FakeClock>, source: &source_repository::PreparedSource) {
    let git = &source.guard.client;
    let id = source.id;
    let work = &source.work;
    assert_eq!(
        git_identity(&work.path),
        "AQ Worker <worker@invalid>|AQ Worker <worker@invalid>"
    );
    let caps = git.exchange(&json!({"protocol_version":1,"request_id":"checkpoint-author-test","operation":"capabilities","params":{}})).unwrap();
    if caps["git"]["checkpoint_author"] == true {
        use aq_worker::source::{GitAction, Workspace};
        use aq_worker_protocol::git_author::GitAuthor;
        let binding = &rt.journal.state().attempts[&id]
            .source
            .as_ref()
            .unwrap()
            .binding;
        let authored: Workspace = serde_json::from_value(
            git.exchange(&GitAction::Create.request(binding, Uuid::new_v4()))
                .unwrap(),
        )
        .unwrap();
        std::fs::write(authored.path.join("authored.txt"), "new source").unwrap();
        let creator = GitAuthor {
            name: "Zoë Task Creator".into(),
            email: "Owner+AQ@example.invalid".into(),
        };
        let request = GitAction::Checkpoint {
            workspace_id: authored.workspace_id,
            generation: authored.generation,
            head: binding.base.clone(),
        }
        .request_as(binding, Uuid::new_v4(), Some(&creator));
        let pushed = git.exchange(&request).unwrap();
        assert_eq!(git.exchange(&request).unwrap()["source"], pushed["source"]);
        assert_eq!(
            git_identity(&authored.path),
            "Zoë Task Creator <Owner+AQ@example.invalid>|AQ Worker <worker@invalid>"
        );
        let retire = GitAction::Retire {
            workspace_id: authored.workspace_id,
            generation: pushed["workspace_generation"].as_u64().unwrap(),
            head: serde_json::from_value(pushed["source"].clone()).unwrap(),
        }
        .request(binding, Uuid::new_v4());
        git.exchange(&retire).unwrap();
        let id = serde_json::from_value(pushed["retention_handle"].clone()).unwrap();
        let handle = git
            .query(binding, "retention.inspect", json!({"handle_id": id}))
            .unwrap();
        let release = GitAction::Release {
            handle_id: id,
            generation: handle["generation"].as_u64().unwrap(),
            refs: serde_json::from_value(handle["refs"].clone()).unwrap(),
        }
        .request(binding, Uuid::new_v4());
        git.exchange(&release).unwrap();
    }
}
