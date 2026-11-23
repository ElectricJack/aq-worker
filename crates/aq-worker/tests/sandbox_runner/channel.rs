use super::support::Fixture;
use aq_worker::native::runner::sandbox::{
    auth::{RunnerHttp, RunnerRoute},
    permit_transport::{PermitTransport, RunnerExchange},
    *,
};
struct RefusingChannel;
impl RunnerExchange for RefusingChannel {
    fn preflight(&mut self, _: &RunnerHttp) -> anyhow::Result<()> {
        Ok(())
    }
    fn renew<'a>(
        &'a mut self,
        http: &'a RunnerHttp,
        request: &'a aq_admission_sandbox::sandbox::RunnerRenewalRequest,
        _: uuid::Uuid,
    ) -> TransportFuture<'a, RenewalOutcome> {
        Box::pin(async move {
            let _ = http
                .post(
                    RunnerRoute::Observations,
                    &request.permit.binding,
                    &serde_json::json!({"binding":request.permit.binding}),
                )
                .await;
            // Another channel revoked authority; renewal itself never completes.
            std::future::pending().await
        })
    }
    fn retain<'a>(
        &'a mut self,
        _: &'a RunnerHttp,
        _: &'a StoppedAttempt,
    ) -> TransportFuture<'a, RetentionReceipts> {
        Box::pin(async { panic!("authority loss must preserve before retention") })
    }
    fn finished<'a>(
        &'a mut self,
        _: &'a RunnerHttp,
        _: &'a RetainedAttempt,
    ) -> TransportFuture<'a, FinishedReceipt> {
        Box::pin(async { panic!("authority loss must never finish") })
    }
}

async fn refuse_foreign_permit(f: &Fixture, origin: &str) {
    // A valid client for a different sandbox cannot launch this frozen attempt.
    let mut foreign_permit = f.permit.clone();
    foreign_permit.binding.sandbox_id = uuid::Uuid::new_v4();
    let foreign_files =
        private::RunnerFiles::create(&f.dir.path().join("foreign-private")).unwrap();
    foreign_files
        .write_secret("permit.json", &serde_json::to_vec(&foreign_permit).unwrap())
        .unwrap();
    let foreign_http =
        RunnerHttp::for_execution(origin, &foreign_files.root().join("permit.json"), &f.frozen)
            .unwrap();
    let mut foreign_transport = PermitTransport::new(foreign_http, RefusingChannel);
    assert!(f.run(&mut foreign_transport).await.is_err());
    assert!(!f.dir.path().join("sandbox-run").exists());
}

#[tokio::test]
async fn runner_channel_refusal_stops_and_preserves() {
    let mut f = Fixture::new(true, 5000);
    f.worker.credential.clear();
    let files = private::RunnerFiles::create(&f.dir.path().join("runner-private")).unwrap();
    files
        .write_secret("permit.json", &serde_json::to_vec(&f.permit).unwrap())
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let router = axum::Router::new().fallback(|| async { axum::http::StatusCode::FORBIDDEN });
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let http =
        RunnerHttp::for_execution(&origin, &files.root().join("permit.json"), &f.frozen).unwrap();
    refuse_foreign_permit(&f, &origin).await;
    let retained = f.local.workspace.join("retained-before-launch");
    std::fs::write(&retained, b"unretained work").unwrap();
    let mut transport = PermitTransport::new(http.clone(), RefusingChannel);
    let result = f.run(&mut transport).await.unwrap();
    server.abort();
    assert_eq!(
        result,
        RunOutcome::Preserved {
            reason: "runner authority lost",
            stop_proven: true
        }
    );
    assert!(http.authority_lost());
    assert_eq!(std::fs::read(retained).unwrap(), b"unretained work");
    assert!(
        f.run(&mut transport).await.is_err(),
        "preserved attempt must never relaunch"
    );
}
