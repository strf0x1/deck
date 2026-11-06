# Linux acceptance: merged #66/#67 rerun — 2026-11-05

At merged main **`337db12`**, all **107 live assertions pass**. The fixes for
#66 and #67 are verified, with one additional genuine-absence regression
check. No new product bug was found in this pass. This accepts the current
automated Linux test suite, not every possible bypass input or the untested
limits listed below.

| Live driver | Result | Portable evidence |
|---|---|---|
| Expanded guest bypass suite | 61/61 | [Bypass results](evidence/2026-11-05-final-bypass.json) |
| Resolver cache, outage expiry, malformed GitHub | 33/33 | [Adversarial results](evidence/2026-11-05-final-adversarial.json) |
| Private real Secret Service | 11/11 | [Private-store results](evidence/2026-11-05-final-private-secret.json) |
| GitHub total header deadline | 2/2 | [Header results](evidence/2026-11-05-final-github-header.json) |

[Revision, binary and harness hashes](evidence/2026-11-05-final-manifest.json)
identify this pass. Driver instructions are in
[`scripts/linux-acceptance/README.md`](../../scripts/linux-acceptance/README.md).

## Fix verification

**#66:** the synthetic item is read successfully in an unlocked private
GNOME Secret Service collection. After the collection's Locked property is
confirmed true, the same daemon broker returns `deck_credential_unavailable`.
Unlock restores access. An additional credential name in the task set has
no stored item and still returns `deck_credential_missing`, distinguishing
lock from actual absence. Four blocked concurrent reads share one lookup,
time out as unavailable at approximately five seconds, recover when the
private service resumes, observe rotation, and recover after service restart.

**#67:** an incomplete GitHub header is trickled with one extra byte every
two seconds. It receives `HTTP/1.1 408 Request Timeout` after **5.113 s**,
without renewing the header budget. The assertion now requires 408 within
eight seconds including relay/scheduling overhead, rather than merely
closing within the former sixty-second read window. The model route remains
usable afterward.

The surrounding VM suite confirms that the earlier mirror fixes remain
correct, a private answer is denied after the real resolver cache expires,
a live tunnel expires while the control daemon is absent, malformed GitHub
inputs are bounded, and previously verified isolation, model credentials,
Construct feedback, crash containment and project rules still pass.

## Rebuilt fixtures and durable evidence

The previous acceptance assets were under Cargo's managed target cache;
that directory had been collected and its target symlink was dangling.
The source-controlled result snapshots survived. This pass recreated the
fixtures outside that cache:

- Root: `~/.local/share/deck-acceptance/linux/`.
- Guest images: `vm/base.img` and `vm/journal.img`.
- Host binary copies: `bin/deckd`, `bin/deck-gateway`, `bin/deck-egress`.
- Pinned libkrun 1.19.7: `lib/libkrun.so.1.19.7`; libkrunfw 5.6.2 under `fw/`.
- Detailed logs and run directories: `round4/`.

Construct v0.4.0's released Linux binary was downloaded and checked against
its published SHA-256, then embedded in the rebuilt Debian guest. The new
`build_image.py` derives Deck's existing image recipe and adds the acceptance
tools, making the previously ad hoc image preparation reproducible. It was
used successfully for this run. Docker remains a build tool only.

The private Secret Service uses a separate D-Bus session, temporary HOME,
credential collection and runtime directory. The user's keyring and lock
state are untouched. The resolver fixture mounts files only inside its
private user/mount namespace; host resolver configuration is untouched.

## Checks and cleanup

- Formatting and clippy pass.
- Workspace tests: **389 passed, 0 failed, 2 ignored**.
- Python drivers compile and the image builder was exercised.
- All seven disposable VM tasks were destroyed, test daemons exited and
  synthetic credential files were removed. The private store/bus/home was
  removed. Durable image templates and evidence remain for later runs.
- Product fixes came from the merged PRs. This pass changed only acceptance
  tooling and documentation; product fixes are in the preceding merged PRs.

## Remaining limits

The fixture-backed results retain the boundaries described in the
[previous follow-up report](2026-11-05-follow-up-linux.md): resolver answer
changes are controlled through libc/NSS rather than a wire DNS server; grant
expiry is shortened in a host fixture; malformed bundle traffic is a sampled
corpus. A complete QUIC-client flow, an exact public 8443/8444 service pair,
successful guest firewall mutation, broader bundle/pack fuzzing, replacement
of the session-bus address, and interactive prompt denial/retry behavior
remain untested. This run does not accept the pending timed-grant UI/contract
amendment or establish macOS behavior. No open reproduced failure remains
in the current 107-check live suite.
