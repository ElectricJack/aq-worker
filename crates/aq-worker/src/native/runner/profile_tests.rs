//! Historical selection/receipt tests; these fixtures create no live authority.
use super::*;
use crate::native::test_support::profile_history::terminal_attempt;
use profile::TerminalProfile;
fn add_stopped(attempt: &mut crate::native::journal::Attempt, refresh: bool, acknowledged: bool) {
    let bytes: &[u8] = if refresh {
        include_bytes!(
            "../../../../../vendor/aq-phase-terminal-protocol-0.1.1/artifacts/refresh-v1/fixtures.json"
        )
    } else {
        include_bytes!(
            "../../../../../vendor/aq-phase-terminal-protocol-0.1.0/artifacts/fixtures.json"
        )
    };
    let fixture: serde_json::Value = serde_json::from_slice(bytes).unwrap();
    let host = attempt.native_host.as_mut().unwrap();
    if refresh {
        host.refresh_terminal.as_mut().unwrap().stopped = Some(
            crate::native::journal::refresh_terminal::StoppedPublication {
                request: serde_json::from_value(fixture["stopped_request"].clone()).unwrap(),
                recorded_revision: 3,
                receipt: if acknowledged {
                    Some(serde_json::from_value(fixture["stopped_receipt"].clone()).unwrap())
                } else {
                    None
                },
            },
        );
    } else {
        host.terminal.as_mut().unwrap().stopped =
            Some(crate::native::journal::phase_terminal::StoppedPublication {
                request: serde_json::from_value(fixture["stopped_request"].clone()).unwrap(),
                recorded_revision: 3,
                receipt: if acknowledged {
                    Some(serde_json::from_value(fixture["stopped_receipt"].clone()).unwrap())
                } else {
                    None
                },
            });
    }
}
#[test]
fn profiles_require_exact_execution_then_matching_terminal_context_and_pair() {
    for refresh in [false, true] {
        let expected = if refresh {
            TerminalProfile::Refresh
        } else {
            TerminalProfile::Frozen
        };
        let mut attempt = terminal_attempt(refresh, false);
        assert_eq!(TerminalProfile::for_execution(&attempt).unwrap(), expected);
        assert_eq!(TerminalProfile::for_attempt(&attempt).unwrap(), expected);
        assert!(!expected.complete(&attempt).unwrap());
        attempt = terminal_attempt(refresh, true);
        assert!(!expected.complete(&attempt).unwrap());
        add_stopped(&mut attempt, refresh, false);
        assert!(!expected.complete(&attempt).unwrap());
        add_stopped(&mut attempt, refresh, true);
        assert!(expected.complete(&attempt).unwrap());
        let wrong = if refresh {
            TerminalProfile::Frozen
        } else {
            TerminalProfile::Refresh
        };
        assert!(wrong.complete(&attempt).is_err());
    }
}
#[test]
fn orphan_mixed_and_changed_receipt_contexts_never_select_or_complete() {
    let mut old = terminal_attempt(false, true);
    let new = terminal_attempt(true, true);
    old.refresh_prepared = new.refresh_prepared.clone();
    assert!(TerminalProfile::for_execution(&old).is_err());
    let mut missing = terminal_attempt(true, true);
    missing.refresh_execution = None;
    assert!(TerminalProfile::for_execution(&missing).is_err());
    let mut mixed = terminal_attempt(false, true);
    mixed.native_host.as_mut().unwrap().refresh_terminal =
        new.native_host.as_ref().unwrap().refresh_terminal.clone();
    assert!(TerminalProfile::for_attempt(&mixed).is_err());
    let mut changed = terminal_attempt(true, true);
    add_stopped(&mut changed, true, true);
    changed
        .native_host
        .as_mut()
        .unwrap()
        .refresh_terminal
        .as_mut()
        .unwrap()
        .stopped
        .as_mut()
        .unwrap()
        .receipt
        .as_mut()
        .unwrap()
        .request_digest = aq_session_protocol::Digest::of(b"changed receipt");
    assert!(TerminalProfile::Refresh.complete(&changed).is_err());
}
