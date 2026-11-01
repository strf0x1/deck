//! The control socket: a Unix listener whose connections each get a thread
//! that reads command lines, hands them to the UI, and writes the replies.
//!
//! The design choice: threads and blocking I/O, like the ACP reader. A
//! connection thread parses its line, sends a [`Request`] down a channel the
//! UI polls, and blocks until the UI answers, so commands from one client run
//! strictly in order and the UI thread never touches the socket.

use super::protocol::{self, Command};
use anyhow::{Context as _, Result};
use futures::channel::mpsc;
use serde_json::Value;
use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::fs::FileTypeExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::mpsc as sync;

/// A parsed command waiting for the UI, and where its reply goes.
pub struct Request {
    pub cmd: Command,
    pub reply: sync::Sender<Value>,
}

/// Listen on `path`, replacing a stale socket left by an earlier run (but
/// never another kind of file). Requests arrive on the returned receiver.
pub fn listen(path: &Path) -> Result<mpsc::UnboundedReceiver<Request>> {
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        anyhow::ensure!(meta.file_type().is_socket(), "{} exists and is not a socket", path.display());
        std::fs::remove_file(path).with_context(|| format!("removing stale {}", path.display()))?;
    }
    let listener = UnixListener::bind(path).with_context(|| format!("binding {}", path.display()))?;
    let (tx, rx) = mpsc::unbounded();
    std::thread::Builder::new().name("deck-control".into()).spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let tx = tx.clone();
            let _ = std::thread::Builder::new().name("deck-control-conn".into()).spawn(move || serve(stream, &tx));
        }
    })?;
    Ok(rx)
}

fn serve(stream: UnixStream, tx: &mpsc::UnboundedSender<Request>) {
    let Ok(mut out) = stream.try_clone() else { return };
    for line in BufReader::new(stream).lines() {
        let Ok(line) = line else { return };
        if line.trim().is_empty() {
            continue;
        }
        let reply = match protocol::parse(&line) {
            Err(e) => protocol::error(e),
            Ok(cmd) => {
                let (reply, answer) = sync::channel();
                if tx.unbounded_send(Request { cmd, reply }).is_err() {
                    return;
                }
                answer.recv().unwrap_or_else(|_| protocol::error("the window closed"))
            }
        };
        if writeln!(out, "{reply}").is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt as _;
    use serde_json::json;

    #[test]
    fn a_line_in_becomes_a_request_and_its_reply_a_line_out() {
        let dir = std::env::temp_dir().join(format!("deck-control-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sock");
        let _ = listen(&path).unwrap();
        // A second listen replaces the first one's socket file.
        let mut rx = listen(&path).unwrap();
        std::thread::spawn(move || {
            while let Some(req) = futures::executor::block_on(rx.next()) {
                let _ = req.reply.send(protocol::ok(json!({ "cmd": format!("{:?}", req.cmd) })));
            }
        });

        let mut conn = UnixStream::connect(&path).unwrap();
        writeln!(conn, "{{\"cmd\":\"state\"}}\n\n{{\"cmd\":\"fly\"}}").unwrap();
        let mut lines = BufReader::new(conn).lines();
        let first: Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
        assert_eq!(first, json!({ "ok": true, "cmd": "State" }));
        let second: Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
        assert_eq!(second["ok"], json!(false));

        let file = dir.join("file");
        std::fs::write(&file, "keep").unwrap();
        assert!(listen(&file).is_err(), "a regular file is never replaced");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "keep");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
