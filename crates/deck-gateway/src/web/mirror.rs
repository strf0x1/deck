//! Plain HTTP for declared mirrors (ADR 0004 §3).
//!
//! The design choice: plain HTTP is only for mirrors, and every redirect is a
//! new request. The guest sends an absolute-URI request; the host resolves the
//! name, applies the same refusal as a tunnel — and so does every `Location`
//! it follows or returns. A redirect to another destination is re-checked and
//! asks for its own scope rather than inheriting the first one. `absolute-form`
//! is the only shape accepted: an origin-form request would mean the client
//! wrote its own `Host`, which is a different destination than it named.

use crate::http::{self, Request};
use crate::web::connect::Connector;
use crate::web::policy::PLAIN_PORT;
use crate::web::resolve::Resolver;
use crate::web::{Answer, refuse};
use std::io::{self, Read as _, Write as _};
use std::net::{SocketAddr, TcpStream};
use std::os::unix::net::UnixStream;
use std::time::Duration;

/// Redirects followed for one request. A mirror chain is short; a longer one
/// is a loop or a client chasing something the host does not vouch for.
const HOPS: usize = 5;
const CONNECT: Duration = Duration::from_secs(10);
const IDLE: Duration = Duration::from_secs(120);
/// The response head kept for the guest. A mirror's head is far smaller.
pub const MAX_RESPONSE_HEAD: usize = 32 * 1024;

/// One absolute-URI request target: `http://host[:port]/path`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub name: String,
    pub port: u16,
    pub path: String,
}

impl Target {
    pub fn parse(uri: &str) -> Option<Self> {
        let rest = uri.strip_prefix("http://")?;
        // The authority ends at the first `/`, `?` or `#`; the path keeps the
        // separator. `char_indices` finds it without slicing a byte boundary.
        let authority_end = rest.char_indices().find(|(_, c)| matches!(c, '/' | '?' | '#')).map(|(index, _)| index);
        let (authority, path) = match authority_end {
            Some(end) => rest.split_at(end),
            None => (rest, "/"),
        };
        if authority.is_empty() || authority.contains('@') {
            return None;
        }
        let (name, port) = if let Some(rest) = authority.strip_prefix('[') {
            let (name, port) = rest.split_once(']')?;
            match port.strip_prefix(':') {
                Some(port) => (name, port.parse().ok()?),
                None => (name, PLAIN_PORT),
            }
        } else {
            match authority.rsplit_once(':') {
                Some((name, port)) => (name, port.parse().ok()?),
                None => (authority, PLAIN_PORT),
            }
        };
        if !crate::web::policy::is_host(&name.to_ascii_lowercase()) {
            return None;
        }
        Some(Self { name: name.to_ascii_lowercase(), port, path: path.to_owned() })
    }

    pub fn destination(&self) -> String {
        format!("{}:{}", self.name, self.port)
    }
}

