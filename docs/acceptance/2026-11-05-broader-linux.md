# Broader Linux bypass coverage — 2026-11-05

Follow-up: [merged-fix and adversarial rerun](2026-11-05-follow-up-linux.md)
verifies #62/#63 at `78127b3` and adds resolver-cache, outage-expiry,
malformed GitHub and isolated Secret Service coverage. The results below
are the historical first broader pass.

At Deck `c820a94`, the checked-in live harness ran **61 checks: 59 passed,
2 failed**. Both failures are filed nonblocking bugs, not accepted exceptions.
No product implementation was changed in this pass.

The previous PR #58 rerun remains valid for Secret Service, public TLS,
SSH transport and header deadlines. This run adds concurrent guests, model
upstream observation, raw/DNS transport probes, real Construct feedback,
crash containment, and controlled mirror/profile/redirect coverage.

## Reproduce and review

- Driver: [`scripts/linux-acceptance/README.md`](../../scripts/linux-acceptance/README.md).
- Portable assertions and observed results:
  [`evidence/2026-11-05-broader-linux.json`](evidence/2026-11-05-broader-linux.json).
- Full local logs, manifest with binary hashes and task records:
  `target/acceptance-linux/broader-verified/` and
  `target/acceptance-linux/broader-verified.log`.
- Linux/x86_64, real KVM, libkrun 1.19.7 / fw 5.6.2; existing Debian guest
  with released Construct v0.4.0 and acceptance tools. Host binaries are
  rebuilt from merged PR #58. Guest probes run through real deckd, VMM,
  deck-egress and deck-gateway.
- Model upstream is a scripted host HTTP server; all credentials are random
  synthetic values in an isolated 0600 file store, deleted at the end.
  This run does not read/change the user's OS credential store. Secret
  Service coverage is recorded separately in the preceding acceptance report.

## Newly verified production-path behavior

Two VMs run concurrently with different host project identities.

- **Own-task maps and authority:** a native AF_VSOCK CONNECT on port 1028 is
  denied in A, then allowed by A's task grant. B still gets 403 at that same
  port. B cannot see A's host Unix socket path, and sampled guest CIDs 3/4
  cannot be connected to. This samples isolation; it is not a CID exhaustive
  scan or proof of arbitrary hypervisor code safety.
- **Host loopback:** a host TCP sentinel receives no connection from the
  guest's direct loopback attempt. The guest cannot reach the old TSI route.
- **Address policy:** native-vsock attempts reject loopback, private,
  metadata/link-local, shared, documentation, multicast, unspecified,
  broadcast, IPv6 equivalents, mapped IPv4 and NAT64 forms, at a nondefault
  port, without creating any approval item.
- **Direct packets:** a DNS-format UDP query, a QUIC-shaped UDP payload to
  port 443, IPv6 UDP, and a raw ICMP send fail with network unreachable.
  This tests the route boundary; it is not a complete QUIC client handshake.
- **Model boundary:** disallowed models make zero upstream requests. Admitted
  requests receive the synthetic host key instead of guest Authorization;
  guest Cookie, Proxy-Authorization, x-api-key and an invented header are
  absent upstream. The allowed attribution header survives. Synthetic values
  are absent from the sampled guest environment and textual task artifacts.
- **Live credentials:** revoking the key ends a live VM's already-streaming
  response while the fixture continues holding it. The next VM request is
  refused before contacting the upstream. Rotation restores the next request
  without restarting either VM. This extends the earlier file-store regression
  to a real VM; it does not establish OS-keyring session-loss recovery.
- **Real Construct feedback:** Construct v0.4.0 runs a scripted shell request
  through the real web relay; the resulting pending denial reaches the next
  model request as a tool-result boundary note containing the actual
  destination and approval ID. No real model was used.
- **Crash containment:** killing only A's uniquely identified gateway stops A
  uncleanly, closes its guest stream, and leaves B able to use its gateway.
  The recorded terminal reason is `VMM exited`.

## Controlled mirror, resolver and project-policy behavior

