//! The OpenAI-compatible model gateway.
//!
//! The design choice: the guest request is rebuilt, not forwarded. Only the
//! listed paths are served, only allowlisted headers survive, the model field
//! is checked on the host, and the real provider key is added after every
//! guest header was discarded. The key is fetched from `deckd` for this one
//! request, after policy and the budget admitted it, and is dropped with the
//! response. Responses stream back with chunked encoding, so an SSE turn is
//! not buffered until it ends. Revoking its authorizing grant or key interrupts
//! the request, including an idle provider read.

mod admission;
mod operation;

use crate::control::Control;
use crate::credential::Broker;
use crate::grants::Grants;
use crate::http::{self, ReadError, Request};
use crate::policy::{self, Budget, Ledger, Models, Usage};
use crate::status::Status;
use crate::usage::Meter;
use operation::{Operation, Revocation};
use std::io::{self, BufReader, Read as _};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// The paths Construct uses. Anything else is a policy denial, not a proxy.
fn allowed(method: &str, target: &str) -> bool {
    matches!((method, target), ("POST", "/v1/chat/completions") | ("GET" | "HEAD", "/v1/models"))
}

/// What the server hands one model gateway. `budget_file` is the task's live
/// budget, rewritten by `deckd`; it overrides the process-start limits on the
/// next admission without restarting the VM.
pub struct Settings {
    pub upstream: String,
    pub models: Models,
    pub grants: Arc<Grants>,
    /// Fetches the provider key per request and closes requests whose key
    /// was revoked.
    pub credentials: Arc<Broker>,
    /// The credential name the key is stored under.
    pub secret: String,
    pub control: Option<Control>,
    pub status: Arc<Status>,
    pub budget: Budget,
    pub usage: Usage,
    pub budget_file: Option<PathBuf>,
}

pub struct Model {
    upstream: String,
    models: Models,
    grants: Arc<Grants>,
    credentials: Arc<Broker>,
    secret: String,
    control: Option<Control>,
    status: Arc<Status>,
    ledger: Arc<Ledger>,
    budget_file: Option<PathBuf>,
    agent_config: ureq::config::Config,
}

impl Model {
    /// `upstream` is the provider base URL including its version segment
    /// (`https://openrouter.ai/api/v1`). A key `deckd` cannot supply denies
    /// instead of forwarding the guest's placeholder.
    pub fn new(settings: Settings) -> Self {
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
        Self {
            upstream: settings.upstream.trim_end_matches('/').to_string(),
            models: settings.models,
            grants: settings.grants,
            credentials: settings.credentials,
            secret: settings.secret,
            control: settings.control,
            status: settings.status,
            ledger: Arc::new(Ledger::new(settings.budget, settings.usage)),
            budget_file: settings.budget_file,
            agent_config: config,
        }
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

        let mut permit = None;
        if request.method == "POST" {
            let Some(model) = requested_model(&request) else {
                return http::write_error(&mut stream, 400, "deck_invalid_body", "the request has no model");
            };
            permit = match self.admit_model(&request, &model) {
                Ok(permit) => permit,
                Err(denial) => return denial.write(&mut stream),
            };
            self.refresh_budget();
            if let Err(spent) = self.ledger.check() {
                return http::write_error(&mut stream, 429, "deck_budget_exhausted", &spent.message());
            }
        }

        let operation = Operation::new(&stream, &self.agent_config)?;
        let _grant = match self.grants.register(permit, operation.revoker(Revocation::Grant)) {
            Ok(registration) => registration,
            Err(_) => {
                return operation.refuse(|guest| {
                    http::write_denied(guest, None, false, "the authorizing model grant was revoked or expired")
                });
            }
        };
        // Watch the grant before the bounded credential fetch, so a slow
        // store cannot delay revocation or allow a late provider request.
        let credential = match self.credentials.fetch(&self.secret) {
            Ok(credential) => credential,
            Err(refusal) => return operation.refuse(|guest| refusal.write(guest)),
        };
        let _registration = match self
            .credentials
            .register(&credential, operation.revoker(Revocation::Credential(credential.revoked())))
        {
            Ok(registration) => registration,
            Err(refusal) => return operation.refuse(|guest| refusal.write(guest)),
        };
        if !operation.active() {
            return Ok(());
        }
        if request.method == "POST"
            && let Err(spent) = self.ledger.admit()
        {
            return operation.refuse(|guest| http::write_error(guest, 429, "deck_budget_exhausted", &spent.message()));
        }
        let key = credential.value();

        let suffix = request.target.strip_prefix("/v1").unwrap_or_default();
        let url = format!("{}{}", self.upstream, suffix);
        let response = match request.method.as_str() {
            "POST" => {
                let mut builder = operation.agent().post(&url);
                for (name, value) in &request.headers {
                    if policy::retained(name) {
                        builder = builder.header(name.as_str(), value.as_str());
                    }
                }
                builder.header("authorization", format!("Bearer {key}")).send(request.body.as_slice())
            }
            _ => {
                let mut builder =
                    if request.method == "GET" { operation.agent().get(&url) } else { operation.agent().head(&url) };
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
                // The request was admitted and therefore counted even though
                // no provider response arrived.
                self.report_usage();
                return operation.refuse(|guest| {
                    http::write_error(guest, 502, "deck_upstream_failed", "the provider request failed")
                });
            }
        };

        let (parts, body) = response.into_parts();
        let status = parts.status.as_u16();
        let content_type =
            parts.headers.get("content-type").and_then(|value| value.to_str().ok()).unwrap_or("application/json");
        let mut meter = Meter::new(content_type);
        match operation.start(status, content_type) {
            Ok(true) => {}
            result => {
                self.report_usage();
                return result.map(|_| ());
            }
        }
        let mut reader = body.into_reader();
        let mut buffer = [0u8; 16 * 1024];
        let outcome = loop {
            if !operation.active() {
                break Err(io::Error::new(io::ErrorKind::PermissionDenied, "model request was revoked"));
            }
            let read = match reader.read(&mut buffer) {
                Ok(read) => read,
                Err(error) => break Err(error),
            };
            if read == 0 {
                break operation.finish();
            }
            meter.observe(&buffer[..read]);
            if let Err(error) = http::write_chunk(&mut stream, &buffer[..read]) {
                break Err(error);
            }
        };
        let tokens = meter.finish();
        if tokens > 0 {
            self.ledger.charge(tokens);
        }
        // The client already has the response; a slow or missing daemon must
        // not hold this connection open.
        self.report_usage();
        outcome
    }

    /// Apply a budget `deckd` rewrote while the task runs. A missing file keeps
    /// the process-start budget; a malformed one is ignored rather than
    /// widening the limits.
    fn refresh_budget(&self) {
        let Some(path) = &self.budget_file else { return };
        match Budget::load(path) {
            Some(budget) => self.ledger.set_budget(budget),
            None if !path.exists() => {}
            None => eprintln!("deck-gateway: ignoring an unreadable model budget at {}", path.display()),
        }
    }

    fn report_usage(&self) {
        let Some(control) = &self.control else { return };
        let control = control.clone();
        let usage = self.ledger.usage();
        let _ = std::thread::Builder::new().name("gateway-usage".into()).spawn(move || {
            if let Err(error) = control.usage(usage) {
                eprintln!("deck-gateway: recording model usage: {error}");
            }
        });
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