impl Connector {
    /// Serve one plain-HTTP request from the guest, following redirects on the
    /// host so each one is checked where it lands.
    pub fn handle_plain(&self, mut guest: UnixStream) -> io::Result<()> {
        guest.set_read_timeout(Some(IDLE))?;
        guest.set_write_timeout(Some(IDLE))?;
        let request = match http::read_shape(&mut io::BufReader::new(guest.try_clone()?), http::Shape::Absolute) {
            Ok(Some(request)) => request,
            Ok(None) => return Ok(()),
            Err(http::ReadError::Refused { status, message }) => {
                return http::write_error(&mut guest, status, "deck_bad_request", &message);
            }
            Err(http::ReadError::Io(error)) => return Err(error),
        };
        let Some(target) = Target::parse(&request.target) else {
            return http::write_error(
                &mut guest,
                400,
                "deck_bad_request",
                "plain HTTP is served only for absolute http:// requests to a declared mirror",
            );
        };
        // One request is one connection; the head is rebuilt with the
        // origin-form target so the mirror sees a normal HTTP/1.1 request.
        let method = request.method.clone();
        let mut next = Some(target.clone());
        let mut hops = 0;
        while let Some(current) = next.take() {
            let addresses = match self.resolver.check(&current.name, current.port) {
                Ok(addresses) => addresses,
                Err(refused) => return self.deny_plain(&mut guest, None, &refused.reason()),
            };
            let permit = match self.decide_mirror(&current, &request) {
                Ok(permit) => permit,
                Err(held) => return self.deny_held(&mut guest, held, &current.destination()),
            };
            let Some(mut upstream) = dial(&addresses, &self.resolver) else {
                return self.deny_plain(&mut guest, None, &format!("{} did not accept the connection", current.name));
            };
            let _registration = match self.register(permit, &guest, &upstream) {
                Ok(registration) => registration,
                Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                    return self.deny_plain(&mut guest, None, &error.to_string());
                }
                Err(error) => return Err(error),
            };
            if let Err(error) = send(&mut upstream, &method, &current, &request) {
                return self.deny_plain(&mut guest, None, &format!("the request to {} failed: {error}", current.name));
            }
            let (status, headers) = match read_head(&mut upstream) {
                Ok(parts) => parts,
                Err(error) => {
                    return self.deny_plain(
                        &mut guest,
                        None,
                        &format!("{} answered an unreadable head: {error}", current.name),
                    );
                }
            };
            match redirect(&status, &headers, &current) {
                Ok(Some(location)) => {
                    hops += 1;
                    if hops > HOPS {
                        return self.deny_plain(&mut guest, None, "the redirect chain is too long");
                    }
                    next = Some(location);
                }
                Ok(None) => return relay(&mut guest, &mut upstream, &status, &headers),
                Err(reason) => return self.deny_plain(&mut guest, None, reason),
            }
        }
        self.deny_plain(&mut guest, None, "the redirect chain ended nowhere")
    }

    /// A mirror destination or a redirect target needs its own scope. A
    /// profile entry covers the destination; a live grant covers it until it
    /// is revoked; otherwise the daemon decides, and a held request is one the
    /// user can allow while the client waits.
    fn decide_mirror(
        &self,
        target: &Target,
        request: &Request,
    ) -> Result<Option<crate::grants::Permit>, crate::web::connect::Held> {
        use crate::web::connect::Held;
        if self.profiles.covering(&target.name, target.port).is_some() {
            return Ok(None);
        }
        if let Some(permit) = self.grants.web(&target.name, target.port, crate::web::FETCH) {
            return Ok(Some(permit));
        }
        let Some(control) = &self.control else { return Err(Held::NoQueue) };
        let ask = crate::web::control::Ask::fetch(
            target.destination(),
            &request.method,
            &target.path,
            Some(request.body.len() as u64),
            None,
        );
        match control.ask(&ask) {
            Answer::Allowed(scope) => self.allowed(&scope, &target.name, target.port, crate::web::FETCH),
            Answer::Pending(id) => {
                self.status.observe(&id, &target.destination(), "pending", None, None);
                Err(Held::Pending(id))
            }
            Answer::Denied(id, state) => {
                self.status.observe(&id, &target.destination(), &state, None, None);
                Err(Held::Denied(id, state))
            }
            Answer::Unavailable(why) => Err(Held::Unavailable(why)),
        }
    }

    fn deny_held(&self, guest: &mut UnixStream, held: crate::web::connect::Held, destination: &str) -> io::Result<()> {
        use crate::web::connect::Held;
        match held {
            Held::Pending(id) => self.deny_plain(
                guest,
                Some(&id),
                &format!("{destination} needs the user's approval (approval {id}); retry after they decide"),
            ),
            Held::Denied(id, _) => {
                let _ = id;
                self.deny_plain(guest, None, &format!("{destination} was denied by the user's boundary policy"))
            }
            Held::Unavailable(why) => self.deny_plain(guest, None, &format!("{destination} needs approval, but {why}")),
            Held::NoQueue => self.deny_plain(guest, None, &format!("{destination} is not allowed for this task")),
        }
    }

    fn deny_plain(&self, guest: &mut UnixStream, id: Option<&str>, message: &str) -> io::Result<()> {
        http::write_denied(guest, id, id.is_some(), message)
    }
}

/// Send one request to one mirror. The guest's own `Host` is replaced by the
/// destination the host dialled, and a proxy credential stays behind.
fn send(upstream: &mut TcpStream, method: &str, target: &Target, request: &Request) -> io::Result<()> {
    let mut head = format!("{method} {} HTTP/1.1\r\nhost: {}\r\n", target.path, target.destination());
    for (name, value) in &request.headers {
        if name == "host" || name == "proxy-authorization" || name == "proxy-connection" || name == "connection" {
            continue;
        }
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    if !request.body.is_empty() || method == "POST" || method == "PUT" {
        head.push_str(&format!("content-length: {}\r\n", request.body.len()));
    }
    head.push_str("\r\n");
    upstream.set_read_timeout(Some(IDLE))?;
    upstream.set_write_timeout(Some(IDLE))?;
    upstream.write_all(head.as_bytes())?;
    upstream.write_all(&request.body)?;
    upstream.flush()
}

/// A response status line and its lowered headers. The byte-at-a-time read
/// stops exactly after the terminator, leaving all body bytes upstream.
type ResponseHead = (String, Vec<(String, String)>);

fn read_head(upstream: &mut TcpStream) -> io::Result<ResponseHead> {
    let mut raw = Vec::new();
    let mut byte = [0u8; 1];
    while !raw.ends_with(b"\r\n\r\n") {
        if raw.len() >= MAX_RESPONSE_HEAD {
            return Err(io::Error::other("response head is too large"));
        }
        if upstream.read(&mut byte)? == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "connection closed in the response head"));
        }
        raw.push(byte[0]);
    }
    raw.truncate(raw.len() - 4);
    let text = String::from_utf8(raw).map_err(|_| io::Error::other("response head is not UTF-8"))?;
    let mut lines = text.split("\r\n");
    let status_line = lines.next().unwrap_or_default().to_owned();
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(io::Error::other("malformed response header"));
        };
        headers.push((name.to_ascii_lowercase(), value.trim().to_owned()));
    }
    Ok((status_line, headers))
}

