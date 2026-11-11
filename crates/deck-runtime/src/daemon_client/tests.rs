//! A slow credential store must not serialize unrelated grant controls on
//! the window's ordinary daemon connection. The peer holds a real reply until
//! the test releases it, without relying on an OS keyring dialog.

use super::*;
use serde_json::json;
use std::io::{BufRead as _, Write as _};
use std::os::unix::net::UnixListener;
use std::sync::mpsc as channel;

#[test]
fn credential_reads_leave_grant_controls_available() {
    let directory = PathBuf::from(format!("/tmp/deck-client-isolated-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let paths = Paths { directory: directory.clone() };
    let listener = UnixListener::bind(paths.control()).unwrap();
    listener.set_nonblocking(true).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let stopping = stop.clone();
    let (started, reading) = channel::channel();
    let (release, released) = channel::channel();
    let released = Arc::new(Mutex::new(released));
    let server = std::thread::spawn(move || {
        let mut workers = Vec::new();
        while !stopping.load(Ordering::Acquire) {
            let (stream, _) = match listener.accept() {
                Ok(connection) => connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(1));
                    continue;
                }
                Err(error) => panic!("accepting client: {error}"),
            };
            let started = started.clone();
            let released = released.clone();
            workers.push(std::thread::spawn(move || {
                stream.set_nonblocking(false).unwrap();
                let mut socket = BufReader::new(stream);
                while let Some(line) = wire::read(&mut socket).unwrap() {
                    let request: Request = serde_json::from_slice(&line).unwrap();
                    let reply = match request {
                        Request::Hello => json!({"ok": hello()}),
                        Request::Tasks | Request::AccessGrants { .. } => json!({"ok": []}),
                        Request::RevokeAccessGrant { .. } => json!({"ok": null}),
                        Request::RetryCredential { name } => {
                            assert_eq!(name, "KEY");
                            json!({"ok": true})
                        }
                        Request::Credentials { .. } => {
                            started.send(()).unwrap();
                            released.lock().unwrap().recv().unwrap();
                            json!({"error": "store unavailable"})
                        }
                        other => panic!("unexpected request: {other:?}"),
                    };
                    writeln!(socket.get_mut(), "{reply}").unwrap();
                }
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }
    });
    let client = DaemonClient::at(paths, None).unwrap();
    let reader = client.clone();
    let credential = std::thread::spawn(move || reader.credentials("t"));
    reading.recv_timeout(Duration::from_secs(5)).unwrap();
    let grants = client.clone();
    let (finished, result) = channel::channel();
    let controls = std::thread::spawn(move || {
        let listing = grants.access_grants("t").unwrap();
        grants.revoke_access_grant("t", "g").unwrap();
        assert!(grants.retry_credential("KEY").unwrap());
        finished.send(listing).unwrap();
    });
    let available = result.recv_timeout(Duration::from_secs(2));
    release.send(()).unwrap();
    assert!(credential.join().unwrap().is_err());
    controls.join().unwrap();
    drop(client);
    stop.store(true, Ordering::Release);
    server.join().unwrap();
    std::fs::remove_dir_all(directory).unwrap();
    assert_eq!(available.unwrap(), Vec::<Grant>::new(), "grant controls complete while the store is still blocked");
}

/// The handshake a current daemon answers.
fn hello() -> serde_json::Value {
    json!({
        "protocol": wire::PROTOCOL,
        "version": "test",
        "pid": 1,
        "executable": "/tmp/deckd",
        "started": 0,
        "stale": false,
    })
}

#[test]
fn another_protocol_or_a_rebuilt_binary_is_incompatible() {
    let reply: Hello = serde_json::from_value(hello()).unwrap();
    assert!(check(&reply, true).is_ok());
    let other = Hello { protocol: reply.protocol + 1, ..reply.clone() };
    assert!(check(&other, false).is_err(), "a protocol mismatch is refused even when only stopping");
    let stale = Hello { stale: true, ..reply };
    assert!(check(&stale, true).is_err(), "a window refuses a daemon whose binary was replaced");
    assert!(check(&stale, false).is_ok(), "the daemon commands must still reach a stale daemon");
}

#[test]
fn a_daemon_without_the_handshake_is_explained_not_parsed() {
    let directory = PathBuf::from(format!("/tmp/deck-client-old-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let paths = Paths { directory: directory.clone() };
    let listener = UnixListener::bind(paths.control()).unwrap();
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        let mut line = String::new();
        BufReader::new(socket.try_clone().unwrap()).read_line(&mut line).unwrap();
        assert!(line.contains("\"op\":\"hello\""), "{line}");
        writeln!(socket, r#"{{"error":"unknown variant `hello`, expected one of `create`"}}"#).unwrap();
    });
    let error = DaemonClient::at(paths, None).err().unwrap().to_string();
    server.join().unwrap();
    std::fs::remove_dir_all(directory).unwrap();
    assert!(error.contains("predates") && error.contains("pkill -x deckd"), "{error}");
    assert!(!error.contains("unknown variant"), "a window never shows the raw grammar error: {error}");
}

#[test]
fn a_read_reconnects_but_a_mutation_is_not_repeated() {
    let directory = PathBuf::from(format!("/tmp/deck-client-retry-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let paths = Paths { directory: directory.clone() };
    let listener = UnixListener::bind(paths.control()).unwrap();
    let server = std::thread::spawn(move || {
        // The first connection answers the handshake, then dies before it
        // answers a read.
        let (stream, _) = listener.accept().unwrap();
        let mut input = BufReader::new(stream);
        let mut line = String::new();
        input.read_line(&mut line).unwrap();
        assert!(line.contains("\"op\":\"hello\""), "{line}");
        writeln!(input.get_mut(), "{}", json!({"ok": hello()})).unwrap();
        line.clear();
        input.read_line(&mut line).unwrap();
        assert!(line.contains("\"op\":\"tasks\""), "{line}");
        drop(input);

        // The read is retried on a new connection and answered there; the
        // mutation that follows shares it and dies without a reply.
        let (stream, _) = listener.accept().unwrap();
        let mut input = BufReader::new(stream);
        line.clear();
        input.read_line(&mut line).unwrap();
        assert!(line.contains("\"op\":\"tasks\""), "a read retries on a fresh connection: {line}");
        writeln!(input.get_mut(), "{}", json!({"ok": []})).unwrap();
        line.clear();
        input.read_line(&mut line).unwrap();
        assert!(line.contains("\"op\":\"title\""), "a mutation is sent unretried: {line}");
        drop(input);
    });
    let client = DaemonClient::at(paths, None).unwrap();
    assert!(client.tasks().unwrap().is_empty());
    assert!(client.title("t", "x").is_err(), "a mutation the daemon may have run is not retried");
    drop(client);
    server.join().unwrap();
    std::fs::remove_dir_all(directory).unwrap();
}
