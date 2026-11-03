//! deck as an ACP client: it talks the Agent Client Protocol over any line
//! stream — a local agent subprocess's stdio, or a socket a VM-hosted agent
//! listens on. `rpc` is the line framing, `conn` the protocol state machine,
//! `client` the connection and its reader thread. Launching and stopping
//! agents is `deck_runtime`'s job, not this crate's.
//!
//! The design choice: everything that decides what to send lives in the pure
//! [`conn::Conn`], fed one parsed line at a time, so the protocol is tested
//! without a process, a UI or a clock. The UI only ever sees [`Inbound`].
//! A connection and the process behind it are separate: closing a [`Client`]
//! ends the stream and stops nothing.
//! This crate has no UI dependency, and must never gain one.

pub mod client;
pub mod conn;
pub mod inbound;
pub mod names;
pub mod rpc;
#[cfg(unix)]
pub mod shutdown;

pub use client::{Client, Writer};
pub use inbound::{Inbound, PermissionOption, ToolStatus, TurnState};

/// The ACP protocol version deck speaks.
pub const VERSION: u64 = 1;
