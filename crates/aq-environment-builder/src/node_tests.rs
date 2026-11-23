//! Against an axum double of `GET /v1/worker/environments` and `POST …/ready`.
use super::*;
use axum::{Json, Router, routing::get, routing::post};
use serde_json::{Value, json};
use std::sync::Mutex as StdMutex;

#[derive(Default)]
struct Double {
    admitted: Value,
    reports: Vec<Value>,
    /// Send the list as a stream, without Content-Length.
    chunked: bool,
}

async fn serve(double: Arc<StdMutex<Double>>) -> Server {
    let listed = Arc::clone(&double);
    let list = move |h: axum::http::HeaderMap| async move {
        use axum::response::IntoResponse as _;
        assert_eq!(h["authorization"], "Bearer node-secret");
        let d = listed.lock().unwrap();
        let body = json!({"environments": d.admitted.clone()});
        if !d.chunked {
            return Json(body).into_response();
        }
        let chunk = Ok::<_, std::io::Error>(axum::body::Bytes::from(body.to_string()));
        axum::body::Body::from_stream(futures_util::stream::iter([chunk])).into_response()
    };
    let ready = move |Json(body): Json<Value>| async move {
        double.lock().unwrap().reports.push(body.clone());
        Json(body)
    };
    let app = Router::new()
        .route("/v1/worker/environments", get(list))
        .route("/v1/worker/environments/ready", post(ready));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Server {
        url,
        credential: "node-secret".into(),
        cf_access: None,
        max_reply_bytes: crate::default_reply_bytes(),
    }
}

fn digest(c: char) -> String {
    format!("sha256:{}", c.to_string().repeat(64))
}
fn admitted(name: &str, c: char) -> Value {
    json!({
        "id": format!("proj.{name}"), "version": "1", "digest": digest(c),
        "image": format!("registry.example/aq-template@{}", digest(c)),
        "harnesses": [{"harness": "claude", "version": "2.1.318"}],
        "warm": ["pnpm install --frozen-lockfile"],
        "definition": {"id": format!("proj.{name}"), "version": "1",
            "digest": "0".repeat(63) + &c.to_string()},
    })
}
fn definition(value: &Value) -> ArtifactRef {
    serde_json::from_value(value["definition"].clone()).unwrap()
}
fn boot() -> ArtifactRef {
    serde_json::from_value(json!({"id": "aq-base", "version": "1", "digest": "d".repeat(64)}))
        .unwrap()
}

/// A daemon double: records pulls and removals; `refuse` digests fail to pull,
/// and `in_use` digests are still used by a container.
#[derive(Clone, Default)]
struct Daemon {
    pulled: Arc<StdMutex<Vec<String>>>,
    removed: Arc<StdMutex<Vec<String>>>,
    refuse: Vec<String>,
    in_use: Arc<StdMutex<Vec<String>>>,
}
impl Templates for Daemon {
    fn pull(
        &mut self,
        environment: &AdmittedEnvironment,
    ) -> impl Future<Output = anyhow::Result<()>> + Send {
        self.pulled.lock().unwrap().push(environment.digest.clone());
        let refused = self.refuse.contains(&environment.digest);
        std::future::ready(match refused {
            true => Err(anyhow::anyhow!("pull refused")),
            false => Ok(()),
        })
    }
    fn remove(
        &mut self,
        environment: &AdmittedEnvironment,
    ) -> impl Future<Output = anyhow::Result<Removal>> + Send {
        let digest = environment.digest.clone();
        std::future::ready(Ok(match self.in_use.lock().unwrap().contains(&digest) {
            true => Removal::InUse,
            false => {
                self.removed.lock().unwrap().push(digest);
                Removal::Removed
            }
        }))
    }
}
const POLL: Poll = Poll {
    interval: Duration::ZERO,
    max_list_bytes: 1 << 20,
};

