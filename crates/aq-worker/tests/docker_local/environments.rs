/// Spec 0017: a node runs each attempt in the template of its offer's environment.
#[cfg(feature = "project_environments")]
mod environments {
    use super::*;
    use aq_environment_builder::node::{AdmittedEnvironment, Held};
    use aq_session_protocol::ArtifactRef;

    pub(super) fn admitted(c: char) -> AdmittedEnvironment {
        let digest = format!("sha256:{}", c.to_string().repeat(64));
        serde_json::from_value(json!({
            "id": "proj.web", "version": c.to_string(), "digest": digest,
            "image": format!("registry.example/aq-template@{digest}"),
            "harnesses": [{"harness": "claude", "version": "2.1.318"}], "warm": [],
            "definition": {"id": "proj.web", "version": c.to_string(), "digest": "0".repeat(63) + &c.to_string()},
        }))
        .unwrap()
    }
    fn boot() -> ArtifactRef {
        serde_json::from_value(json!({"id": "aq-base", "version": "1", "digest": "d".repeat(64)}))
            .unwrap()
    }
    fn held(environments: Vec<AdmittedEnvironment>) -> Held {
        let held = Held::default();
        held.replace(environments);
        held
    }

    #[tokio::test]
    async fn an_offer_runs_in_the_template_of_its_environment() {
        let web = admitted('e');
        let held = held(vec![web.clone()]);
        let h = harness_with(ScriptedDocker::new(), |c| c.environments = held.clone()).await;
        // As the launch does: the attempt's copy of the node's configuration.
        let mut config = h.backend.config().clone();
        assert_eq!(
            config.offered_template(&boot(), &digest()).unwrap(),
            digest()
        );
        let template = config.offered_template(&web.definition, &digest()).unwrap();
        assert_eq!(template.as_str(), web.digest);
        assert_eq!(
            config.template_for(&web.definition),
            Some(&AllowedTemplate {
                image: web.image.clone(),
                digest: template.clone()
            })
        );
        let mut backend = bounded(DockerLocalBackend::connect(config, h.docker.clone()))
            .await
            .unwrap();
        bounded(backend.pull_template(&template)).await.unwrap();
        let mut request = request(1, 2, 3);
        request.template_digest = template;
        bounded(backend.create(&request)).await.unwrap();
        let body = h.docker.container(&request.name).unwrap().body;
        assert_eq!(body["Image"], web.image);
    }

    #[tokio::test]
    async fn an_offer_for_an_unadmitted_environment_is_refused_not_run_in_the_default() {
        let (web, other) = (admitted('e'), admitted('f'));
        let held = held(vec![web.clone()]);
        // The executor refuses it before launch; the boot and held ones pass.
        let refused = held.select(&boot(), &other.definition).unwrap_err();
        assert!(refused.to_string().contains("does not hold"), "{refused:#}");
        assert!(held.select(&boot(), &boot()).unwrap().is_none());
        assert!(held.select(&boot(), &web.definition).unwrap().is_some());
        let mut h = harness_with(ScriptedDocker::new(), |c| c.environments = held.clone()).await;
        let mut config = h.backend.config().clone();
        assert_eq!(config.template_for(&other.definition), None);
        let mut request = request(1, 2, 3);
        request.template_digest = TemplateDigest::parse(other.digest.clone()).unwrap();
        assert_eq!(
            bounded(h.backend.create(&request)).await,
            Err(NOT_ALLOWLISTED)
        );
        // An admitted template is checked like a static one: pinned to its digest.
        let unpinned = AdmittedEnvironment {
            image: "registry.example/aq-template:latest".into(),
            ..other
        };
        assert!(config.admit(&[unpinned]).is_err());
        assert!(
            config
                .templates
                .iter()
                .all(|t| t.image.ends_with(t.digest.as_str()))
        );
    }
}

