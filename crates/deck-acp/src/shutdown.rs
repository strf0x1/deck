//! A close-only attachment for the runtime's stop path. Negotiate support
//! without replaying history, and bound the entire exchange even if the peer
//! sends unrelated notifications or an unterminated line continuously.

use crate::{VERSION, client::request_prefix, rpc};
use anyhow::{Context as _, Result, bail};
use serde_json::{Value, json};
use std::io::{BufRead as _, BufReader, Read, Write as _};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

/// Returns false when close is unadvertised. The caller retains exclusive
/// transport ownership until process termination, including on failure.
pub fn close_session(socket: &UnixStream, session: &str, budget: Duration) -> Result<bool> {
    let deadline = Instant::now() + budget;
    socket.set_nonblocking(true)?;
    let mut input = BufReader::new(Bounded { socket, deadline });
    let prefix = request_prefix();
    let initialized = exchange(
        socket,
        &mut input,
        &format!("{prefix}-init"),
        "initialize",
        json!({
            "protocolVersion": VERSION,
            "clientCapabilities": {},
            "clientInfo": { "name": "deck", "version": env!("CARGO_PKG_VERSION") },
        }),
    )?;
    if !initialized["agentCapabilities"]["sessionCapabilities"]["close"].is_object() {
        return Ok(false);
    }
    exchange(socket, &mut input, &format!("{prefix}-close"), "session/close", json!({ "sessionId": session }))?;
    Ok(true)
}

fn exchange(
    socket: &UnixStream,
    input: &mut BufReader<Bounded<'_>>,
    id: &str,
    method: &str,
    params: Value,
) -> Result<Value> {
    let mut output = socket;
    remaining(input.get_ref().deadline)?;
    output.write_all(rpc::request(id, method, params).as_bytes())?;
    loop {
        remaining(input.get_ref().deadline)?;
        let mut line = Vec::new();
        let n = input.take(1024 * 1024 + 1).read_until(b'\n', &mut line)?;
        if n == 0 || n > 1024 * 1024 || !line.ends_with(b"\n") {
            bail!("invalid or closed shutdown stream");
        }
        let line = std::str::from_utf8(&line)?;
        if let Some(rpc::Incoming::Response { id: reply, result }) = rpc::parse(line)
            && reply == id
        {
            return result.map_err(anyhow::Error::msg).with_context(|| method.to_string());
        }
    }
}

struct Bounded<'a> {
    socket: &'a UnixStream,
    deadline: Instant,
}

impl Read for Bounded<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        loop {
            let left = remaining(self.deadline)?;
            match self.socket.read(bytes) {
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(left.min(Duration::from_millis(1)));
                }
                result => return result,
            }
        }
    }
}

fn remaining(deadline: Instant) -> std::io::Result<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "session close timed out"));
    }
    Ok(left)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_negotiates_support_and_waits_for_its_own_reply() {
        let (socket, peer) = UnixStream::pair().unwrap();
        let server = std::thread::spawn(move || {
            let mut peer = BufReader::new(peer);
            let mut line = String::new();
            peer.read_line(&mut line).unwrap();
            let value: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(value["method"], "initialize");
            peer.get_mut()
                .write_all(
                    rpc::ok(&value["id"], json!({ "agentCapabilities": { "sessionCapabilities": { "close": {} } } }))
                        .as_bytes(),
                )
                .unwrap();
            line.clear();
            peer.read_line(&mut line).unwrap();
            let value: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(value["method"], "session/close");
            assert_eq!(value["params"]["sessionId"], "saved");
            peer.get_mut().write_all(rpc::notification("_construct/turn", json!({})).as_bytes()).unwrap();
            peer.get_mut().write_all(rpc::ok(&value["id"], json!({})).as_bytes()).unwrap();
        });
        assert!(close_session(&socket, "saved", Duration::from_secs(2)).unwrap());
        server.join().unwrap();
    }

    #[test]
    fn an_unadvertised_close_is_never_sent() {
        let (socket, peer) = UnixStream::pair().unwrap();
        let server = std::thread::spawn(move || {
            let mut input = BufReader::new(peer);
            let mut line = String::new();
            input.read_line(&mut line).unwrap();
            let value: Value = serde_json::from_str(&line).unwrap();
            input.get_mut().write_all(rpc::ok(&value["id"], json!({ "agentCapabilities": {} })).as_bytes()).unwrap();
            line.clear();
            assert_eq!(input.read_line(&mut line).unwrap(), 0);
        });
        assert!(!close_session(&socket, "saved", Duration::from_secs(2)).unwrap());
        drop(socket);
        server.join().unwrap();
    }

    #[test]
    fn continuous_notifications_or_partial_lines_cannot_extend_the_deadline() {
        for payload in [rpc::notification("noise", json!({})), "x".into()] {
            let (socket, mut peer) = UnixStream::pair().unwrap();
            let server = std::thread::spawn(move || {
                while peer.write_all(payload.as_bytes()).is_ok() {
                    std::thread::sleep(Duration::from_millis(1));
                }
            });
            let start = Instant::now();
            assert!(close_session(&socket, "saved", Duration::from_millis(50)).is_err());
            assert!(start.elapsed() < Duration::from_secs(1));
            drop(socket);
            server.join().unwrap();
        }
    }
}
