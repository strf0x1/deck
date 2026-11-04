//! The web and package gateway (ADR 0004 §3): CONNECT tunnels and plain HTTP
//! to declared mirrors, resolved and dialled on the host.
//!
//! The design choice: the guest names a destination; the host resolves it,
//! refuses host and private address space on any port, and asks the daemon for
//! a scope it does not already hold. Everything the guest can influence — the
//! name, the port, a redirect, its own `/etc/hosts` — is an input to a check,
//! never a route. One request is one connection, so a grant is evaluated where
//! it applies and a revoked grant stops the next attempt without a restart.
//!
//! [`profiles`] is the built-in table and [`policy`] the checks that are pure.
//! [`resolve`] is bounded host resolution. [`connect`] is the opaquely relayed
//! tunnel and [`mirror`] the parsed plain-HTTP proxy. [`control`] asks the
//! daemon, [`status`] records what the agent needs to read.

pub mod connect;
pub mod control;
pub mod mirror;
pub mod policy;
pub mod profiles;
pub mod resolve;
pub mod status;

pub use control::{Answer, Control};
pub use status::Events;

/// The extension-port kind a connection names to reach this gateway.
pub const KIND: &str = "web";
/// The gateway name approvals and grants carry for this kind.
pub const GATEWAY: &str = "web";
/// The default CONNECT operation; the port is part of the destination.
pub const CONNECT: &str = "connect";
/// The operation that asks for a whole profile, decided once at launch
/// instead of once per destination.
pub const PROFILE: &str = "profile";
/// The plain-HTTP operation, used for a mirror destination the task does not
/// hold yet and for a redirect that leaves an allowed one.
pub const FETCH: &str = "fetch";

/// The shared refusal of a destination about to be dialled. `None` means the
/// address may be dialled.
pub fn refuse(address: std::net::IpAddr) -> Option<String> {
    policy::blocked(address).map(|class| format!("the destination resolves to {}", class.reason()))
}
