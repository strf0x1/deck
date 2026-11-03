# 0006 — Extension names and payloads are a versioned shared contract

Status: accepted, 2026-11-03. Follow-up to 0005 and deck#36.

## Context

ADR 0005 renamed Deck's and Construct's ACP extensions and promised one
release of dual names. Each side then kept its own prose (Deck's 0002/0004/0005,
Construct's 0038) and its own tests for the same wire, and a change lands in
one repository first. Nothing failed when the two descriptions drifted; a
rename is exactly the change that needs one owner.

## Decision

- The contract lives in `strf0x1/sprawlcommons-acp` (private, semver, tagged
  `v1.0.0`): normative spec, JSON Schemas, the negotiation matrix and the
  change protocol. It contains no implementation.
- Deck pins a release in `CONTRACT.lock` and vendors the machine artifacts
  (`VERSION`, `matrix.json`, `schema/`) under
  `crates/deck-acp/tests/fixtures/acp-contract/<version>/`. The prose stays in
  the contract repository and is cited by version.
- `crates/deck-acp/tests/contract.rs` asserts `names.rs` and the forms Deck
  accepts against the vendored matrix.
- `scripts/acp-contract.sh verify` hashes the tree against the lock and runs
  in CI; `update <version>` refreshes the pin with `gh`.
- A wire change starts in the contract repo (semver; matrix and schemas in the
  same PR), then lands in Deck and Construct pinned in the same wave. Removal
  of the bare names is contract 2.0.0, per 0005.

## Consequences

- `names.rs` remains the one place the code declares names, and the contract
  test makes that declaration agree with the matrix both sides test.
- A stale vendored copy fails CI as a hash mismatch, not as a protocol bug in
  production.
- Deck's private tree holds the normative prose only by reference; the
  contract repository owns the compatibility policy.

## Evidence

- `strf0x1/sprawlcommons-acp` v1.0.0, commit
  `8c24a07c28aaabd539582d749a5ee9c583ca107d`, extracted from 0005 without a
  behavior change.
- `CONTRACT.lock`, `crates/deck-acp/tests/contract.rs`,
  `scripts/acp-contract.sh verify`.
- deck#36 and construct#171 (compatibility matrix); PR #40 pins the first
  release.
