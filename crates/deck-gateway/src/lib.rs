//! deck's host-side egress gateway for a VM task (ADR 0004).
//!
//! The design choice: this crate only ever listens on host Unix sockets that
//! libkrun maps from guest vsock ports. The guest supplies bytes and names;
//! the listener set, the model allowlist and the credential injection are
//! host state. Nothing here opens an IP listener, and nothing here writes a
//! secret to a file, a log or the guest.
//!
//! [`policy`] is pure and unit-tested without sockets. [`http`] is the
//! bounded HTTP/1.1 carrier. [`usage`] reads provider token counts.
//! [`model`] is the OpenAI-compatible reverse proxy. [`status`] is the
//! boundary status kind on the extension port. [`web`] is the CONNECT and
//! plain-mirror proxy, served on its own socket and on the extension port.
//! [`server`] binds the sockets and dispatches.

pub mod control;
pub mod http;
pub mod model;
pub mod policy;
pub mod server;
pub mod status;
pub mod usage;
pub mod web;
