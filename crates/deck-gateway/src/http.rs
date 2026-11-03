//! Bounded HTTP/1.1 for the gateway's Unix-socket listeners.
//!
//! The design choice: one request per connection, origin-form target, and a
//! body only by `Content-Length`. The guest is untrusted, so every read has a
//! byte limit, chunked request bodies and `Expect` are refused instead of
//! parsed, and every response says `Connection: close`. Streaming responses
//! leave in chunked encoding so the gateway never buffers an SSE turn.

use serde_json::json;
use std::io::{self, BufRead, Write};

/// A request line plus headers must fit in this, so a hostile client cannot
/// make the gateway allocate without bound.
pub const MAX_HEAD: usize = 32 * 1024;
/// A model request carries a conversation; 32 MiB leaves room for a large
/// context and still bounds one connection's memory.
pub const MAX_BODY: usize = 32 * 1024 * 1024;
const MAX_HEADERS: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub method: String,
    pub target: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str())
    }
}

/// Why a request could not be read. A refusal already has a status and a
/// message the listener can return; an IO error means the peer is gone.
#[derive(Debug)]
pub enum ReadError {
    Refused { status: u16, message: String },
    Io(io::Error),
}

impl From<io::Error> for ReadError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

fn refused(status: u16, message: impl Into<String>) -> ReadError {
    ReadError::Refused { status, message: message.into() }
}

/// Read one request. `Ok(None)` is a clean EOF before any byte of a request,
/// which a keep-alive client produces when it closes an idle connection.
pub fn read(reader: &mut impl BufRead) -> Result<Option<Request>, ReadError> {
    let Some(head) = read_head(reader)? else {
        return Ok(None);
    };
    parse(&head, reader).map(Some)
}

fn read_head(reader: &mut impl BufRead) -> Result<Option<Vec<u8>>, ReadError> {
    const END: [u8; 4] = *b"\r\n\r\n";
    let mut head = Vec::with_capacity(1024);
    let mut matched = 0;
    loop {
        if head.len() >= MAX_HEAD {
            return Err(refused(431, format!("request head exceeds {MAX_HEAD} bytes")));
        }
        let Some(byte) = take(reader)? else {
            if head.is_empty() {
                return Ok(None);
            }
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "connection closed in the request head").into());
        };
        head.push(byte);
        matched = if byte == END[matched] {
            matched + 1
        } else if byte == b'\r' {
            1
        } else {
            0
        };
        if matched == END.len() {
            head.truncate(head.len() - END.len());
            return Ok(Some(head));
        }
    }
}

fn take(reader: &mut impl BufRead) -> io::Result<Option<u8>> {
    let available = reader.fill_buf()?;
    if available.is_empty() {
        return Ok(None);
    }
    let byte = available[0];
    reader.consume(1);
    Ok(Some(byte))
}

fn parse(head: &[u8], reader: &mut impl BufRead) -> Result<Request, ReadError> {
    let text = std::str::from_utf8(head).map_err(|_| refused(400, "request head is not UTF-8"))?;
    let mut lines = text.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut words = request_line.split(' ');
    let (Some(method), Some(target), Some(version), None) = (words.next(), words.next(), words.next(), words.next())
    else {
        return Err(refused(400, "malformed request line"));
    };
    if !method.bytes().all(|b| b.is_ascii_uppercase()) || method.is_empty() {
        return Err(refused(400, "malformed method"));
    }
    if !target.starts_with('/') {
        return Err(refused(400, "only origin-form request targets are accepted"));
    }
    if version != "HTTP/1.1" && version != "HTTP/1.0" {
        return Err(refused(400, "only HTTP/1.x is accepted"));
    }

    let mut headers: Vec<(String, String)> = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if headers.len() == MAX_HEADERS {
            return Err(refused(431, "too many headers"));
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(refused(400, "malformed header line"));
        };
        let name = name.to_ascii_lowercase();
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)) {
            return Err(refused(400, "malformed header name"));
        }
        let value = value.trim_matches(|c| c == ' ' || c == '\t');
        if value.bytes().any(|b| b < 0x20 && b != b'\t') {
            return Err(refused(400, "control byte in a header value"));
        }
        headers.push((name, value.to_string()));
    }

    if headers.iter().any(|(name, _)| name == "transfer-encoding") {
        return Err(refused(400, "chunked request bodies are not accepted"));
    }
    if headers.iter().any(|(name, value)| name == "expect" && value.eq_ignore_ascii_case("100-continue")) {
        return Err(refused(417, "100-continue is not supported"));
    }
    let length = match headers.iter().find(|(name, _)| name == "content-length") {
        Some((_, value)) => value.parse::<usize>().map_err(|_| refused(400, "content-length is not a number"))?,
        None => 0,
    };
    if length > MAX_BODY {
        return Err(refused(413, format!("request body exceeds {MAX_BODY} bytes")));
    }

    let mut body = vec![0; length];
    read_exact(reader, &mut body)?;
    Ok(Request { method: method.to_string(), target: target.to_string(), headers, body })
}

fn read_exact(reader: &mut impl BufRead, body: &mut [u8]) -> Result<(), ReadError> {
    reader.read_exact(body).map_err(|error| {
        if error.kind() == io::ErrorKind::UnexpectedEof {
            refused(400, "connection closed in the request body")
        } else {
            ReadError::Io(error)
        }
    })?;
    Ok(())
}

