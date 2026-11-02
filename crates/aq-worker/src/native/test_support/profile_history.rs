//! Plain synthetic historical fixtures; never construct live authority.
use crate::native::journal::Attempt;
use aq_session_protocol::Digest;
use serde_json::{Value, json};
use uuid::Uuid;
/// Published synthetic protocol data only: never passed to Journal/live host
/// constructors. The shared helper cannot construct a relay or live permit.
pub(crate) fn terminal_attempt(refresh: bool, acknowledged: bool) -> Attempt {
    let (terminal, phase): (Value, Value) = if refresh {
        (serde_json::from_slice(include_bytes!("../../../../../vendor/aq-phase-terminal-protocol-0.1.1/artifacts/refresh-v1/fixtures.json")).unwrap(),
             serde_json::from_slice(include_bytes!("../../../../../vendor/aq-phase-terminal-protocol-0.1.1/tests/data/preparation-refresh-v1.fixture.json")).unwrap())
    } else {
        (serde_json::from_slice(include_bytes!("../../../../../vendor/aq-phase-terminal-protocol-0.1.0/artifacts/fixtures.json")).unwrap(),
             serde_json::from_slice(include_bytes!("../../../../../vendor/aq-phase-terminal-protocol-0.1.1/tests/data/preparation-v2.fixture.json")).unwrap())
    };
    let request = &terminal["started_request"];
    let original = &request["original"];
    let digest = Digest::of(b"synthetic history metadata, no live owner");
    let runtime = json!({"runtime":original["runtime"],"qualification":original["qualification"]});
    let intent = json!({"execution_digest":digest,"binding":original["binding"],"grant_id":original["grant_id"],"logical_guardian_id":original["guardian_id"],"launch_id":original["launch_id"],"reservation_id":original["reservation_id"],"runtime":runtime,"terminal":{"session":original["binding"]["session"],"terminal_id":Uuid::from_u128(123),"epoch":1},"host_root":"/synthetic-only","root_device":1,"root_inode":1,"pid_namespace":"pid:[1]","profile_digest":digest,"executable":original["runtime"],"helper":original["runtime"],"tmux":original["runtime"],"renderer":original["runtime"],"columns":80,"rows":24,"deadline_ns":20,"hard_deadline_ns":30,"created_ns":1,"created_revision":1,"started_operation_id":original["operation_id"],"stopped_operation_id":Uuid::from_u128(124)});
    let publication = json!({"started":{"request":request,"recorded_revision":2,"receipt":if acknowledged {terminal["started_receipt"].clone()}else{Value::Null}},"stopped":null});
    let mut host = json!({"intent":intent,"host_intent":null,"guardian":null,"start_intended":true,"baseline":null,"running":{"observation":original["observation"],"request":original,"recorded_revision":2},"started_receipt":null,"stopped":null,"uncertain":null,"terminal":null,"refresh_terminal":null});
    host[if refresh {
        "refresh_terminal"
    } else {
        "terminal"
    }] = publication;
    let reservation = json!({"offer":phase["proposal"]["original"],"runtime":runtime,"reserved_at_ns":1,"deadline_ns":2,"disk_bytes":1,"reservation_id":original["reservation_id"],"reservation_operation_id":Uuid::from_u128(125),"accept_operation_id":phase["accept"]["original"]["operation_id"],"guardian_id":original["guardian_id"],"launch_id":original["launch_id"]});
    let received = json!({"delivery":{"schema_version":2,"receipt":phase["accepted"],"delivery":{"nonce":Uuid::from_u128(126),"clock":aq_admission::transport::DeliveryClock::new(1500,3000).unwrap()}},"receipt_digest":digest,"sent_ns":1,"received_ns":2,"deadline_ns":3,"observed_before_write_ns":2,"record_revision":2});
    let acceptance = json!({"proposal":phase["proposal"],"proposal_digest":digest,"request":phase["accept"],"request_digest":digest,"created_ns":1,"original_deadline_ns":2});
    let prepared = json!({"request":phase["prepare"],"request_digest":digest,"prepared_at_ns":2,"origin":{"protocol":if refresh{"refresh"}else{"phase"},"record":received}});
    let execution = json!({"delivery":phase["delivery"],"request_digest":digest,"accepted_digest":digest,"runtime":runtime,"kernel_boot_id":Uuid::from_u128(127),"clock_source":"linux_clock_boottime","sent_ns":1,"received_ns":2,"first_send_deadline_ns":3,"deadline_ns":4,"hard_deadline_ns":5,"observed_before_write_ns":2,"record_revision":3});
    let mut attempt = json!({"reservation":reservation,"acceptance":null,"acceptance_digest":null,"preparation":null,"downloads":{},"material_downloads":{},"source":null,"prepared":null,"initial_grant":null,"phase_acceptance":null,"phase_preparation":null,"phase_prepared":null,"phase_execution":null,"refresh_acceptance":null,"refresh_preparation":null,"refresh_prepared":null,"refresh_execution":null,"native_host":host,"result":null});
    let prefix = if refresh { "refresh" } else { "phase" };
    for (suffix, value) in [
        ("acceptance", acceptance),
        ("preparation", received),
        ("prepared", prepared),
        ("execution", execution),
    ] {
        attempt[format!("{prefix}_{suffix}")] = value;
    }
    serde_json::from_value(attempt).unwrap()
}
