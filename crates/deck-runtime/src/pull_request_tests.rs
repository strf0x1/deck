//! The reader against a stub gateway socket: what it sends, when it reads
//! again, what it keeps when a read fails, and what a stopped task gets.

use super::*;
use std::io::{BufRead as _, BufReader};
use std::os::unix::net::UnixListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT: AtomicUsize = AtomicUsize::new(0);

/// Each request's path and JSON body, in arrival order.
type Log = Arc<Mutex<Vec<(String, Value)>>>;

/// A short scratch directory: the socket path must stay under `SUN_LEN`.
fn scratch() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("dpr-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A gateway that records each request's path and body and answers with
/// `reply(path)` as `(status, body)`.
fn gateway(dir: &Path, reply: impl Fn(&str) -> (u16, Value) + Send + 'static) -> (PathBuf, Log) {
    let socket = dir.join("g.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let path = line.split(' ').nth(1).unwrap().to_string();
            let mut length = 0;
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).unwrap();
                if header.trim().is_empty() {
                    break;
                }
                if let Some(value) = header.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse().unwrap();
                }
            }
            let mut body = vec![0; length];
            std::io::Read::read_exact(&mut reader, &mut body).unwrap();
            log.lock().unwrap().push((path.clone(), serde_json::from_slice(&body).unwrap()));
            let (status, value) = reply(&path);
            let text = value.to_string();
            let mut stream = stream;
            let _ = write!(stream, "HTTP/1.1 {status} X\r\ncontent-length: {}\r\n\r\n{text}", text.len());
        }
    });
    (socket, seen)
}

fn pull() -> Publication {
    Publication::PullRequest { number: 7, url: "https://github.com/o/r/pull/7".into(), title: "t".into(), draft: true }
}

fn review() -> Value {
    json!({ "number": 7, "state": "open", "draft": true, "merged": false, "checks": { "total": 1, "runs": [
        { "name": "ci", "status": "in_progress", "conclusion": null }
    ] } })
}

#[test]
fn a_pull_request_is_read_with_review_and_not_again_within_the_interval() {
    let dir = scratch();
    let (socket, seen) = gateway(&dir, |_| (200, review()));
    let reader = Reader::default();
    let first = reader.state("t-1", &dir, Some(pull()), Some(socket.clone()), false);
    assert!(first.fresh);
    assert_eq!(first.live.as_ref().unwrap().pull_request, Some(review()));
    assert_eq!(
        *seen.lock().unwrap(),
        [("/operations/pull_request/review".to_string(), json!({ "operation": "review", "number": 7 }))]
    );
    // A window asking again, even with refresh, inside MIN_REFRESH gets the
    // same read back and costs GitHub nothing.
    for refresh in [false, true] {
        let again = reader.state("t-1", &dir, Some(pull()), Some(socket.clone()), refresh);
        assert!(!again.fresh);
        assert_eq!(again.live, first.live);
    }
    assert_eq!(seen.lock().unwrap().len(), 1);
}

#[test]
fn a_pushed_branch_is_read_with_refs() {
    let dir = scratch();
    let tip = "b".repeat(40);
    let reply = json!({ "branches": [{ "branch": "deck/t-1", "commit": tip, "task_branch": true }] });
    let (socket, seen) = gateway(&dir, move |_| (200, reply.clone()));
    let branch = Publication::Branch { branch: "deck/t-1".into(), commit: "a".repeat(40) };
    let state = Reader::default().state("t-1", &dir, Some(branch), Some(socket), false);
    assert_eq!(state.live.unwrap().branch_commit.as_deref(), Some(tip.as_str()));
    assert_eq!(seen.lock().unwrap()[0].0, "/operations/refs");
}

#[test]
fn a_failed_read_keeps_the_last_success_and_backs_off_even_on_refresh() {
    let dir = scratch();
    let fail = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let failing = fail.clone();
    let (socket, seen) = gateway(&dir, move |_| {
        if failing.load(Ordering::SeqCst) {
            (502, json!({ "error": { "message": "rate limited", "code": "deck_github_failed" }, "retryable": true }))
        } else {
            (200, review())
        }
    });
    let reader = Reader::default();
    reader.state("t-1", &dir, Some(pull()), Some(socket.clone()), false);
    fail.store(true, Ordering::SeqCst);
    // Make the next read due, as if the interval had passed.
    reader.entries.lock().unwrap().get_mut("t-1").unwrap().next = Some(Instant::now());
    let failed = reader.state("t-1", &dir, Some(pull()), Some(socket.clone()), false);
    let live = failed.live.unwrap();
    assert!(live.error.as_deref().unwrap().contains("deck_github_failed (502): rate limited"), "{live:?}");
    assert_eq!(live.pull_request, Some(review()), "the last good state stays");
    let entry_backoff = reader.entries.lock().unwrap()["t-1"].backoff;
    assert_eq!(entry_backoff, INTERVAL);
    reader.entries.lock().unwrap().get_mut("t-1").unwrap().read = Some(Instant::now() - MIN_REFRESH * 2);
    assert!(!reader.state("t-1", &dir, Some(pull()), Some(socket), true).fresh, "a refresh does not skip a backoff");
    assert_eq!(seen.lock().unwrap().len(), 2);
}

#[test]
fn a_stopped_task_gets_the_record_and_the_persisted_last_read() {
    let dir = scratch();
    let (socket, seen) = gateway(&dir, |_| (200, review()));
    let read = Reader::default().state("t-1", &dir, Some(pull()), Some(socket), false).live.unwrap();
    // Another daemon, the task stopped: no socket, no request, the last read
    // with its time.
    let stopped = Reader::default().state("t-1", &dir, Some(pull()), None, true);
    assert_eq!(stopped, State { publication: Some(pull()), live: Some(read.clone()), fresh: false });
    assert!(read.read_at_ms > 0);
    assert_eq!(seen.lock().unwrap().len(), 1);
}

#[test]
fn nothing_published_reads_nothing() {
    let dir = scratch();
    let (socket, seen) = gateway(&dir, |_| (200, json!({})));
    let state = Reader::default().state("t-1", &dir, None, Some(socket), true);
    assert_eq!(state, State { publication: None, live: None, fresh: false });
    assert!(seen.lock().unwrap().is_empty());
}

#[test]
fn only_reads_are_reachable() {
    // Exhaustive: a new variant fails to compile here until it is listed, and
    // every listed one is a read the contract marks `writes: false`.
    for read in [Read::Refs, Read::Review(1)] {
        let (path, operation) = match read {
            Read::Refs => (read.path(), "refs"),
            Read::Review(_) => (read.path(), "review"),
        };
        assert_eq!(read.body()["operation"], operation);
        assert!(["/operations/refs", "/operations/pull_request/review"].contains(&path));
    }
}
