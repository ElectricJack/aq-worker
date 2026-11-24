use super::*;
use crate::state_io::durable_write_with;
#[test]
fn new_request_is_invisible_to_polling_reader_during_slow_write() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("start.request");
    let bytes = br#"{"permit":{"sequence":1},"guardian":"fixture"}"#;
    let (partial_tx, partial_rx) = std::sync::mpsc::channel();
    let (polled_tx, polled_rx) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        let path = &path;
        let reader = scope.spawn(move || {
            partial_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            // The writer is held after one byte until the reader has polled
            // the exact path the guardian watches. This deterministically
            // exposes the old create_new-before-write publication race.
            for _ in 0..100 {
                assert!(!path.exists(), "partial request was published");
                std::thread::yield_now();
            }
            polled_tx.send(()).unwrap();
            let end = Instant::now() + Duration::from_secs(5);
            loop {
                match read_bytes(path, MAX_LOCAL_FILE) {
                    Ok(observed) => {
                        let _: serde_json::Value = serde_json::from_slice(&observed).unwrap();
                        assert_eq!(observed, bytes);
                        break;
                    }
                    Err(error) => assert_eq!(
                        error.downcast_ref::<std::io::Error>().unwrap().kind(),
                        std::io::ErrorKind::NotFound
                    ),
                }
                assert!(Instant::now() < end, "request was never published");
                std::thread::yield_now();
            }
        });
        durable_write_with(path, false, |file| {
            file.write_all(&bytes[..1])?;
            partial_tx.send(()).unwrap();
            polled_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            file.write_all(&bytes[1..])
        })
        .unwrap();
        reader.join().unwrap();
    });
    assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}
#[test]
fn new_request_never_clobbers_existing_entries_and_cleans_failed_writes() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("start.request");
    assert!(
        durable_write_with(&path, false, |file| {
            file.write_all(b"{")?;
            Err(std::io::Error::other("injected write failure"))
        })
        .is_err()
    );
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    durable_new(&path, b"original").unwrap();
    assert!(durable_new(&path, b"replacement").is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"original");
    let link = dir.path().join("link");
    symlink(&path, &link).unwrap();
    assert!(durable_new(&link, b"replacement").is_err());
    assert!(std::fs::symlink_metadata(&link).unwrap().is_symlink());
    let dangling = dir.path().join("dangling");
    symlink(dir.path().join("absent"), &dangling).unwrap();
    assert!(durable_new(&dangling, b"replacement").is_err());
    assert!(std::fs::symlink_metadata(&dangling).unwrap().is_symlink());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 3);
}
#[test]
fn concurrent_new_requests_have_exactly_one_complete_winner() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("start.request");
    let barrier = std::sync::Barrier::new(2);
    std::thread::scope(|scope| {
        let publishers: Vec<_> = [b"first".as_slice(), b"second".as_slice()]
            .into_iter()
            .map(|bytes| {
                let path = &path;
                let barrier = &barrier;
                scope.spawn(move || {
                    durable_write_with(path, false, |file| {
                        file.write_all(bytes)?;
                        barrier.wait();
                        Ok(())
                    })
                })
            })
            .collect();
        let successes = publishers
            .into_iter()
            .map(|publisher| publisher.join().unwrap())
            .filter(Result::is_ok)
            .count();
        assert_eq!(successes, 1);
    });
    let bytes = std::fs::read(&path).unwrap();
    assert!(bytes == b"first" || bytes == b"second");
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}
#[test]
#[ignore = "subprocess output fixture invoked by bounded output test"]
fn large_stdout_fixture() {
    std::io::stdout().write_all(&vec![b'x'; 60000]).unwrap();
}
#[test]
fn command_stdout_above_pipe_capacity_is_drained_before_exit() {
    let output = bounded_command(Command::new(std::env::current_exe().unwrap()).args([
        "--ignored",
        "--exact",
        "io_tests::large_stdout_fixture",
        "--nocapture",
    ]))
    .unwrap();
    assert!(output.windows(60000).any(|w| w.iter().all(|b| *b == b'x')));
}
#[test]
fn command_input_and_output_above_pipe_capacity_progress_under_one_deadline() {
    let bytes: Vec<_> = (0..60000).map(|n| (n % 251) as u8).collect();
    let output = bounded_command_input(
        &mut Command::new("/bin/cat"),
        Some(&bytes),
        Duration::from_millis(250),
    )
    .unwrap();
    assert_eq!(output, bytes);
    let started = Instant::now();
    assert!(
        bounded_command_input(
            Command::new("/bin/sleep").arg("2"),
            Some(&bytes),
            Duration::from_millis(50)
        )
        .is_err()
    );
    assert!(started.elapsed() < Duration::from_secs(1));
}
#[test]
fn nonregular_and_symlink_material_is_refused() {
    use std::os::unix::{ffi::OsStrExt, fs::symlink};
    let temp = tempfile::TempDir::new().unwrap();
    let path = temp.path().join("file");
    std::fs::write(&path, b"material").unwrap();
    let link = temp.path().join("link");
    symlink(&path, &link).unwrap();
    assert!(read_bytes(&link, 100).is_err());
    let fifo = temp.path().join("fifo");
    let c = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    assert!(read_bytes(&fifo, 100).is_err());
}
