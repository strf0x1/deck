//! JSON-RPC 2.0 as ACP uses it: one JSON object per line, in both directions.
//! Pure functions from values to lines and lines to [`Incoming`]; the client
//! owns its I/O and only uses these.
//!
//! The design choice: ids are opaque `Value`s on the way in (a peer may use
//! numbers or strings, and a response must echo it exactly), and plain `u64`s
//! on the way out, which is all deck ever sends. Mirrors construct's
//! `acp/rpc.rs` so the two ends of the wire read the same.

use serde_json::{Value, json};

pub const METHOD_NOT_FOUND: i64 = -32601;
const INTERNAL: i64 = -32603;

#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    Request { id: Value, method: String, params: Value },
    Notification { method: String, params: Value },
    Response { id: Value, result: Result<Value, String> },
}

/// One line from the peer, or `None` for anything that isn't a JSON-RPC
/// message (blank lines, stray output): a reader skips those.
pub fn parse(line: &str) -> Option<Incoming> {
    let v: Value = serde_json::from_str(line.trim()).ok()?;
    let obj = v.as_object()?;
    let method = obj.get("method").and_then(Value::as_str).map(str::to_string);
    let params = obj.get("params").cloned().unwrap_or(Value::Null);
    match (method, obj.get("id")) {
        (Some(method), Some(id)) => Some(Incoming::Request { id: id.clone(), method, params }),
        (Some(method), None) => Some(Incoming::Notification { method, params }),
        (None, Some(id)) => {
            let result = match obj.get("error") {
                Some(e) => Err(error_text(e)),
                None => Ok(obj.get("result").cloned().unwrap_or(Value::Null)),
            };
            Some(Incoming::Response { id: id.clone(), result })
        }
        (None, None) => None,
    }
}

/// An error's message with the reason an agent put in `data`.
fn error_text(e: &Value) -> String {
    let code = e.get("code").and_then(Value::as_i64).unwrap_or(INTERNAL);
    let message = e.get("message").and_then(Value::as_str).unwrap_or("");
    match e.get("data") {
        Some(Value::String(d)) if !d.is_empty() => format!("{message}: {d} ({code})"),
        _ => format!("{message} ({code})"),
    }
}

fn line(v: Value) -> String {
    format!("{v}\n")
}

pub fn request(id: u64, method: &str, params: Value) -> String {
    line(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
}

pub fn notification(method: &str, params: Value) -> String {
    line(json!({ "jsonrpc": "2.0", "method": method, "params": params }))
}

pub fn ok(id: &Value, result: Value) -> String {
    line(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

pub fn err(id: &Value, code: i64, message: &str) -> String {
    line(json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_requests_notifications_and_responses() {
        let r = parse(r#"{"jsonrpc":"2.0","id":"a","method":"m","params":{"x":1}}"#);
        assert_eq!(r, Some(Incoming::Request { id: json!("a"), method: "m".into(), params: json!({"x":1}) }));
        let n = parse(r#"{"jsonrpc":"2.0","method":"n"}"#);
        assert_eq!(n, Some(Incoming::Notification { method: "n".into(), params: Value::Null }));
        let e = parse(r#"{"jsonrpc":"2.0","id":3,"error":{"code":-32602,"message":"bad","data":"why"}}"#);
        assert_eq!(e, Some(Incoming::Response { id: json!(3), result: Err("bad: why (-32602)".into()) }));
        assert_eq!(parse("not json"), None);
        assert_eq!(parse("[]"), None);
    }

    #[test]
    fn lines_are_newline_terminated_json() {
        let l = request(7, "session/new", json!({}));
        assert!(l.ends_with('\n'));
        assert_eq!(
            parse(&l),
            Some(Incoming::Request { id: json!(7), method: "session/new".into(), params: json!({}) })
        );
    }
}
