//! deck as an ACP client: it launches an agent (construct by default) as a
//! subprocess and talks the Agent Client Protocol over its stdin and stdout.
//! `rpc` is the line framing, `conn` the protocol state machine, `process`
//! the subprocess and its reader thread.
//!
//! The design choice: everything that decides what to send lives in the pure
//! [`conn::Conn`], fed one parsed line at a time, so the protocol is tested
//! without a process, a UI or a clock. The UI only ever sees [`Inbound`].

pub mod conn;
pub mod process;
pub mod rpc;

pub use conn::{Inbound, PermissionOption, ToolStatus};
pub use process::Client;

/// The ACP protocol version deck speaks.
pub const VERSION: u64 = 1;
