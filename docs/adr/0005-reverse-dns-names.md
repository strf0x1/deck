# 0005 — Extension names use reverse DNS

Status: accepted, 2026-11-03. Implements Deck's side of deck#36, paired with
construct#171. Supersedes the extension names in ADR 0002 and ADR 0004 §6/§10.

## Context

ACP reserves `_`-prefixed methods for extensions and, in v2, unprefixed
`_meta` keys. The documented namespace is reverse DNS: methods named
`_com.example/<name>` and `_meta` keys named `com.example/<name>`.

Deck and Construct used bare names: `_construct/turn`, `_construct/event`,
`_deck/boundary`, and `_meta` keys `construct` and `deck`. A released
Construct still advertises and parses those names, and the two halves ship
separately, so the rename needs a transition rather than a cutover.

## Decision

The namespace is `com.sprawlcommons`. `crates/deck-acp/src/names.rs` is the
one place the names are declared.

| Use | Name |
|---|---|
| construct `_meta` key | `com.sprawlcommons/construct` |
| turn notification | `_com.sprawlcommons/construct/turn` |
| deck `_meta` key | `com.sprawlcommons/deck` |
| boundary notification | `_com.sprawlcommons/deck/boundary` |

**Deck prefers the new capability and falls back to the old.** `initialize`
reads `agentCapabilities._meta["com.sprawlcommons/deck"].boundary` first and
`_meta.deck.boundary` second; the method it sends matches the key the agent
advertised. Live attach reads `_meta["com.sprawlcommons/construct"].liveAttach`
first and `_meta.construct.liveAttach` second. The load response's turn
snapshot is read under the new construct key, then the bare one.

**Deck asks for the new turn method.** Its `clientCapabilities._meta` names
`com.sprawlcommons/construct.turn`; a new Construct sends that method, and
Deck parses both it and `_construct/turn` so an old Construct still drives
status. A Construct that does not see the capability keeps the bare method
for released Decks.

**The fake agent advertises and accepts both.** It advertises both
`liveAttach` keys and both `boundary` keys (when its boundary endpoint
exists), accepts both boundary notification methods, and returns the turn
snapshot under both keys. Its turn notification follows the client's
capability exactly as Construct does, so the tests exercise negotiation and
the fallback.

The bare names stay for one release; then the fallbacks and `LEGACY_*`
constants are removed together with Construct's.

## Consequences

- New + new uses only the new names. New Construct + old Deck: the old keys,
  old methods and old turn method keep attach and late decisions working.
  Old Construct + new Deck: the fallback keeps working, and the bare turn
  method is still parsed.
- One module holds the names, so the transition cannot drift between the
  state machine and the fake agent.
- The transition costs a few fallbacks and two extra keys in tests; payloads
  do not change.

## Evidence

- deck#36 and construct#171 (acceptance matrix).
- `crates/deck-acp/src/conn.rs` unit tests: new-only capability uses the new
  boundary method; both capabilities prefer the new one; both turn methods
  and both snapshot keys are accepted; the old-only fallback is unchanged.
- `crates/deck-acp/tests/acp.rs` and `crates/deck/tests/{reattach,view}.rs`
  against the fake agent, which now negotiates like Construct.
