//! The key a prompt is sent with (contract 1.3.0 §12.3): minted when the user
//! submits, kept with the pending prompt, and sent again unchanged on every
//! attempt, so an agent that already took the prompt never runs it twice.
//!
//! The design choice: no dependency for a UUID. `RandomState` is seeded from
//! the operating system's randomness, and four hashes of a process-wide
//! counter and the clock give 128 unpredictable bits, formatted as a version-4
//! UUID so the key reads like the one RFC 9562 suggests.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT: AtomicU64 = AtomicU64::new(0);

/// A fresh idempotency key: a random version-4 UUID string.
pub fn new_key() -> String {
    let seed = RandomState::new();
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let word = |salt: u64| {
        let mut h = seed.build_hasher();
        h.write_u64(salt);
        h.write_u64(n);
        h.write_u128(nanos);
        h.finish()
    };
    let (hi, lo) = (word(1) ^ word(3).rotate_left(17), word(2) ^ word(4).rotate_left(29));
    let hi = (hi & 0xffff_ffff_ffff_0fff) | 0x4000; // version 4
    let lo = (lo & 0x3fff_ffff_ffff_ffff) | 0x8000_0000_0000_0000; // RFC 9562 variant
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        hi >> 32,
        (hi >> 16) & 0xffff,
        hi & 0xffff,
        lo >> 48,
        lo & 0xffff_ffff_ffff
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_distinct_version_4_uuids_within_the_contracts_length() {
        let keys: std::collections::HashSet<String> = (0..1000).map(|_| new_key()).collect();
        assert_eq!(keys.len(), 1000);
        for k in keys.iter().take(20) {
            assert_eq!(k.len(), 36, "{k}");
            assert_eq!(k.get(14..15), Some("4"), "{k}");
            assert!(matches!(k.get(19..20), Some("8" | "9" | "a" | "b")), "{k}");
        }
    }
}
