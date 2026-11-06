# Live Linux bypass acceptance

Run `bypass.py` from the repository root with built host binaries, an existing
Linux/x86_64 guest image, KVM access, and libkrun 1.19.7 / libkrunfw 5.6.2.
The guest needs Construct v0.4.0, deck-egress, Python 3, curl, and the normal
`deck-guest` entry from `scripts/vm-image.sh`. `build_image.py` derives that
recipe, uses a supplied checksum-verified Linux release binary, and adds
curl, Python, OpenSSH client, iproute2 and iptables:

```sh
python3 scripts/linux-acceptance/build_image.py \
  --construct-binary /path/to/verified/linux/construct \
  --output "$HOME/.local/share/deck-acceptance/linux/vm"
```

Keep images, libkrun/libkrunfw, evidence and copied test executables outside
Cargo's managed `target` cache. The pinned libkrun build recipe is in
`docs/vm-spike.md`; copy its built library into the `lib/` directory above.
A build-cache collection removed the original fixtures during acceptance;
the next run recreated them in the durable directory shown here.

```sh
export DECK_ACCEPTANCE_ROOT=$HOME/.local/share/deck-acceptance/linux
cargo build -p deck-runtime -p deck-gateway -p deck-egress
mkdir -p "$DECK_ACCEPTANCE_ROOT/bin"
cp target/debug/deckd target/debug/deck-gateway target/debug/deck-egress "$DECK_ACCEPTANCE_ROOT/bin/"
export DECK_VM_IMAGE=$DECK_ACCEPTANCE_ROOT/vm/base.img
export DECK_VM_JOURNAL=$DECK_ACCEPTANCE_ROOT/vm/journal.img
export DECK_VM_KRUN=$DECK_ACCEPTANCE_ROOT/lib/libkrun.so.1.19.7
export DECK_VM_EXECUTABLE=$DECK_ACCEPTANCE_ROOT/bin/deckd
export DECK_GATEWAY_EXECUTABLE=$DECK_ACCEPTANCE_ROOT/bin/deck-gateway
export LD_LIBRARY_PATH=$DECK_ACCEPTANCE_ROOT/fw/lib/x86_64-linux-gnu
python3 scripts/linux-acceptance/bypass.py --output "$DECK_ACCEPTANCE_ROOT/new-run"
```

Choose a fresh output directory. `--section production` runs only the
production gateway configuration; `--section mirrors` runs only the
controlled mirror/DNS fixtures. The full run uses four disposable VMs and
makes a few unauthenticated public connections to GitHub's SSH listener.
It uses a synthetic 0600 file credential store, not the user's OS keyring.
The separate Secret Service acceptance results are in the report.

The production phase tests simultaneous VM grant isolation, native vsock,
private address classes, host-loopback sentinel isolation, UDP and raw
socket routing, model credential stripping/injection, live stream
revocation/rotation, real Construct tool-result feedback, and gateway-crash
containment. No model tokens or authenticated publication operations are
used: the model endpoint is a local scripted HTTP server.

The mirror phase wraps the real gateway with its existing `--web-hosts`
and `--web-test-connect` test options. Names are pinned to public addresses,
policy checks those addresses, and only the dial is redirected to a local
HTTP fixture. This exercises the guest relay, HTTP parser, policy, daemon,
redirect chain and response bytes. It is controlled integration evidence,
not a public-host transport measurement. The earlier public TLS/SSH probes
use the production resolver and dial path without this override.

`results.json` records each assertion; `manifest.json` records the revision
and hashes of host binaries/library. Raw model headers and synthetic secret
values stay in memory. The temporary credential file is removed on exit.
VMs are stopped/destroyed and the daemon is terminated in cleanup. Avoid
interrupting the process with SIGKILL, which cannot run that cleanup.

Exit status is 1 if an assertion fails, including known issues. The
2026-11-05 rerun at `78127b3` passes all 61 original assertions and verifies
the merged fixes for #62/#63. The `337db12` rerun passes 107/107 assertions
across all four drivers, verifying #66/#67 and genuine-absence behavior.
Failures are never converted into expected successes; see the latest
acceptance report for coverage limits.

The original `bypass.py` does not build the guest image, test a locked
Secret Service, perform an authenticated SSH operation, or claim a complete
fuzzing or QUIC client test. The additional drivers below cover the named
resolver, expiry and isolated Secret Service scenarios. See the acceptance
report for exact coverage and remaining gaps.

## Resolver expiry, daemon outage and malformed GitHub requests

With the same VM environment, run:

```sh
python3 scripts/linux-acceptance/adversarial.py --output target/acceptance-linux/adversarial-new-run
python3 scripts/linux-acceptance/github_slow.py --output target/acceptance-linux/github-slow-new-run
```

`adversarial.py` adds 33 assertions. Its resolver phase wraps only the test
VM's gateway in a fresh user/mount namespace and binds private fixture files
over `/etc/hosts` and `/etc/nsswitch.conf` inside that namespace. It requires
unprivileged user/mount namespaces (`unshare -Urm`); it never changes host
resolver files. Real libc resolution and the gateway's unchanged 30-second
cache are exercised. The answer changes from public to loopback, and the
host dial remains redirected to the controlled HTTP fixture. This is a
controlled NSS answer-change test, not a DNS-server/TTL or DNSSEC test.

