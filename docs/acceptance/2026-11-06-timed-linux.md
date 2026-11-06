# Linux live acceptance: timed grants — 2026-11-06

Deck **`29dd10f`** and Construct **`28e1b8f`** both pin released contract
**1.1.0** at `8d5a67c`, with matching verified checksums. Final selected
drivers pass **159/161 assertions**. A separate private Secret Service repeat
passes **10/11**. This is **not full ADR acceptance**: three new bugs were
found, and the existing locked-store issue reproduced twice.

Timed/public/UI results below use the local #76 fix. UI results also need
an explicitly labeled guest-path fixture around #78. They must not be
described as an all-green result for unmodified merged main.

| Driver | Result | Evidence |
|---|---|---|
| Existing bypass suite | 61/61 | [Results](evidence/2026-11-06-bypass.json) |
| Resolver cache, outage, malformed GitHub | 33/33 | [Results](evidence/2026-11-06-adversarial.json) |
| Private real Secret Service | 10/11 | [Results](evidence/2026-11-06-private-secret.json) |
| Private Secret Service repeat, counted separately | 10/11 | [Results](evidence/2026-11-06-private-secret-repeat.json) |
| GitHub header deadline | 2/2 | [Results](evidence/2026-11-06-github-header.json) |
| Public TLS and SSH relay | 5/5 | [Results](evidence/2026-11-06-public.json) |
| Production-duration timed grants | 37/38 | [Results](evidence/2026-11-06-timed.json) |
| Real window and VM Construct, with path fixture | 11/11 | [Results](evidence/2026-11-06-timed-ui.json) |

The [manifest](evidence/2026-11-06-manifest.json) records revisions, the exact
local product patch, binary/image/harness hashes, and fixture boundaries.
Detailed logs and fresh images remain under
`~/.local/share/deck-acceptance/linux/live-2026-11-06/`.

## Findings

- **[#76](https://github.com/strf0x1/deck/issues/76), fixed locally:** timed
  approvals persist no task ID. Real gateways require that ID: model retries
  return 403 (`the authorizing model grant is no longer present`) and timed
  web reuse fails. Earlier tests supplied task IDs in handcrafted gateway
  rules and missed the integration. The fix binds Timed as well as Task
  grants to the owning task. A persisted-identity test fails before the fix
  and passes after it. Existing malformed rules remain fail-closed; the fix
  applies to newly created grants. The duration test can then proceed without
  changing any deadlines. [Before-fix observations](evidence/2026-11-06-before-task-fix.json).
- **[#77](https://github.com/strf0x1/deck/issues/77), open:** a timed grant
  approved with `OPENAI_API_KEY` is reused for an otherwise identical host
  control request naming `DIFFERENT_KEY`. The runtime loses the reusable
  grant's credential binding. This violates contract scope isolation. The
  probe uses the trusted host control interface; it does not demonstrate
  guest credential selection or escape. The assertion remains failed.
- **[#78](https://github.com/strf0x1/deck/issues/78), open:** the ordinary
  VM/window ACP session receives the host project path as cwd. The guest
  project is at `/workspace`, so its shell tool fails with `spawn failed:
  No such file or directory`. The successful UI driver uses a disposable
  guest-only symlink from that host spelling to `/workspace`, created by its
  fixture agent wrapper. This is a workaround, not production-path acceptance.
  [Unmodified-path failure](evidence/2026-11-06-before-cwd-workaround.json).
- **[#71](https://github.com/strf0x1/deck/issues/71), reproduced twice:** an
  actually locked private collection's item is reported as
  `deck_credential_missing` instead of unavailable (first response: about
  21 ms). Unlock, genuine absence, single-flight, timeout, rotation and
  service restart checks pass. The user's keyring is never accessed.

## Successful live behavior

The host creates a deadline exactly 300000 ms after its timed decision.
Changed model prompts and mirror paths reuse authority. Other tasks, models,
destinations, ports, gateways and operations do not. HTTP status and events
carry the original scope and absolute deadline. Traffic, reconnect and
daemon restart preserve that value.

Model and opaque web streams remain open near the **real five-minute
deadline**. The driver then terminates its daemon. Both operations close at
expiry; new admissions remain denied during the outage and after restart.
The unrelated configured-model stream stays open. No fixture shortens or
rewrites these deadlines. Separate early revocation closes its active model
stream and preserves unrelated traffic. Task stop removes timed authority.
The live poll observes closure shortly after expiry; it is not a
millisecond-precision scheduler measurement.

With the path fixture, the actual decision card's **2** key creates the grant.
Access lists its exact deadline and displays `5 minutes · expires in 5m`.
Screenshots were inspected: [card](evidence/2026-11-06-timed-card.png),
[Access](evidence/2026-11-06-timed-access.png). VM Construct receives one late
ACP decision with that same deadline. `boundary_status` and `boundary_wait`
retain it in model text and journal structured data; observation does not
renew authority. Closing the window leaves the VM running. The
[feedback snapshot](evidence/2026-11-06-construct-feedback.json) contains
scripted tool results and public response headers only.

Public transport uses the production resolver/dial: certificate-verified
TLS to `example.com` returns HTTP 200 through the guest proxy, and SSH
identification banners are exchanged with `ssh.github.com:443` through the
loopback TCP relay and CONNECT. Both targets are denied before approval;
a separate undeclared TLS destination remains denied. SSH is an
unauthenticated transport probe, not a login, push or publication test.

## Verification and limits

Formatting, clippy and workspace tests pass: **397 passed, 0 failed,
2 ignored**. Python drivers compile. Disposable tasks, VMs, windows, daemons
and synthetic credential files were cleaned up. Private bus/home/collection
files are removed; durable images and evidence remain.

Existing fixture boundaries remain: timed opaque web probes use public-class
fixture addresses and a local dial override; resolver changes use private
NSS mounts rather than a wire DNS server. Public TLS/SSH use no override.
This adds no full QUIC flow, arbitrary public 8443/8444 service pair,
authenticated SSH/GitHub publication, exhaustive bundle fuzzing, replacement
session-bus migration, interactive Linux prompt denial/retry, or macOS proof.
It does not separately drive the timed HTTP tool-result-note branch;
status/wait and late ACP feedback are driven live. No acceptance assertion
was changed to treat a product bug as success.
