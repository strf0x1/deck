//! The names that cross the guest boundary (ADR 0004): the loopback TCP ports
//! the guest relays own, one vsock port per host gateway kind, and the
//! placeholder credential the guest receives.
//!
//! The design choice: the numbers live here, not in the guest image alone.
//! The runtime writes them into the guest environment, the VMM maps the same
//! ports, and `deck-egress` forwards to the one it is told; a change cannot
//! move one half without the other.

/// The guest relay's loopback listener. Construct's `CONSTRUCT_BASE_URL`
/// points here; it is not an external route.
pub const RELAY_PORT: u16 = 8080;
/// The boundary status relay's guest loopback listener, which Construct
/// reads as `DECK_BOUNDARY_URL`.
pub const BOUNDARY_PORT: u16 = 8081;
/// The web and package gateway's guest loopback listener. The task's proxy
/// environment points here, so a package manager or git reaches the mirror
/// policy without knowing about vsock.
pub const WEB_PORT: u16 = 8082;
/// The plain-HTTP mirror listener, beside the proxy one: a client that reads
/// `http_proxy` gets both forms through the same relay, and one that was told
/// only about plain HTTP still reaches a declared mirror.
pub const MIRROR_PORT: u16 = 8083;
/// The model gateway's guest vsock port.
pub const MODEL_PORT: u32 = 1026;
/// The web and package gateway's guest vsock port. It has its own port rather
/// than the extension port because a client's proxy setting cannot send the
/// framed kind header.
pub const WEB_VSOCK_PORT: u32 = 1028;
/// The reserved extension port. The boundary status kind is served here; a
/// later gateway kind is added without restarting running tasks.
pub const EXTENSION_PORT: u32 = 1027;
/// The framed kind the guest relay names on the extension port.
pub const BOUNDARY_KIND: &str = "boundary";
/// The framed kind the web gateway answers on the extension port, beside the
/// dedicated port.
pub const WEB_KIND: &str = "web";
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

/// The marker Construct reads to treat every guest credential as host-owned
/// (ADR 0004 §4). Any non-empty value other than `0`/`false` means managed;
/// the runtime uses `1`. Without it a guest would take Deck's placeholder for
/// a real key and keep the host-facing secrets UI paths.
pub const MANAGED_ENV: &str = "DECK_MANAGED";
pub const MANAGED_VALUE: &str = "1";

/// Construct's own default endpoint and model. The runtime uses them when the
/// host names neither, so the task is configured the way Construct would be
/// without Deck.
pub const DEFAULT_UPSTREAM: &str = "https://openrouter.ai/api/v1";
pub const DEFAULT_MODEL: &str = "openrouter/free";
