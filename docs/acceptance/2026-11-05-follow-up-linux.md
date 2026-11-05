# Linux acceptance follow-up — 2026-11-05

Follow-up: [the `337db12` rerun](2026-11-05-final-linux-rerun.md) verifies
both #66/#67 fixes and passes 107/107 live checks. The failure findings below
are retained as historical evidence.

At merged main `78127b3`, **104 of 106 live assertions pass**. The two
failures are newly filed nonblocking bugs #66 and #67, left unfixed. The
full bypass matrix is not accepted yet.

No product code was changed. The checkout was fast-forwarded while
preserving the existing acceptance docs, harness and `deck-live.png`.
Host binaries were rebuilt; the guest remains Construct v0.4.0 on Debian,
libkrun 1.19.7 / libkrunfw 5.6.2, Linux/x86_64 KVM.

## Results and portable evidence

| Driver | Result | Evidence |
|---|---|---|
| Original expanded bypass harness | 61/61 | [Merged-fix rerun](evidence/2026-11-05-mirror-fixes-merged.json) |
| Cache expiry, daemon outage, malformed GitHub | 33/33 | [Adversarial results](evidence/2026-11-05-adversarial-linux.json) |
| Isolated real Secret Service | 9/10 | [Private keyring results](evidence/2026-11-05-private-secret-service.json) |
| GitHub trickled-header bound | 1/2 | [Slow-header results](evidence/2026-11-05-github-slow.json) |

Reproduction instructions and dependencies:
[`scripts/linux-acceptance/README.md`](../../scripts/linux-acceptance/README.md).

## Merged mirror fixes verified

PRs #64/#65 close the two failures reproduced previously. The real guest now
receives the unchanged fixed-length mirror body; relative redirects work;
unsupported HTTPS redirect targets are explicitly refused. The entire
61-check expanded harness passes, including concurrent VM grant isolation,
model header handling, live credential revocation/rotation, real Construct
boundary feedback, gateway-crash containment and project-profile rules.

## Resolver answer change across cache expiry

The test gateway alone runs in a new user/mount namespace. Private files
are bind-mounted over its `/etc/hosts` and `/etc/nsswitch.conf`; the host's
files and other applications are untouched. The libc/NSS answer changes
from a public address to loopback, while the production gateway's cache and
address checks remain unchanged. Its existing host-only dial override sends
admitted connections to the controlled HTTP fixture.

The first public answer is admitted, the cached public answer remains in
use before expiry, and after the actual 30-second cache interval the
changed loopback answer receives 403 without contacting the upstream.
The old grant does not override the new address-class refusal.

This closes the controlled answer-change/cache-expiry gap. It is not a
wire-level DNS-server TTL, DNSSEC or resolver-network outage test.

## Active grant expiry during daemon outage

A host-side fixture shortens a persisted task grant to four seconds. A
native-vsock CONNECT opens under that grant, then the test terminates its
own `deckd`. The gateway closes the open tunnel at expiry, and a retry is
refused while the daemon remains absent. The guest/holder continues running.
Restarting the daemon adopts the same VM and does not restore expired access.

Production expiry-monitor behavior is used; no test bypass widens policy.
This avoids waiting the production grant's full 12 hours. It does not
measure long-duration clock drift or every host power-loss scenario.

## Malformed GitHub/git requests

A disposable VM enables the GitHub gateway for `acceptance/fixture` with no
GitHub token configured. The real guest relay handles malformed JSON,
unsupported operations, absent/oversized/truncated bundle lengths, pkt-line
bytes instead of a bundle, malformed bundle headers and a deterministic
random-byte corpus. All 19 malformed cases are refused within the probe's
eight-second bound, without an approval item or credential-use record. The
model route remains usable afterward.

Deck exposes structured operations plus git bundles, not a guest-controlled
native git receive-pack stream. The corpus therefore tests the boundary
that actually exists. It is sampled adversarial input, not exhaustive fuzzing
of every valid bundle/pack or host git implementation.

