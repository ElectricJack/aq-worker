//! Explicit local public-Git plus HTTP qualification; no live daemon/provider.
use super::source_repository::Fixture;
use super::*;
use aq_worker::{
    config::WorkerConfig, execution_client::ExecutionClient, source_transfer::UploadAction,
    transfer_client::TransferClient,
};
use std::{
    collections::BTreeMap,
    io::Read,
    net::{TcpListener, TcpStream},
    sync::atomic::{AtomicBool, Ordering},
};
#[path = "source_http/report.rs"]
mod report;
#[path = "source_http/retention.rs"]
mod retention;
#[path = "source_http/server.rs"]
mod server;
#[path = "source_http/work.rs"]
mod work;
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires explicit AQ_GIT_TEST_BINARY pinned public binary; local HTTP qualification"]
async fn pinned_git_http_retention_lost_finalize_and_report_restart() {
    let exe = std::env::var_os("AQ_GIT_TEST_BINARY").expect("set pinned executable");
    let exe = Path::new(&exe);
    let temp = TempDir::new().unwrap();
    let fixture = source_repository::create_fixture(exe, temp.path());
    let (work, rt) = work::Work::prepare(&fixture, temp.path());
    let server = server::Server::start(fixture.clone());
    let config = WorkerConfig {
        server_url: format!("http://{}", server.addr),
        node_id: rt.journal.state().identity.node_id,
        credential: "fixture-http-node".into(),
        cf_client_id: None,
        cf_client_secret: None,
    };
    let rt = work.finish_upload(rt, &config, &server.state).await;
    work.report_and_retire(rt, &config, &server.state).await;
    work.assert_central_result(&fixture);
    server.stop();
}
#[derive(Default)]
struct PeerState {
    manifest: Option<TransferManifest>,
    status: Option<TransferStatus>,
    operations: BTreeMap<Uuid, (Vec<u8>, Value)>,
    report: Option<ResultReport>,
    finalize_lost: bool,
    report_lost: bool,
}
fn respond(stream: &mut TcpStream, body: &Value, lost: bool) {
    let bytes = serde_json::to_vec(body).unwrap();
    let length = if lost { bytes.len() + 1 } else { bytes.len() };
    write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {length}\r\n\r\n").unwrap();
    stream.write_all(&bytes).unwrap();
}

fn refuse(stream: &mut TcpStream) {
    let body = b"{\"error\":\"synthetic contract refusal\"}";
    write!(stream,"HTTP/1.1 409 Conflict\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",body.len()).unwrap();
    stream.write_all(body).unwrap();
}
