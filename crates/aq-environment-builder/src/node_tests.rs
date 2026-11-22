//! Against an axum double of `GET /v1/worker/environments` and `POST …/ready`.
use super::*;
use axum::{Json, Router, routing::get, routing::post};
use serde_json::{Value, json};
use std::sync::Mutex as StdMutex;

#[derive(Default)]
struct Double {
    admitted: Value,
    reports: Vec<Value>,
}

async fn serve(double: Arc<StdMutex<Double>>) -> Server {
    let listed = Arc::clone(&double);
    let list = move |h: axum::http::HeaderMap| async move {
        assert_eq!(h["authorization"], "Bearer node-secret");
        Json(json!({"environments": listed.lock().unwrap().admitted.clone()}))
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

/// Pulls once per digest, recording each; `refuse` digests fail to pull.
#[derive(Clone, Default)]
struct Puller {
    pulled: Arc<StdMutex<Vec<String>>>,
    refuse: Vec<String>,
}
impl Puller {
    fn pull(
        &self,
    ) -> impl FnMut(AdmittedEnvironment) -> std::future::Ready<anyhow::Result<()>> + use<> {
        let me = self.clone();
        move |environment: AdmittedEnvironment| {
            me.pulled.lock().unwrap().push(environment.digest.clone());
            let refused = me.refuse.contains(&environment.digest);
            std::future::ready(match refused {
                true => Err(anyhow::anyhow!("pull refused")),
                false => Ok(()),
            })
        }
    }
}

#[tokio::test]
async fn admitted_environments_are_pulled_and_reported_ready() {
    let double = Arc::new(StdMutex::new(Double::default()));
    let (web, api) = (admitted("web", 'a'), admitted("api", 'e'));
    double.lock().unwrap().admitted = json!([web, api]);
    let server = serve(Arc::clone(&double)).await;
    let puller = Puller {
        refuse: vec![digest('e')],
        ..Puller::default()
    };
    let held = Held::default();
    let (stop, shutdown) = tokio::sync::watch::channel(false);
    let polling = tokio::spawn({
        let (held, pull) = (held.clone(), puller.pull());
        async move { hold(&server, held, Duration::ZERO, shutdown, pull).await }
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
    let (held, puller) = (Held::default(), Puller::default());
    let mut pull = puller.pull();
    refresh(&client, &held, &mut pull).await.unwrap();
    // An attempt runs in web; then both leave the admitted list.
    let lease = held.select(&boot(), &definition(&web)).unwrap();
    assert!(lease.is_some());
    double.lock().unwrap().admitted = json!([]);
    refresh(&client, &held, &mut pull).await.unwrap();
    assert_eq!(double.lock().unwrap().reports[1], json!({"ready": []}));
    assert!(held.get(&definition(&web)).is_some(), "still in use");
    assert!(held.get(&definition(&api)).is_none(), "unused, so dropped");
    // Once the attempt ends, the next poll drops it too.
    drop(lease);
    refresh(&client, &held, &mut pull).await.unwrap();
    assert!(held.get(&definition(&web)).is_none());
    assert!(held.select(&boot(), &definition(&web)).is_err());
    // Re-admitted: it is pulled again.
    double.lock().unwrap().admitted = json!([web]);
    refresh(&client, &held, &mut pull).await.unwrap();
    assert_eq!(
        double.lock().unwrap().reports[3],
        json!({"ready": [digest('a')]})
    );
    let pulled = puller.pulled.lock().unwrap().clone();
    assert_eq!(pulled, [digest('a'), digest('e'), digest('a')]);
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
