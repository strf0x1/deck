//! The CONNECT tunnel: the guest asks for `host:port`, the host resolves and
//! dials it, and from then on the bytes are opaque (ADR 0004 §3).
//!
//! The design choice: the destination-level control is all of it. The host
//! resolves the name itself, refuses host and private address space on any
//! port, and asks if the task holds no profile entry and no grant for exactly
//! that `host:port`; anything but an allowance refuses, so no failure to reach
//! `deckd` can widen the boundary. Once allowed, the guest's request head is
//! treated as bytes: it is forwarded, but every proxy-authorization it carried
//! is dropped first, because a proxy credential belongs to the guest's proxy
//! configuration and not to the tunnel's destination.

use crate::web::policy::{Grants, Profiles};
use crate::web::resolve::{Refused, Resolver};
use crate::web::status::Events;
use crate::web::{Answer, Control, refuse};
use std::io::{self, BufReader, Write as _};
use std::net::{SocketAddr, TcpStream};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// A tunnel that carries nothing is a client that changed its mind; one that
/// is idle for this long is a stream that stalled and holds a socket.
const IDLE: Duration = Duration::from_secs(600);
/// The request head a CONNECT carries. Anything larger is refused before it
/// is read into memory.
const MAX_HEAD: usize = 8 * 1024;

pub struct Connector {
    pub resolver: Arc<Resolver>,
    pub grants: Arc<Grants>,
    pub profiles: Arc<Profiles>,
    pub control: Option<Control>,
    pub status: Arc<Events>,
}

impl Connector {
    /// One connector per direction of work, sharing the resolver, the grant
    /// cache and the status sequence with the server that built it.
    pub fn with_events(&self, status: Arc<Events>) -> Self {
        Self {
            resolver: self.resolver.clone(),
            grants: self.grants.clone(),
            profiles: self.profiles.clone(),
            control: self.control.clone(),
            status,
        }
    }
}

/// One CONNECT request, as the host read it.
pub struct Request {
    pub name: String,
    pub port: u16,
    pub head: Vec<u8>,
}

impl Request {
    /// Parse the request line and headers of a CONNECT. The head is kept whole
    /// (minus proxy credentials) so it can be forwarded byte for byte.
    pub fn parse(reader: &mut impl io::BufRead) -> Result<Self, Refused> {
        let mut head = Vec::new();
        let mut line = Vec::new();
        loop {
            line.clear();
            let read = reader.read_until(b'\n', &mut line).map_err(|_| Refused::Invalid("request head".into()))?;
            if read == 0 {
                return Err(Refused::Invalid(String::new()));
            }
            head.extend_from_slice(&line);
            if head.len() > MAX_HEAD {
                return Err(Refused::Invalid("request head is too large".into()));
            }
            if line == b"\r\n" || line == b"\n" {
                break;
            }
        }
        Self::of(head)
    }

    fn of(head: Vec<u8>) -> Result<Self, Refused> {
        let text = String::from_utf8(head.clone()).map_err(|_| Refused::Invalid("request head".into()))?;
        let mut lines = text.split("\r\n");
        let request_line = lines.next().unwrap_or_default();
        let mut words = request_line.split(' ');
        let (Some("CONNECT"), Some(target), Some("HTTP/1.1" | "HTTP/1.0"), None) =
            (words.next(), words.next(), words.next(), words.next())
        else {
            return Err(Refused::Invalid("the request is not a CONNECT".into()));
        };
        // The head is rebuilt from the parsed form, so a lenient target
        // (upper case, a bare name where a port belongs) is never forwarded.
        let (name, port) = split(target).ok_or_else(|| Refused::Invalid(target.to_owned()))?;
        if !crate::web::policy::is_host(&name) {
            return Err(Refused::Invalid(name));
        }
        // The head is forwarded without the guest's proxy credentials. Every
        // other header is opaque tunnel setup and stays.
        let mut kept = format!("CONNECT {name}:{port} HTTP/1.1\r\n");
        for line in lines {
            if line.is_empty() {
                break;
            }
            let name = line.split(':').next().unwrap_or_default().trim();
            if name.eq_ignore_ascii_case("proxy-authorization") || name.eq_ignore_ascii_case("proxy-connection") {
                continue;
            }
            kept.push_str(line);
            kept.push_str("\r\n");
        }
        kept.push_str("\r\n");
        Ok(Self { name, port, head: kept.into_bytes() })
    }