/// A redirect must name a usable destination; an unusable `Location` is a
/// refusal, distinct from a non-redirect response. Root-relative paths keep
/// the current destination, while scheme-relative and other schemes fail closed.
fn redirect(status: &str, headers: &[(String, String)], current: &Target) -> Result<Option<Target>, &'static str> {
    let code = status.split(' ').nth(1).and_then(|value| value.parse::<u16>().ok());
    if !matches!(code, Some(301 | 302 | 303 | 307 | 308)) {
        return Ok(None);
    }
    let location = headers
        .iter()
        .find(|(name, _)| name == "location")
        .map(|(_, value)| value)
        .ok_or("the redirect has no Location target")?;
    if location.starts_with("http://") {
        return Target::parse(location).map(Some).ok_or("the redirect has an invalid HTTP target");
    }
    if location.starts_with('/') && !location.starts_with("//") {
        return Ok(Some(Target { name: current.name.clone(), port: current.port, path: location.clone() }));
    }
    Err("the redirect target must be an absolute http:// URI or a root-relative path")
}

/// Answer the guest with the mirror's response, unchanged but for the framing
/// the gateway itself needs (`content-length` or chunked, never both).
fn relay(
    guest: &mut UnixStream,
    upstream: &mut TcpStream,
    status: &str,
    headers: &[(String, String)],
) -> io::Result<()> {
    write!(guest, "{status}\r\n")?;
    for (name, value) in headers {
        if name == "transfer-encoding" || name == "connection" {
            continue;
        }
        write!(guest, "{name}: {value}\r\n")?;
    }
    write!(guest, "connection: close\r\n\r\n")?;
    io::copy(upstream, guest)?;
    guest.flush()
}

/// Dial the first address that accepts.
fn dial(addresses: &[SocketAddr], resolver: &Resolver) -> Option<TcpStream> {
    for address in addresses {
        if refuse(address.ip()).is_some() {
            continue;
        }
        let dialled = SocketAddr::new(resolver.dialled(address.ip()), address.port());
        if let Ok(stream) = TcpStream::connect_timeout(&dialled, CONNECT) {
            stream.set_nodelay(true).ok();
            return Some(stream);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absolute_uri_names_a_destination_and_keeps_the_path() {
        let target = Target::parse("http://deb.debian.org/debian/dists/stable/Release").unwrap();
        assert_eq!(target.name, "deb.debian.org");
        assert_eq!(target.port, 80);
        assert_eq!(target.path, "/debian/dists/stable/Release");
        assert_eq!(Target::parse("http://registry.example:8443/v2/").unwrap().port, 8443);
        assert_eq!(Target::parse("http://[2606:4700::1111]:8080/x").unwrap().name, "2606:4700::1111");
    }

    #[test]
    fn https_credentials_and_other_schemes_are_not_plain_http_targets() {
        for bad in ["https://crates.io/", "ftp://x/", "http://user:pass@crates.io/", "http:///path", "/path"] {
            assert!(Target::parse(bad).is_none(), "{bad} is not a plain-http destination");
        }
    }

    #[test]
    fn a_redirect_is_a_destination_of_its_own() {
        let current = Target::parse("http://deb.debian.org/x").unwrap();
        for code in [301, 302, 303, 307, 308] {
            let status = format!("HTTP/1.1 {code} Redirect");
            let moved =
                redirect(&status, &[("location".into(), "http://pypi.org/simple/".into())], &current).unwrap().unwrap();
            assert_eq!(moved.destination(), "pypi.org:80");
            let relative = redirect(&status, &[("location".into(), "/elsewhere".into())], &current).unwrap().unwrap();
            assert_eq!(relative.destination(), "deb.debian.org:80");
            assert_eq!(relative.path, "/elsewhere");
            for invalid in [
                "https://crates.io/",
                "ftp://mirror.example/file",
                "//other.example/file",
                "///file",
                "http:///path",
                "http://mirror.example:bad/",
                "*",
                "",
                "relative-file",
            ] {
                assert!(
                    redirect(&status, &[("location".into(), invalid.into())], &current).is_err(),
                    "{code}: {invalid}"
                );
            }
            assert!(redirect(&status, &[], &current).is_err());
        }
        assert_eq!(redirect("HTTP/1.1 200 OK", &[("location".into(), "https://x/".into())], &current), Ok(None));
        assert_eq!(redirect("HTTP/1.1 404 Not Found", &[], &current), Ok(None));
    }
}
