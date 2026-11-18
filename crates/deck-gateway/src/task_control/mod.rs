//! The `control` kind (ADR 0011 §4, contract 1.4.0 §13): a coordinator's
//! closed surface for creating, prompting, waiting on and stopping tasks.
//! `POST /control` carries one bounded JSON operation to `deckd` and returns
//! its one reply.
//!
//! The design choice: the gateway carries and `deckd` decides. This module
//! checks the bearer token (before the path, the method or a body byte),
//! the route and the closed operation set ([`wire`]), and forwards the typed
//! operation with the coordinator's task id, which is host state from the
//! gateway's arguments, never the guest's. A VM coordinator reaches it on the
//! extension port with no token; a local coordinator reaches it on host
//! loopback, where other host processes could too, so there the per-task
//! token is required and the listener refuses to start without one. The
//! token is held as a digest and never written anywhere.
//!
//! A refusal sent before the body is read still reads the body (up to the
//! limit) before the stream closes: closing a TCP socket with unread bytes
//! sends a reset, and on Linux the reset can reach the client before it has
//! read the refusal.

pub mod daemon;
pub mod wire;

use crate::http::{self, ReadError, Shape};
use anyhow::{Context as _, Result, bail};
use daemon::{Daemon, Outcome};
use std::io::{self, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;
use std::time::Duration;
use wire::{CHALLENGE, Code, Refusal, Token};

/// The kind's name on the extension port.
pub const KIND: &str = "control";
/// The environment variable a local coordinator's token reaches the gateway
/// in; the same name the agent reads (contract 1.4.0 §13.2).
pub const TOKEN_ENV: &str = "DECK_CONTROL_TOKEN";
/// A request head must arrive within this; a `wait` reply may take 300 s more.
const HEAD_TIMEOUT: Duration = Duration::from_secs(30);
const WRITE_TIMEOUT: Duration = Duration::from_secs(wire::WAIT_LIMIT_MS / 1000 + 60);

/// One coordinator's surface.
pub struct Surface {
    task: String,
    token: Option<Token>,
    daemon: Arc<dyn Daemon>,
}

impl Surface {
    /// The surface for coordinator `task`. `token` is required on host
    /// loopback and absent in a VM.
    pub fn new(task: String, token: Option<Token>, daemon: Arc<dyn Daemon>) -> Self {
        Self { task, token, daemon }
    }

    /// Serve one request on any stream: a guest connection from the extension
    /// port, or a loopback TCP connection.
    pub fn handle<S: Read + Write>(&self, stream: S) -> io::Result<()> {
        let mut reader = BufReader::new(stream);
        let mut unread = 0;
        let refusal = match self.answer(&mut reader, &mut unread) {
            Ok(None) => return Ok(()),
            Ok(Some(Ok(body))) => {
                let body = body.to_string();
                return http::write(reader.get_mut(), 200, &json_type(), body.as_bytes());
            }
            Ok(Some(Err(refusal))) => refusal,
            Err(ReadError::Io(error)) => return Err(error),
            // A head this gateway cannot parse is a bad request whatever the
            // carrier said; the table allows no other status for it.
            Err(ReadError::Refused { message, .. }) => Refusal::new(Code::BadRequest, message),
        };
        let mut headers = json_type();
        if refusal.code == Code::Unauthorized {
            headers.push(("www-authenticate", CHALLENGE.into()));
        }
        http::write(reader.get_mut(), refusal.status, &headers, refusal.body().to_string().as_bytes())?;
        drain(&mut reader, unread);
        Ok(())
    }

    /// `Ok(None)` is a peer that closed before a request. `unread` is left
    /// at the body bytes the head declared and this did not read.
    fn answer<S: Read>(
        &self,
        reader: &mut BufReader<S>,
        unread: &mut usize,
    ) -> Result<Option<Result<serde_json::Value, Refusal>>, ReadError> {
        let Some((request, length)) = http::read_head_only(reader, Shape::Origin)? else { return Ok(None) };
        *unread = length.unwrap_or(0);
        if !wire::authorized(&request.headers, self.token.as_ref()) {
            return Ok(Some(Err(Refusal::new(Code::Unauthorized, "missing or wrong bearer token"))));
        }
        let length = match wire::route(&request.method, &request.target, length) {
            Ok(length) => length,
            Err(refusal) => return Ok(Some(Err(refusal))),
        };
        let body = http::read_body(reader, length)?;
        *unread = 0;
        let operation = match wire::parse(&body) {
            Ok(operation) => operation,
            Err(refusal) => return Ok(Some(Err(refusal))),
        };
        Ok(Some(match self.daemon.call(&self.task, &operation) {
            Outcome::Reply(body) => Ok(body),
            Outcome::Refused(refusal) => Err(refusal),
        }))
    }
}

/// Read and drop up to `unread` body bytes, at most [`wire::MAX_BODY`], so
/// the close that follows is a clean one. A peer that never sends them is
/// bounded by the stream's read timeout; an error only means it is gone.
fn drain<S: Read>(reader: &mut BufReader<S>, unread: usize) {
    let limit = u64::try_from(unread.min(wire::MAX_BODY)).unwrap_or(0);
    let _ = io::copy(&mut reader.take(limit), &mut io::sink());
}

fn json_type() -> Vec<(&'static str, String)> {
    vec![("content-type", "application/json".into())]
}

/// What a local coordinator's gateway announces once its listener is bound.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct Ready {
    /// The bound loopback address, `127.0.0.1:<port>`.
    pub control: SocketAddr,
}

/// Serve `surface` on host loopback until the listener fails. The address
/// must be a loopback one and the surface must hold a token: other host
/// processes can reach this port, and the token is what keeps them off it.
pub fn serve_loopback(surface: Surface, listen: SocketAddr) -> Result<()> {
    if !listen.ip().is_loopback() {
        bail!("the control surface listens on loopback only, not {listen}");
    }
    if surface.token.is_none() {
        bail!("a loopback control surface needs {TOKEN_ENV}");
    }
    let listener = TcpListener::bind(listen).with_context(|| format!("binding the control surface on {listen}"))?;
    let ready = Ready { control: listener.local_addr()? };
    println!("{}", serde_json::to_string(&ready)?);
    io::stdout().flush().context("announcing the control surface")?;
    let surface = Arc::new(surface);
    let limits = Arc::new(crate::server::Limit::new(crate::server::CONNECTIONS));
    loop {
        let slot = limits.acquire();
        let (stream, _) = listener.accept().context("accepting a control connection")?;
        let surface = surface.clone();
        std::thread::Builder::new().name("gateway-control".into()).spawn(move || {
            let _slot = slot;
            let served = stream
                .set_read_timeout(Some(HEAD_TIMEOUT))
                .and_then(|()| stream.set_write_timeout(Some(WRITE_TIMEOUT)))
                .and_then(|()| surface.handle(stream));
            if let Err(error) = served
                && !matches!(error.kind(), io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset)
            {
                eprintln!("deck-gateway: control connection ended: {error}");
            }
        })?;
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