An initial harness attempt used the wrong endpoint and half-closed the
client socket. That closes the byte relay, yielding empty responses; the
final run uses `/operations/publish`, a complete Content-Length-framed
request and an open read side. The portable evidence is from the corrected
run, not those initial harness failures.

## Private Secret Service and recovery

The test creates a separate D-Bus session with no service activation, a
temporary HOME/runtime directory, and a real password-protected GNOME Secret
Service collection containing a random synthetic Deck item. It uses
GNOME's internal master-password interface only to create/unlock this
disposable collection without a GUI. The user's bus, store and lock state
are never accessed. The broker has a real local holder and a fixture
VM-format credential-name set; this exercises production store/broker code,
not a new VM model round trip.

Verified:

- Unlocked reads return the synthetic value; the collection's Locked property
  is true after the lock operation; unlocking restores the same daemon's read.
- Pausing the private service makes four concurrent callers join exactly one
  real `secret-tool lookup`. All four return unavailable after about five
  seconds. Resuming the service completes the outstanding read and restores
  access. Rotation is still observed on the next operation.
- Terminating and restarting the private service on the same bus, then
  unlocking its persisted collection, restores reads without restarting
  `deckd`. Service disappearance is unavailable, not missing.

A first instrumentation attempt inspected only the daemon's main thread's
children and counted zero lookups; the corrected driver inspects children
of all daemon threads and verifies one lookup.

**Failure [#66](https://github.com/strf0x1/deck/issues/66):** while the
collection is locked and no prompter is available, `secret-tool lookup`
exits 1 silently, and Deck reports `deck_credential_missing`. The item still
exists and is readable after unlock. ADR §4.2 requires unavailable here;
the current error misleadingly recommends setting a credential. No value
is lost or disclosed.

The paused-service case establishes bounded pending reads and single-flight.
The locked/no-prompter case fails immediately with the wrong classification;
it is not evidence of a successful interactive password-prompt timeout.
Service restart recovery retains the same private bus address; migration
to a new desktop session bus remains untested.

## GitHub slow-header failure

**Failure [#67](https://github.com/strf0x1/deck/issues/67):** an incomplete
GitHub HTTP header, with a byte added every two seconds, remains open with
no response or EOF after **66.09 seconds**. The model route still succeeds.
GitHub sets a 60-second socket inactivity timeout, but reads the header one
byte at a time; successful trickled bytes renew that wait. CONNECT's total
header deadline fix does not cover this parser. MAX_HEAD bounds bytes but
there is no total header duration bound in this path.

The probe closes its own connection and destroys the VM after observing the
failure. This is a resource-bound failure, not a demonstrated policy bypass.

## Verification, cleanup and remaining work

- `cargo fmt --check`: pass.
- `cargo clippy --all-targets -- -D warnings`: pass.
- `cargo test`: **387 passed, 0 failed, 2 ignored**.
- All Python drivers compile; the real live runs above exercise them.
- All disposable VM tasks were destroyed, test daemons terminated, synthetic
  credential files deleted, and private keyring/bus/home removed. No provider
  credential, user keyring or host resolver configuration was changed.
- Detailed local logs: `target/acceptance-linux/round3/`; full run directories
  are `mirror-fixes-merged`, `round3-adversarial-verified`,
  `round3-secret-service-verified`, and `round3-github-slow` under
  `target/acceptance-linux/`.

Next: verify fixes for #66/#67, then decide whether the remaining broader
limits need explicit additional acceptance: a full QUIC client, exact public
8443/8444 service pair, successful guest firewall mutation, an extended
valid/malformed bundle/pack fuzz corpus, private-bus replacement rather than
service restart, and interactive prompt-denial/retry behavior. This pass
does not accept the still-pending timed-grant UI/contract amendment in ADR
0004 and does not establish macOS behavior.