/// Write a complete response and close the connection.
pub fn write(stream: &mut impl Write, status: u16, headers: &[(&str, String)], body: &[u8]) -> io::Result<()> {
    write!(stream, "HTTP/1.1 {status} {}\r\n", reason(status))?;
    for (name, value) in headers {
        write!(stream, "{name}: {value}\r\n")?;
    }
    write!(stream, "content-length: {}\r\nconnection: close\r\n\r\n", body.len())?;
    stream.write_all(body)?;
    stream.flush()
}

/// Write the head of a chunked response. The body follows with
/// [`write_chunk`] and [`end_stream`].
pub fn write_stream_head(stream: &mut impl Write, status: u16, content_type: &str) -> io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status} {}\r\ncontent-type: {content_type}\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n",
        reason(status)
    )
}

pub fn write_chunk(stream: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
    if bytes.is_empty() {
        return Ok(());
    }
    write!(stream, "{:x}\r\n", bytes.len())?;
    stream.write_all(bytes)?;
    stream.write_all(b"\r\n")?;
    stream.flush()
}

pub fn end_stream(stream: &mut impl Write) -> io::Result<()> {
    stream.write_all(b"0\r\n\r\n")?;
    stream.flush()
}

/// The gateway's structured denial. The type is stable so Construct can tell
/// policy from an outage; the message is the only free text.
pub fn write_error(stream: &mut impl Write, status: u16, code: &str, message: &str) -> io::Result<()> {
    let body = json!({ "error": { "message": message, "type": code, "code": code } }).to_string();
    write(stream, status, &[("content-type", "application/json".into())], body.as_bytes())
}

/// A boundary denial that names the approval item (ADR 0004 §5). Construct
/// reads `approval_id` from the body or `X-Deck-Approval`; clients that only
/// see the status still get a bounded, retryable failure.
pub fn write_denied(stream: &mut impl Write, approval: Option<&str>, retryable: bool, message: &str) -> io::Result<()> {
    let mut body = json!({
        "error": { "message": message, "type": "deck_boundary_denied", "code": "deck_boundary_denied" },
        "retryable": retryable,
    });
    let mut headers = vec![("content-type", "application/json".to_string())];
    if let Some(id) = approval {
        body["approval_id"] = json!(id);
        headers.push(("x-deck-approval", id.to_string()));
    }
    write(stream, 403, &headers, body.to_string().as_bytes())
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        411 => "Length Required",
        413 => "Payload Too Large",
        417 => "Expectation Failed",
        431 => "Request Header Fields Too Large",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn read_from(raw: &[u8]) -> Result<Option<Request>, ReadError> {
        read(&mut Cursor::new(raw))
    }

    #[test]
    fn reads_a_content_length_body() {
        let raw = b"POST /v1/chat/completions HTTP/1.1\r\nHost: relay\r\nContent-Type: application/json\r\nContent-Length: 13\r\n\r\n{\"model\":\"x\"}";
        let request = read_from(raw).unwrap().unwrap();
        assert_eq!(request.method, "POST");
        assert_eq!(request.target, "/v1/chat/completions");
        assert_eq!(request.header("content-type"), Some("application/json"));
        assert_eq!(request.body, b"{\"model\":\"x\"}");
    }

    #[test]
    fn clean_eof_before_a_request_is_not_an_error() {
        assert!(read_from(b"").unwrap().is_none());
    }

    #[test]
    fn refuses_chunked_requests_and_expect() {
        let chunked = b"POST /v1/chat/completions HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n";
        assert!(matches!(read_from(chunked), Err(ReadError::Refused { status: 400, .. })));
        let expect = b"POST /v1/chat/completions HTTP/1.1\r\nExpect: 100-continue\r\nContent-Length: 0\r\n\r\n";
        assert!(matches!(read_from(expect), Err(ReadError::Refused { status: 417, .. })));
    }

    #[test]
    fn refuses_a_body_over_the_limit() {
        let raw = format!("POST /v1/chat/completions HTTP/1.1\r\nContent-Length: {}\r\n\r\n", MAX_BODY + 1);
        assert!(matches!(read_from(raw.as_bytes()), Err(ReadError::Refused { status: 413, .. })));
    }

    #[test]
    fn refuses_an_oversized_head() {
        let mut raw = b"GET /v1/models HTTP/1.1\r\n".to_vec();
        raw.extend(std::iter::repeat_n(b'X', MAX_HEAD));
        assert!(matches!(read_from(&raw), Err(ReadError::Refused { status: 431, .. })));
    }

    #[test]
    fn refuses_a_proxy_form_target() {
        let raw = b"GET http://example.com/ HTTP/1.1\r\n\r\n";
        assert!(matches!(read_from(raw), Err(ReadError::Refused { status: 400, .. })));
    }

    #[test]
    fn a_boundary_denial_names_the_approval() {
        let mut out = Vec::new();
        write_denied(&mut out, Some("a1f3"), true, "needs approval").unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("HTTP/1.1 403 Forbidden\r\n"));
        assert!(text.contains("x-deck-approval: a1f3"), "{text}");
        assert!(text.contains("\"approval_id\":\"a1f3\""), "{text}");
        assert!(text.contains("\"retryable\":true"), "{text}");
        let mut out = Vec::new();
        write_denied(&mut out, None, false, "queue down").unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(!text.contains("approval_id"), "{text}");
        assert!(text.contains("\"retryable\":false"), "{text}");
    }

    #[test]
    fn writes_a_bounded_error_body() {
        let mut out = Vec::new();
        write_error(&mut out, 403, "deck_boundary_denied", "no").unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("HTTP/1.1 403 Forbidden\r\n"));
        assert!(text.contains("connection: close"));
        assert!(text.contains("\"code\":\"deck_boundary_denied\""));
    }
}
