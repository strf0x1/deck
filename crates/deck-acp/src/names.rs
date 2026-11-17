//! The ACP extension names Deck and Construct share. The reverse-DNS form is
//! what ACP documents; the bare form is what a released Construct advertises
//! and parses, kept for one release (deck#36).

/// Construct's extension in `_meta` and the turn-state notification.
pub const CONSTRUCT_META: &str = "com.sprawlcommons/construct";
pub const LEGACY_CONSTRUCT_META: &str = "construct";
pub const TURN_METHOD: &str = "_com.sprawlcommons/construct/turn";
pub const LEGACY_TURN_METHOD: &str = "_construct/turn";

/// Deck's boundary extension: its `_meta` key and the informational
/// notification sent after a late decision (ADR 0004 §6).
pub const DECK_META: &str = "com.sprawlcommons/deck";
pub const LEGACY_DECK_META: &str = "deck";
pub const BOUNDARY_METHOD: &str = "_com.sprawlcommons/deck/boundary";
pub const LEGACY_BOUNDARY_METHOD: &str = "_deck/boundary";

/// The agent capability that says its tools speak Deck's GitHub operation API
/// (contract 1.2.0 §11.1), under [`DECK_META`] only: it has no bare form. The
/// value is the highest API version the agent speaks; this is the one Deck
/// serves.
pub const GITHUB_OPERATIONS: &str = "github_operations";
pub const GITHUB_OPERATIONS_VERSION: u64 = 1;

/// Idempotent prompts (contract 1.3.0 §12), under [`CONSTRUCT_META`] only:
/// the agent capability, the key's field in `session/prompt` `_meta` (ACP v2's
/// name), and `error.data.reason` when a key already names another prompt.
pub const IDEMPOTENT_PROMPTS: &str = "idempotentPrompts";
pub const IDEMPOTENCY_KEY: &str = "idempotencyKey";
pub const IDEMPOTENCY_CONFLICT: &str = "idempotency_key_conflict";