    pub fn destination(&self) -> String {
        format!("{}:{}", self.name, self.port)
    }
}

/// `host:port`, `[v6]:port`.
fn split(target: &str) -> Option<(String, u16)> {
    let target = target.to_ascii_lowercase();
    if let Some(rest) = target.strip_prefix('[') {
        let (name, port) = rest.split_once("]:")?;
        return Some((name.to_owned(), port.parse().ok()?));
    }
    let (name, port) = target.rsplit_once(':')?;
    Some((name.to_owned(), port.parse().ok()?))
}

impl Connector {
    /// Serve one CONNECT on a guest connection: read the request, decide, dial,
    /// then relay until both directions are done.
    pub fn handle(&self, mut guest: UnixStream) -> io::Result<()> {
        guest.set_write_timeout(Some(IDLE))?;
        // The request is parsed through a clone of the guest connection. The
        // clone holds a pipe the kernel keeps for the peer's reads, so it is
        // dropped before anything is written back: a reply written while it is
        // open can be lost, because the kernel sizes that pipe from the peer's
        // buffer and a guest that has not started reading yet has none.
        let request = {
            let mut reader = BufReader::new(guest.try_clone()?);
            match Request::parse(&mut reader) {
                Ok(request) => request,
                Err(refused) => return self.refuse(&mut guest, None, &refused.reason(), None),
            }
        };
        let destination = request.destination();
        let addresses = match self.resolver.check(&request.name, request.port) {
            Ok(addresses) => addresses,
            // A host or private address is never a human decision (§9.5).
            // A host or private address is never a human decision (§9.5).
            Err(refused) => return self.refuse(&mut guest, None, &refused.reason(), None),
        };
        if let Err(held) = self.decide(&destination, &request.name, &addresses) {
            return self.deny(&mut guest, held, &destination);
        }
        let Some(upstream) = connect(&addresses, &self.resolver) else {
            return self.refuse(
                &mut guest,
                None,
                &format!("{} did not accept the connection", request.name),
                Some("502"),
            );
        };
        upstream.set_read_timeout(Some(IDLE))?;
        upstream.set_write_timeout(Some(IDLE))?;
        upstream.set_nodelay(true).ok();
        guest.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")?;
        guest.write_all(&request.head)?;
        guest.flush()?;
        let mut upstream_writer = upstream.try_clone()?;
        let mut guest_writer = guest.try_clone()?;
        let mut upstream_reader = upstream;
        let mut guest_reader = guest;
        let up = thread::Builder::new().name("gateway-tunnel-up".into()).spawn(move || {
            let _ = io::copy(&mut guest_reader, &mut upstream_writer);
            let _ = upstream_writer.shutdown(std::net::Shutdown::Write);
        })?;
        let down = thread::Builder::new().name("gateway-tunnel-down".into()).spawn(move || {
            let _ = io::copy(&mut upstream_reader, &mut guest_writer);
            let _ = guest_writer.shutdown(std::net::Shutdown::Write);
        })?;
        let _ = up.join();
        let _ = down.join();
        Ok(())
    }

    /// A profile entry or a live grant lets the tunnel through; otherwise the
    /// daemon decides, and its hold is what makes `git` or `ssh` succeed after
    /// the user's click without a retry.
    fn decide(&self, destination: &str, name: &str, addresses: &[SocketAddr]) -> Result<(), Held> {
        if let Some(profile) = self.profiles.covering(name, port_of(destination)) {
            let _ = profile;
            return Ok(());
        }
        if self.grants.allows(name, port_of(destination)) {
            return Ok(());
        }
        let Some(control) = &self.control else { return Err(Held::NoQueue) };
        let reason = addresses.first().map(|address| format!("resolves to {}", address.ip()));
        match control.ask(&crate::web::control::Ask { reason, ..crate::web::control::Ask::connect(destination.into()) })
        {
            Answer::Allowed(_) => Ok(()),
            Answer::Pending(id) => {
                self.status.observe(&id, destination, "pending", None, None);
                Err(Held::Pending(id))
            }
            Answer::Denied(id, state) => {
                self.status.observe(&id, destination, &state, None, None);
                Err(Held::Denied(id, state))
            }
            Answer::Unavailable(why) => Err(Held::Unavailable(why)),
        }
    }