The expiry phase shortens one persisted host task grant to four seconds,
opens a tunnel, terminates its test daemon, observes tunnel closure and
retry denial, then restarts the daemon and confirms adoption without renewed
access. It tests actual expiry-monitor behavior without a 12-hour wait; the
host fixture, not the guest, writes the shortened deadline.

The GitHub phase enables a repository on the VM and sends malformed JSON,
operation names, length prefixes, truncated/oversized bundles, pkt-line
bytes and a deterministic random-byte corpus over the guest GitHub relay.
No GitHub token is configured. It asserts bounded refusals, no approval or
credential-use event, and an unaffected model route. These are sampled
parser tests, not exhaustive fuzzing or authenticated publication tests.

`github_slow.py` additionally tests an incomplete header while sending a
byte every two seconds for 66 seconds. It checks whether the nominal
60-second GitHub read budget is a total deadline. This run takes slightly
over a minute on the old failing implementation, and creates its own
disposable VM. At `337db12` the assertion is tightened to require HTTP 408
within the new five-second total budget (eight seconds including relay
scheduling). A failing assertion still
cleans up the connection and task and exits nonzero.

## Private Secret Service failure and recovery

```sh
DECK_VM_EXECUTABLE=$PWD/target/debug/deckd \
  python3 scripts/linux-acceptance/secret_service.py \
    --output target/acceptance-linux/private-secret-new-run
```

This test does not need a VM. It needs `dbus-run-session`,
`gnome-keyring-daemon`, `secret-tool`, and a Python with `dbus-python`
(default `/usr/bin/python3.14`, override with `--dbus-python`). It creates a
separate session bus with no service activation, a temporary HOME and
runtime directory, and a synthetic password-protected persistent collection.
It uses GNOME's internal master-password methods only to create/unlock this
disposable collection without a GUI. The user's bus, keyring and lock state
are never accessed. Passwords and synthetic values are not written to logs.

The daemon has a real local holder and a fixture VM-format credential-name
set, so the production broker/store checks are exercised. Assertions
cover genuine absence, unlocked reads, actual collection lock, classification, unlock,
five-second bounds, four concurrent callers sharing one lookup, rotation,
service disappearance and service restart without restarting deckd. The
blocked-read case pauses only the private keyring process. Recovery keeps
the private bus alive; this does not prove migration to a replacement bus
address or interactive desktop password prompts. Private bus/home/keyring
files are deleted after cleanup.

## Five-minute grants, public transport and the real window

Use a fresh guest image containing the **same merged Construct revision**
as the receiver under test, rather than an older installed release. The
2026-11-06 pass builds Construct at `28e1b8f` in `rust:1.94-bookworm` with
`pkg-config` and `libdbus-1-dev`, using `cargo build --locked -p construct`.
Its binary is passed to `build_image.py` above. The manifest hashes that
binary and both resulting images. Host binaries are built from merged Deck
`29dd10f` plus the explicitly recorded #76 fix.

With the VM environment above and fresh output directories:

```sh
python3 scripts/linux-acceptance/timed.py --output "$DECK_ACCEPTANCE_ROOT/timed-new"
python3 scripts/linux-acceptance/public_transports.py --output "$DECK_ACCEPTANCE_ROOT/public-new"
export DECK_ACCEPTANCE_UI_EXECUTABLE=$DECK_ACCEPTANCE_ROOT/bin/deck
python3 scripts/linux-acceptance/timed_ui.py --output "$DECK_ACCEPTANCE_ROOT/timed-ui-new"
```

`timed.py` uses production 300000 ms deadlines and takes slightly over five
minutes. It checks real model/web reuse, scope isolation, metadata, restart,
early revocation, expiry with the daemon absent, unrelated stream survival
and task-stop cleanup. Its web address/dial fixture is explicitly controlled;
it does not claim public transport. A different-credential probe uses the
trusted daemon interface. Known #77 remains a failed assertion, so this
driver currently exits 1 even when the expiry probes pass. No deadlines are
shortened, rewritten or renewed by the harness.

`public_transports.py` uses the production resolver/dial and guest loopback
TCP relay: certificate-verified TLS to `example.com`, and an unauthenticated
SSH identification-banner exchange with `ssh.github.com:443`. It requires
public network access, creates timed decisions only for disposable tasks,
and performs no login, push or publication.

`timed_ui.py` additionally requires a visible Hyprland desktop, `hyprctl`,
`grim`, and the built Deck window executable. It launches without taking
initial focus and drives GPUI input using `DECK_CONTROL`. It records the
card, Access expiry, Construct's actual ACP/HTTP feedback and journal data.
It runs `/usr/local/bin/construct acp` directly, with the host project copied
into `/workspace`; no guest path alias is installed. The host task record
keeps the original project path. The user keyring and desktop settings are untouched.

Results, failures, exact product patch and inspected screenshots are in the
[2026-11-06 report](../../docs/acceptance/2026-11-06-timed-linux.md).
