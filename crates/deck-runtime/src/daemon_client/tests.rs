//! A slow credential store must not serialize unrelated grant controls on
//! the window's ordinary daemon connection. The peer holds a real reply until
//! the test releases it, without relying on an OS keyring dialog.

use super::*;
use serde_json::json;
use std::io::Write as _;
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
    let client = DaemonClient::at(paths).unwrap();
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