/// Spec 0017 §6.7: a template that left the admitted list is removed from the
/// daemon once no container (running, stopped or preserved) uses it. The node's
/// own templates are never removed.
#[cfg(feature = "project_environments")]
#[tokio::test]
async fn a_dropped_template_is_removed_unless_a_container_still_uses_it() {
    use aq_environment_builder::node::Removal;
    let (used, unused) = (environments::admitted('e'), environments::admitted('f'));
    let h = harness().await;
    let mut config = h.backend.config().clone();
    config.admit(&[used.clone(), unused.clone()]).unwrap();
    let docker = ScriptedDocker::new();
    let mut node = bounded(DockerLocalBackend::connect(config, docker.clone()))
        .await
        .unwrap();
    let digest_of = |e: &aq_environment_builder::node::AdmittedEnvironment| {
        TemplateDigest::parse(e.digest.clone()).unwrap()
    };
    for environment in [&used, &unused] {
        bounded(node.pull_template(&digest_of(environment)))
            .await
            .unwrap();
    }
    let mut request = request(1, 2, 3);
    request.template_digest = digest_of(&used);
    bounded(node.create(&request)).await.unwrap();
    docker.set_status(&request.name, "exited");
    // The poll removes with the node's own configuration: the static templates only.
    let static_only = h.backend.config().clone();
    let mut backend = bounded(DockerLocalBackend::connect(static_only, docker.clone()))
        .await
        .unwrap();
    let remove =
        |e: &aq_environment_builder::node::AdmittedEnvironment| (digest_of(e), e.image.clone());
    let (digest, image) = remove(&used);
    let outcome = bounded(backend.remove_template(&digest, &image))
        .await
        .unwrap();
    assert_eq!(outcome, Removal::InUse, "a stopped container still uses it");
    assert!(docker.has_image(&used.image));
    let delete = (
        DockerMethod::Delete,
        format!("/v1.44/images/{}", used.image),
    );
    assert!(
        !docker.requests().contains(&delete),
        "the daemon is asked first"
    );
    let (digest, image) = remove(&unused);
    let outcome = bounded(backend.remove_template(&digest, &image))
        .await
        .unwrap();
    assert_eq!(outcome, Removal::Removed);
    assert!(!docker.has_image(&unused.image));
    assert!(
        docker
            .requests()
            .contains(&(DockerMethod::Delete, format!("/v1.44/images/{image}")))
    );
    // Once the sandbox is released, its image goes too.
    docker.remove_container(&request.name);
    let (digest, image) = remove(&used);
    assert_eq!(
        bounded(backend.remove_template(&digest, &image))
            .await
            .unwrap(),
        Removal::Removed
    );
    // Absent already: nothing left to do.
    assert_eq!(
        bounded(backend.remove_template(&digest, &image))
            .await
            .unwrap(),
        Removal::Removed
    );
    // A static template is never removed, and the daemon is not asked.
    bounded(backend.pull_template(&self::digest()))
        .await
        .unwrap();
    docker.clear_requests();
    let outcome = bounded(backend.remove_template(&self::digest(), &self::image()))
        .await
        .unwrap();
    assert_eq!(outcome, Removal::Kept);
    assert!(docker.requests().is_empty());
    assert!(docker.has_image(&self::image()));
}

/// A template image reaches daemon API paths, and an admitted one comes from the
/// server: only an OCI reference pinned to its digest is accepted.
#[test]
fn a_template_image_is_a_plain_reference_pinned_to_its_digest() {
    let pinned = |name: &str| format!("{name}@sha256:{HEX}");
    for good in [
        "docker.io/library/busybox",
        "127.0.0.1:5138/electricjack/aq-template",
        "localhost/aq-template",
        "registry.example/aq-template:v1.2",
        "busybox",
        "ghcr.io/a__b/c-d.e",
    ] {
        assert!(pinned_reference(&pinned(good), &digest()), "{good}");
    }
    for bad in [
        "registry.example/aq-template?force=1",
        "registry.example/aq-template&force=1",
        "registry.example/aq#x",
        "registry.example/a b",
        "registry.example/a\nb",
        "registry.example/Upper",
        "registry.example/x/",
        "registry.example:/x",
        "",
        "x:",
        "x@sha256:abc",
    ] {
        assert!(!pinned_reference(&pinned(bad), &digest()), "{bad:?}");
    }
    let other = TemplateDigest::parse(format!("sha256:{}", "c".repeat(64))).unwrap();
    assert!(!pinned_reference(&image(), &other), "another digest");
    assert!(
        !pinned_reference(&format!("busybox@{}", HEX), &digest()),
        "no algorithm"
    );
}

#[cfg(feature = "project_environments")]
#[tokio::test]
async fn an_image_name_carrying_query_parameters_is_refused_without_a_daemon_call() {
    let environment = environments::admitted('e');
    let digest = TemplateDigest::parse(environment.digest.clone()).unwrap();
    let mut h = harness().await;
    let forced = format!(
        "registry.example/aq-template?force=1@{}",
        environment.digest
    );
    let refused = bounded(h.backend.remove_template(&digest, &forced)).await;
    assert!(
        matches!(refused, Err(BackendError::InvalidRequest(_))),
        "{refused:?}"
    );
    assert!(h.docker.requests().is_empty());
    let mut config = h.backend.config().clone();
    let unsafe_entry = aq_environment_builder::node::AdmittedEnvironment {
        image: forced,
        ..environment
    };
    assert!(config.admit(&[unsafe_entry]).is_err());
}
