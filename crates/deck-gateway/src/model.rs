//! The OpenAI-compatible model gateway.
//!
//! The design choice: the guest request is rebuilt, not forwarded. Only the
//! listed paths are served, only allowlisted headers survive, the model field
//! is checked on the host, and the real provider key is added after every
//! guest header was discarded. Responses stream back with chunked encoding, so
//! an SSE turn is not buffered until it ends.

use crate::http::{self, ReadError, Request};
use crate::policy::{self, Models};
use std::io::{self, BufReader, Read as _};
use std::os::unix::net::UnixStream;
use std::time::Duration;

/// The paths Construct uses. Anything else is a policy denial, not a proxy.
fn allowed(method: &str, target: &str) -> bool {
    matches!((method, target), ("POST", "/v1/chat/completions") | ("GET" | "HEAD", "/v1/models"))
}

pub struct Model {
    upstream: String,
    models: Models,
    key: Option<String>,
    secret: String,
    agent: ureq::Agent,
}

impl Model {
    /// `upstream` is the provider base URL including its version segment
    /// (`https://openrouter.ai/api/v1`); `key` is the host-side credential for
    /// that provider. A missing key denies instead of forwarding the guest's
    /// placeholder.
    pub fn new(upstream: String, models: Models, key: Option<String>, secret: String) -> Self {
        let config = ureq::Agent::config_builder()
            // The gateway decides what a non-2xx means; it is not an ureq error.
            .http_status_as_error(false)
            // A redirect could leave the approved destination. Providers do
            // not need them on these paths.
            .max_redirects(0)
            .timeout_global(None)
            .timeout_connect(Some(Duration::from_secs(20)))
            .timeout_recv_response(Some(Duration::from_secs(120)))
            .build();
        Self { upstream: upstream.trim_end_matches('/').to_string(), models, key, secret, agent: config.new_agent() }
    }

    /// Read one request and answer it. The connection always closes after the
    /// response; a new request uses a new connection, which is also the
    /// granularity at which policy is applied.
    pub fn handle(&self, mut stream: UnixStream) -> io::Result<()> {
        stream.set_read_timeout(Some(Duration::from_secs(60)))?;
        stream.set_write_timeout(Some(Duration::from_secs(120)))?;
        let request = match http::read(&mut BufReader::new(&stream)) {
            Ok(Some(request)) => request,
            Ok(None) => return Ok(()),
            Err(ReadError::Refused { status, message }) => {
                return http::write_error(&mut stream, status, "deck_bad_request", &message);
            }
            Err(ReadError::Io(error)) => return Err(error),
        };

        if !allowed(&request.method, &request.target) {
            return http::write_error(
                &mut stream,
                403,
                "deck_boundary_denied",
                &format!("{} is not served by this task's model gateway", request.target),
            );
        }

        if request.method == "POST" {
            let Some(model) = requested_model(&request) else {
                return http::write_error(&mut stream, 400, "deck_invalid_body", "the request has no model");
            };
            if !self.models.allows(&model) {
                return http::write_error(
                    &mut stream,
                    403,
                    "deck_boundary_denied",
                    &format!("model {model:?} is not allowed for this task"),
                );
            }
        }

        let Some(key) = &self.key else {
            return http::write_error(
                &mut stream,
                503,
                "deck_credential_missing",
                &format!("the host has no value for {}; the guest placeholder is never forwarded", self.secret),
            );
        };

        let suffix = request.target.strip_prefix("/v1").unwrap_or_default();
        let url = format!("{}{}", self.upstream, suffix);
        let response = match request.method.as_str() {
            "POST" => {
                let mut builder = self.agent.post(&url);
                for (name, value) in &request.headers {
                    if policy::retained(name) {
                        builder = builder.header(name.as_str(), value.as_str());
                    }
                }
                builder.header("authorization", format!("Bearer {key}")).send(request.body.as_slice())
            }
            _ => {
                let mut builder = if request.method == "GET" { self.agent.get(&url) } else { self.agent.head(&url) };
                for (name, value) in &request.headers {
                    if policy::retained(name) {
                        builder = builder.header(name.as_str(), value.as_str());
                    }
                }
                builder.header("authorization", format!("Bearer {key}")).call()
            }
        };
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                eprintln!("deck-gateway: upstream request failed: {error}");
                return http::write_error(&mut stream, 502, "deck_upstream_failed", "the provider request failed");
            }
        };

        let (parts, body) = response.into_parts();
        let status = parts.status.as_u16();
        let content_type =
            parts.headers.get("content-type").and_then(|value| value.to_str().ok()).unwrap_or("application/json");
        http::write_stream_head(&mut stream, status, content_type)?;
        let mut reader = body.into_reader();
        let mut buffer = [0u8; 16 * 1024];
        loop {
            let read = reader.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            http::write_chunk(&mut stream, &buffer[..read])?;
        }
        http::end_stream(&mut stream)
    }
}

/// The model field from a parsed JSON body. A body that is not a JSON object
/// with a string model has no model, so it is a bad request.
fn requested_model(request: &Request) -> Option<String> {
    let body: serde_json::Value = serde_json::from_slice(&request.body).ok()?;
    body.get("model")?.as_str().map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_construct_paths_are_served() {
        assert!(allowed("POST", "/v1/chat/completions"));
        assert!(allowed("GET", "/v1/models"));
        assert!(allowed("HEAD", "/v1/models"));
        assert!(!allowed("GET", "/v1/chat/completions"));
        assert!(!allowed("POST", "/v1/models"));
        assert!(!allowed("POST", "/v1/embeddings"));
        assert!(!allowed("GET", "/v1/models/evil"));
        assert!(!allowed("POST", "/"));
    }

    #[test]
    fn missing_model_is_not_a_model() {
        let request = Request {
            method: "POST".into(),
            target: "/v1/chat/completions".into(),
            headers: vec![],
            body: br#"{"messages":[]}"#.to_vec(),
        };
        assert_eq!(requested_model(&request), None);
        let broken = Request { body: b"not json".to_vec(), ..request };
        assert_eq!(requested_model(&broken), None);
    }
}