The real guest-to-host path is used. For deterministic upstream responses,
a host-owned wrapper adds the existing `--web-hosts` and
`--web-test-connect` arguments. Fixture names resolve to a pinned public
address, address policy checks it, then the dial alone is redirected to a
local HTTP server. These are integration results, not public Internet
mirror reachability results.

- A mixed public/private DNS answer and a private-only answer are refused
  without a dial or approval item.
- An ungranted mirror makes no upstream request. Once admitted, it receives
  an origin-form path without the guest proxy credential.
- A relative redirect reaches the held mirror. A redirect to another,
  ungranted destination is refused before that destination is contacted.
  After a profile admits that second destination, the redirected request
  reaches it. Unresolvable/private targets and excessive redirect loops fail.
- A task grant carries a future expiry. Whole-profile approval writes one
  project rule per destination and a repeated decision changes nothing.
  Another project's running VM stays denied. Revoking one destination
  removes its access and preserves the sibling destination. This checks live
  policy-file persistence, not survival of every daemon/reboot failure mode
  or waiting 12 hours for the production task expiry.

## Failures filed, left unfixed

1. [#62](https://github.com/strf0x1/deck/issues/62): plain mirror response
   corruption. A 9-byte `mirror-ok` upstream body arrives as the 9 bytes
   `\r\n\r\nmirro`. The response parser returns the header delimiter as body
   remainder, and the relay prepends it while retaining Content-Length.
   Request admission and origin-form forwarding pass; exact response
   content does not.
2. [#63](https://github.com/strf0x1/deck/issues/63): an unsupported-scheme
   redirect is relayed as 302 rather than explicitly refused as ADR §9 case
   14 requires. No second destination was contacted. This is not evidence
   of a network-policy bypass; a following client still needs another
   gateway decision.

The harness intentionally exits nonzero for these two assertions.

## Updated matrix and remaining limits

| ADR §9 case | Coverage added | Remaining gap |
|---|---|---|
| 1 | Host-loopback sentinel confirms no dial | Sampled port only |
| 2 | Broad private IPv4/IPv6 refusal sweep | Not every address in each class |
| 3 | Real DNS-format UDP send has no route; prior public TLS succeeds | No packet capture; route error is the observation |
| 4 | Raw ICMP and UDP/IPv6 fail unreachable | Full QUIC-client behavior remains untested |
| 5 | Real native-vsock task approval; fixture mirror grants at arbitrary ports | No controlled public listener on exact 8443/8444 pair |
| 6 | Guest redirect denied before second contact, then admitted by a new rule | Controlled fixture rather than public redirect service |
| 7 | Earlier proxy/hosts mutations retained | Successful firewall mutation not established |
| 8 | Native vsock address sweep and same-port grant enforcement | Not every gateway request shape |
| 9 | Concurrent task authority/map isolation, guest CIDs, host socket invisibility | No exhaustive CID/port or hypervisor fuzzing |
| 10 | Live model upstream captures admission and header injection | More provider dialects and delegation remain untested |
| 11 | Owning VM fails closed on gateway crash; prior header bounds pass | Fuzzed GitHub/git pkt-lines remain untested |
| 12 | Broad literal sweep and mixed/private host resolution | Live DNS rebinding across resolver cache expiry not driven |
| 13 | Origin-form forwarding, no proxy credential, no ungranted dial | Exact body fails #62 |
| 14 | Private/unresolvable/ungranted targets and loop cap checked | Unsupported scheme fails #63; additional schemes/URI edge cases |
| 15 | Profile persistence/idempotence/project isolation/selective revocation | Active 12-hour expiry and daemon-outage scenario not repeated here |
| 16 | Real Construct shell denial note with actual approval ID | Other boundary tools/late-notification permutations not repeated |

Formatting and clippy pass. The workspace suite passes: **364 passed,
0 failed, 2 ignored**. Python modules compile, and the final live run executes
the checked-in driver. All test tasks, including crash cases, were destroyed;
test daemons exited and synthetic credential files were removed. An initial
cleanup path skipped destroy after a stopped-task error; the harness was
corrected and that earlier task was explicitly destroyed as well.

The full bypass matrix remains open on #62/#63 and the precise limits above.
Locked-keyring timeout/single-flight and actual session recovery also remain
separate credential acceptance work; this run never locks the user's keyring.