#[tokio::test]
async fn admitted_environments_are_pulled_and_reported_ready() {
    let double = Arc::new(StdMutex::new(Double::default()));
    let (web, api) = (admitted("web", 'a'), admitted("api", 'e'));
    double.lock().unwrap().admitted = json!([web, api]);
    let server = serve(Arc::clone(&double)).await;
    let puller = Daemon {
        refuse: vec![digest('e')],
        ..Daemon::default()
    };
    let held = Held::default();
    let (stop, shutdown) = tokio::sync::watch::channel(false);
    let polling = tokio::spawn({
        let (held, daemon) = (held.clone(), puller.clone());
        async move { hold(&server, held, POLL, shutdown, daemon).await }
    });
    tokio::time::timeout(Duration::from_secs(10), async {
        while double.lock().unwrap().reports.len() < 2 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    stop.send_replace(true);
    polling.await.unwrap().unwrap();
    // Only what was pulled is reported; a pulled digest is never pulled again,
    // and one that failed is tried again on the next poll.
    let reports = double.lock().unwrap().reports.clone();
    assert_eq!(reports[0], json!({"ready": [digest('a')]}));
    assert_eq!(reports[1], json!({"ready": [digest('a')]}));
    let pulled = puller.pulled.lock().unwrap().clone();
    assert_eq!(pulled.iter().filter(|d| **d == digest('a')).count(), 1);
    assert!(pulled.iter().filter(|d| **d == digest('e')).count() >= 2);
    // Held for launches, with `warm` kept for the build cache.
    let entry = held.get(&definition(&web)).unwrap();
    assert_eq!(
        entry.image,
        format!("registry.example/aq-template@{}", digest('a'))
    );
    assert_eq!(entry.warm, ["pnpm install --frozen-lockfile"]);
    assert_eq!(entry.harnesses[0].harness, "claude");
    assert!(held.get(&definition(&api)).is_none());
}

#[tokio::test]
async fn a_template_an_attempt_still_uses_is_not_dropped_when_it_leaves_the_list() {
    let double = Arc::new(StdMutex::new(Double::default()));
    let (web, api) = (admitted("web", 'a'), admitted("api", 'e'));
    double.lock().unwrap().admitted = json!([web, api]);
    let client = Client::new(&serve(Arc::clone(&double)).await).unwrap();
    let (held, mut daemon, mut retired) = (Held::default(), Daemon::default(), vec![]);
    refresh(&client, &held, POLL, &mut daemon, &mut retired)
        .await
        .unwrap();
    // An attempt runs in web; then both leave the admitted list.
    let lease = held.select(&boot(), &definition(&web)).unwrap();
    assert!(lease.is_some());
    double.lock().unwrap().admitted = json!([]);
    refresh(&client, &held, POLL, &mut daemon, &mut retired)
        .await
        .unwrap();
    assert_eq!(double.lock().unwrap().reports[1], json!({"ready": []}));
    assert!(held.get(&definition(&web)).is_some(), "still in use");
    assert!(held.get(&definition(&api)).is_none(), "unused, so dropped");
    // The unused image is removed; the leased one is kept.
    assert_eq!(*daemon.removed.lock().unwrap(), [digest('e')]);
    // Once the attempt ends, the next poll drops it and removes its image.
    drop(lease);
    refresh(&client, &held, POLL, &mut daemon, &mut retired)
        .await
        .unwrap();
    assert!(held.get(&definition(&web)).is_none());
    assert!(held.select(&boot(), &definition(&web)).is_err());
    assert_eq!(*daemon.removed.lock().unwrap(), [digest('e'), digest('a')]);
    // Re-admitted: it is pulled again.
    double.lock().unwrap().admitted = json!([web]);
    refresh(&client, &held, POLL, &mut daemon, &mut retired)
        .await
        .unwrap();
    assert_eq!(
        double.lock().unwrap().reports[3],
        json!({"ready": [digest('a')]})
    );
    let pulled = daemon.pulled.lock().unwrap().clone();
    assert_eq!(pulled, [digest('a'), digest('e'), digest('a')]);
}

#[tokio::test]
async fn an_image_a_container_still_uses_is_kept_and_removed_once_it_is_not() {
    let double = Arc::new(StdMutex::new(Double::default()));
    let (web, api) = (admitted("web", 'a'), admitted("api", 'e'));
    double.lock().unwrap().admitted = json!([web, api]);
    let client = Client::new(&serve(Arc::clone(&double)).await).unwrap();
    let (held, mut daemon, mut retired) = (Held::default(), Daemon::default(), vec![]);
    refresh(&client, &held, POLL, &mut daemon, &mut retired)
        .await
        .unwrap();
    // A preserved sandbox (no lease, e.g. across a restart) still uses web's image.
    daemon.in_use.lock().unwrap().push(digest('a'));
    double.lock().unwrap().admitted = json!([]);
    refresh(&client, &held, POLL, &mut daemon, &mut retired)
        .await
        .unwrap();
    assert_eq!(*daemon.removed.lock().unwrap(), [digest('e')]);
    assert!(held.get(&definition(&web)).is_none(), "no longer offered");
    // It is tried again every poll, and removed once the container is gone.
    refresh(&client, &held, POLL, &mut daemon, &mut retired)
        .await
        .unwrap();
    assert_eq!(*daemon.removed.lock().unwrap(), [digest('e')]);
    daemon.in_use.lock().unwrap().clear();
    refresh(&client, &held, POLL, &mut daemon, &mut retired)
        .await
        .unwrap();
    assert_eq!(*daemon.removed.lock().unwrap(), [digest('e'), digest('a')]);
    assert!(retired.is_empty());
}

#[tokio::test]
async fn an_admitted_list_over_the_configured_cap_is_refused_naming_both_sizes() {
    let double = Arc::new(StdMutex::new(Double::default()));
    double.lock().unwrap().admitted = json!([admitted("web", 'a')]);
    let client = Client::new(&serve(Arc::clone(&double)).await).unwrap();
    let (held, mut daemon, mut retired) = (Held::default(), Daemon::default(), vec![]);
    let size = json!({"environments": [admitted("web", 'a')]})
        .to_string()
        .len() as u64;
    let tight = Poll {
        max_list_bytes: size - 1,
        ..POLL
    };
    let refused = refresh(&client, &held, tight, &mut daemon, &mut retired).await;
    let message = refused.unwrap_err().to_string();
    assert!(message.contains(&format!("{size} bytes")), "{message}");
    assert!(
        message.contains(&format!("cap of {} bytes", size - 1)),
        "{message}"
    );
    assert!(daemon.pulled.lock().unwrap().is_empty());
    assert!(
        double.lock().unwrap().reports.is_empty(),
        "nothing reported"
    );
    let fits = Poll {
        max_list_bytes: size,
        ..POLL
    };
    refresh(&client, &held, fits, &mut daemon, &mut retired)
        .await
        .unwrap();
    assert_eq!(*daemon.pulled.lock().unwrap(), [digest('a')]);
}

#[test]
fn an_offer_selects_the_boot_template_or_a_held_environment_and_nothing_else() {
    let held = Held::default();
    let web: AdmittedEnvironment = serde_json::from_value(admitted("web", 'a')).unwrap();
    held.replace(vec![web.clone()]);
    assert!(held.select(&boot(), &boot()).unwrap().is_none());
    assert!(held.select(&boot(), &web.definition).unwrap().is_some());
    let unknown = definition(&admitted("other", 'f'));
    let refused = held.select(&boot(), &unknown).unwrap_err();
    assert!(refused.to_string().contains("does not hold"), "{refused:#}");
}

#[tokio::test]
async fn replies_over_the_configured_caps_are_refused_with_the_bytes_seen() {
    let double = Arc::new(StdMutex::new(Double::default()));
    double.lock().unwrap().admitted = json!([admitted("web", 'a')]);
    let tight = Server {
        max_reply_bytes: 8,
        ..serve(Arc::clone(&double)).await
    };
    let client = Client::new(&tight).unwrap();
    // The claim, complete and ready replies are read up to `max_reply_bytes`.
    let reply = json!({"ready": [digest('a')]}).to_string().len();
    let refused = client.ready(&[digest('a')]).await.unwrap_err().to_string();
    assert!(
        refused.contains(&format!("is {reply} bytes, over the cap of 8 bytes")),
        "{refused}"
    );
    // Without a Content-Length, the error carries the bytes read when the cap was passed.
    double.lock().unwrap().chunked = true;
    let size = json!({"environments": [admitted("web", 'a')]})
        .to_string()
        .len() as u64;
    let refused = client.admitted(size - 1).await.unwrap_err().to_string();
    let expected = format!(
        "(no Content-Length) reached {size} bytes, over the cap of {} bytes",
        size - 1
    );
    assert!(refused.contains(&expected), "{refused}");
    assert_eq!(client.admitted(size).await.unwrap().len(), 1);
}

#[test]
fn the_builder_reply_cap_is_a_bounded_setting() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("environments.json");
    let config = |extra: &str| {
        let git = serde_json::to_string(&aq_node_git::test_support::config(dir.path())).unwrap();
        let text =
            format!(r#"{{"build_config": "/b.json", "work_dir": "/w", "node_git": {git}{extra}}}"#);
        std::fs::write(&path, text).unwrap();
        crate::EnvironmentsConfig::load(&path)
    };
    match config("") {
        Ok(c) => assert_eq!(c.max_reply_bytes, 1024 * 1024),
        Err(e) => panic!("{e:#}"),
    }
    assert_eq!(
        config(r#", "max_reply_bytes": 2097152"#)
            .unwrap()
            .max_reply_bytes,
        2 << 20
    );
    for bad in ["0", "1024", "1073741824"] {
        let refused = config(&format!(r#", "max_reply_bytes": {bad}"#)).unwrap_err();
        assert!(
            refused.to_string().contains("max_reply_bytes"),
            "{refused:#}"
        );
    }
}