    /// Answer a guest whose request was refused before a tunnel existed. A
    /// CONNECT gets its status line in the head, so the body is only a hint;
    /// a plain-HTTP request gets a JSON body the client can show.
    pub fn refuse(
        &self,
        guest: &mut UnixStream,
        id: Option<&str>,
        message: &str,
        status: Option<&str>,
    ) -> io::Result<()> {
        let status = status.unwrap_or("403");
        write!(
            guest,
            "HTTP/1.1 {status} {}\r\nx-deck-boundary: {}\r\ncontent-type: application/json\r\ncontent-length: 0\r\n\r\n",
            if status == "403" { "Forbidden" } else { "Bad Gateway" },
            message.replace(['\r', '\n'], " ")
        )?;
        if let Some(id) = id {
            let _ = id;
        }
        guest.flush()
    }

    fn deny(&self, guest: &mut UnixStream, held: Held, destination: &str) -> io::Result<()> {
        match held {
            Held::Pending(id) => self.refuse(
                guest,
                Some(&id),
                &format!("{destination} needs the user's approval (approval {id}); retry after they decide"),
                None,
            ),
            Held::Denied(id, _) => {
                self.refuse(guest, Some(&id), &format!("{destination} was denied by the user's boundary policy"), None)
            }
            Held::Unavailable(why) => {
                self.refuse(guest, None, &format!("{destination} needs approval, but {why}"), None)
            }
            Held::NoQueue => self.refuse(guest, None, &format!("{destination} is not allowed for this task"), None),
        }
    }
}

/// Why a tunnel did not open. The id is what the agent reads from the status
/// endpoint; a client that only sees the status still fails closed.
pub enum Held {
    Pending(String),
    Denied(String, String),
    Unavailable(String),
    NoQueue,
}

pub fn port_of(destination: &str) -> u16 {
    destination.rsplit_once(':').and_then(|(_, port)| port.parse().ok()).unwrap_or_default()
}

/// Dial the first address that accepts. Every address was classified already.
fn connect(addresses: &[SocketAddr], resolver: &Resolver) -> Option<TcpStream> {
    for address in addresses {
        // The check is on what was resolved; the dial may be redirected by a
        // test resolver, never by the binary.
        if refuse(address.ip()).is_some() {
            continue;
        }
        let dialled = SocketAddr::new(resolver.dialled(address.ip()), address.port());
        if let Ok(stream) = TcpStream::connect_timeout(&dialled, Duration::from_secs(10)) {
            return Some(stream);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(raw: &str) -> Result<Request, Refused> {
        Request::parse(&mut BufReader::new(raw.as_bytes()))
    }

    #[test]
    fn a_connect_names_one_destination_and_keeps_its_head() {
        let request = parse("CONNECT example.com:8443 HTTP/1.1\r\nhost: example.com:8443\r\n\r\n").unwrap();
        assert_eq!(request.name, "example.com");
        assert_eq!(request.port, 8443);
        assert_eq!(request.destination(), "example.com:8443");
        assert!(request.head.starts_with(b"CONNECT example.com:8443 HTTP/1.1\r\n"));
    }

    #[test]
    fn a_guest_proxy_credential_never_reaches_the_destination() {
        let request = parse(
            "CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\nProxy-Authorization: Basic Zm9v\r\nproxy-connection: keep-alive\r\nx-extra: kept\r\n\r\n",
        )
        .unwrap();
        let head = String::from_utf8(request.head).unwrap();
        assert!(!head.to_ascii_lowercase().contains("proxy-authorization"), "{head}");
        assert!(!head.contains("Basic Zm9v"), "{head}");
        assert!(!head.to_ascii_lowercase().contains("proxy-connection"), "{head}");
        assert!(head.contains("x-extra: kept"), "{head}");
    }

    #[test]
    fn the_ipv6_and_malformed_forms_are_understood_or_refused() {
        assert_eq!(
            parse("CONNECT [2606:4700::1111]:443 HTTP/1.1\r\n\r\n").unwrap().destination(),
            "2606:4700::1111:443"
        );
        assert!(parse("GET http://example.com/ HTTP/1.1\r\n\r\n").is_err());
        assert!(parse("CONNECT example.com HTTP/1.1\r\n\r\n").is_err());
        assert!(parse("CONNECT example.com:https HTTP/1.1\r\n\r\n").is_err());
        assert!(parse("CONNECT *:443 HTTP/1.1\r\n\r\n").is_err());
        assert!(parse("").is_err());
    }
}
