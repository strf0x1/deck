# Credential cancellation and explicit host retry — 2026-11-05

This ADR 0004 §4.2/§11.10 slice retains a completed OS cancellation or
credential-access refusal for the current daemon lifetime. Automatic fetch,
listing and revoke calls return the existing failure instead of starting
another OS read. A host command clears that refusal by credential name:

```sh
deckd credential retry OPENAI_API_KEY
```

Retry itself fetches no value and grants no authority. An outstanding read
is never cleared, including after its caller has timed out. Concurrent retry
calls clear a completed refusal once. Revoked fingerprints and task/project
grants are unchanged. Successful reads still go to the selected store, so
rotation remains immediate. Only the refusal is retained, never a credential
value. Restarting the daemon clears this in-memory pause.

macOS maps `errSecUserCanceled` and `errSecAuthFailed` to this paused state.
Other failures, including interaction-not-allowed, remain transient. Linux
continues to use its existing unavailable/missing classification; it does
not infer explicit refusal from localized secret-tool diagnostics. The
private host control operation adds no ACP field, scope or error code:
gateways retain `deck_credential_unavailable`, with an actionable host retry
instruction. The Access-panel retry button and five-minute grant contract/UI
remain outside this slice.

## Validation

- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`: pass.
- `cargo test`: 395 passed, 0 failed, 2 ignored on Linux.
- Added store/broker regressions cover automatic-read suppression, concurrent
  retry, pending reads after timeout, late cancellation, repeated refusal,
  transient recovery, unchanged credential-set checks and no successful-value
  cache. macOS has an OS-status mapping test, to be exercised by macOS CI.
- A real daemon/CLI test verifies name validation, no credential-store fallback,
  unchanged grants and a revoked credential remaining refused. The updated
  targeted test passes after adding raw control-message validation.
- The real-socket daemon-client test verifies explicit retry stays responsive
  while a credential read is blocked on another connection.
- Private real Secret Service acceptance: one initial run passed 10/11, with
  locked-item classification reported as missing. The immediate repeat and
  two further branch runs passed 11/11, as did two accepted-baseline runs.
  The Linux lookup implementation was not changed by this slice. This
  intermittent observation is tracked as
  [#71](https://github.com/strf0x1/deck/issues/71), not declared resolved or
  attributed to a confirmed cause.

Local evidence is under `~/.local/share/deck-acceptance/linux/denial-retry/`,
plus `denial-retry-tests.log` and `denial-retry-clippy.log` in its parent.
Synthetic keyrings and test tasks are disposable; no user keyring or provider
credential was changed.

## macOS acceptance still required

With the supported signing identity, deny/cancel a real Keychain access
request, then repeat CLI listings, gateway requests and panel opens. Confirm
no new dialog until `credential retry NAME`; confirm that retry permits
another dialog and a successful read. Also check a late denial after the
first five-second caller timeout, wrong authentication, and a locked Keychain
that later becomes available. CI can validate status mapping and store logic;
it cannot establish this live dialog behavior. This implementation is a
reviewable host/CLI slice, not full credential-prompt or timed-grant acceptance.
