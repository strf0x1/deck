//! The names that cross the guest boundary (ADR 0004): one loopback TCP port
//! the guest relay owns, one vsock port per host gateway kind, and the
//! placeholder credential the guest receives.
//!
//! The design choice: the numbers live here, not in the guest image alone.
//! The runtime writes them into the guest environment, the VMM maps the same
//! ports, and `deck-egress` forwards to the one it is told; a change cannot
//! move one half without the other.

/// The guest relay's loopback listener. Construct's `CONSTRUCT_BASE_URL`
/// points here; it is not an external route.
pub const RELAY_PORT: u16 = 8080;
/// The model gateway's guest vsock port.
pub const MODEL_PORT: u32 = 1026;
/// The reserved extension port. v1 answers every kind as unsupported; a later
/// gateway kind is served here without restarting running tasks.
pub const EXTENSION_PORT: u32 = 1027;
/// ACP keeps the port the guest already opens.
pub const ACP_PORT: u32 = 1024;

/// The guest's value for the provider secret. Non-empty so Construct's
/// environment-first secretspec resolves it and never touches a guest keyring;
/// the host gateway replaces it before any provider request.
pub const PLACEHOLDER_KEY: &str = "deck-placeholder";
/// The secret name Construct reads and the host gateway injects.
pub const SECRET: &str = "OPENAI_API_KEY";

/// The agent's own sandbox mode inside the VM. The guest is the isolation
/// boundary, and Construct's filesystem/exec sandbox is a host-process
/// mechanism that the guest kernel cannot provide (no Landlock). The runtime
/// sets this for the guest process; the local backend leaves the user's
/// Construct configuration alone, because there the process sandbox is the
/// boundary around the real workspace and host credentials.
pub const GUEST_SANDBOX: &str = "off";

/// Construct's own default endpoint and model. The runtime uses them when the
/// host names neither, so the task is configured the way Construct would be
/// without Deck.
pub const DEFAULT_UPSTREAM: &str = "https://openrouter.ai/api/v1";
pub const DEFAULT_MODEL: &str = "openrouter/free";
