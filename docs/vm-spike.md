# libkrun VM spike (#16)

Status: spike complete on macOS 27 / Apple M4 Pro, 2026-11-02, and verified
on Linux/x86_64 (KVM) the same day. Decisions are in
[ADR 0003](adr/0003-vm-backend.md); this file is the recipe and the numbers.

The backend is `crates/deck-runtime/src/vm.rs`; the holder is
`vmm.rs`/`krun.rs`, run as `deckd --vmm <config.json>`. The guest is a
hand-made Debian disk for the host architecture (aarch64 on macOS, x86_64 on
Linux) with Construct at `/usr/local/bin/construct`. No environment pipeline
exists: that is #10.

## Recipe

Host prerequisites (macOS/arm64):

```sh
brew install lld xz libkrunfw
rustup target add aarch64-unknown-linux-musl
```

Build the pinned libkrun (1.19.7 + libkrunfw 5.6.2) from source:

```sh
git clone --depth 1 --branch v1.19.7 https://github.com/libkrun/libkrun /tmp/libkrun
cd /tmp/libkrun
# bindgen's build scripts are linked with @rpath/libclang.dylib; dyld looks
# in target/release for it.
ln -sf "$(brew --prefix llvm)/lib/libclang.dylib" target/release/libclang.dylib
LIBCLANG_PATH="$(brew --prefix llvm)/lib" DYLD_LIBRARY_PATH="$(brew --prefix llvm)/lib" \
  make -j8 BLK=1 NET=1
```

Build the guest image (Docker is a build tool only). The original
measurements used Construct at `883e7fd` (0.3.0) and `rust:1.91-bookworm`;
the final verification used Construct `v0.3.1` (`8836ed0`), which contains
#163 and #164. That release ships macOS and x86_64 Linux artifacts only, so
the aarch64 guest is built from the tag with `vm-image.sh`. The script takes
the construct checkout as its first argument so the commit is the caller's
choice:

```sh
scripts/vm-image.sh ../construct target/vm
```

Sign the daemon; macOS refuses `hv_vm_create` without the entitlement:

```sh
scripts/vm-sign.sh target/debug/deckd
```

Run the contract test (the harness needs a `deckd` cargo will not relink, so
sign a copy):

```sh
cp target/debug/deckd /tmp/deckd && scripts/vm-sign.sh /tmp/deckd
DECK_VM_IMAGE=target/vm/base.img \
DECK_VM_JOURNAL=target/vm/journal.img \
DECK_VM_KRUN=/tmp/libkrun/target/release/libkrun.1.19.7.dylib \
DECK_VM_EXECUTABLE=/tmp/deckd \
DECK_GATEWAY_EXECUTABLE=$PWD/target/debug/deck-gateway \
cargo test -p deck-runtime --test vm -- --nocapture
```

For a prompt + permission round trip, run construct's scripted endpoint on
the host and name it as the gateway's upstream with `CONSTRUCT_BASE_URL`
(`http://127.0.0.1:<port>/v1`). The guest has no IP route (ADR 0004), so the
guest's Construct talks to `deck-egress` on guest loopback; that relay carries
the stream over vsock to the `deck-gateway` process, which fetches the host
key from `deckd` for that request (ADR 0004 §4) and forwards it upstream. The
test starts its own `deckd` from `DECK_VM_EXECUTABLE`, with a decoy
`OPENAI_API_KEY` exported that must reach neither the VMM nor the gateway;
the real key is in a `DECK_CREDENTIALS` file. The measurement run:

```sh
construct-dev serve --script script.json --port-file /tmp/port
export DECK_CREDENTIALS=/tmp/deck-credentials.json
echo x | /tmp/deckd credential set OPENAI_API_KEY
CONSTRUCT_BASE_URL=http://127.0.0.1:$(cat /tmp/port)/v1 \
DECK_VM_MOCK=1 ... cargo test -p deck-runtime --test vm -- --ignored --nocapture
```

### Linux/x86_64 (KVM)

The host needs `/dev/kvm` and `libkrunfw` 5.6.2 (Arch's `extra/libkrun` is
1.19.6 and links a virglrenderer this recipe does not need). Build the pinned
release with only block and net devices:

```sh
git clone --depth 1 --branch v1.19.7 https://github.com/libkrun/libkrun /tmp/libkrun
make -C /tmp/libkrun -j"$(nproc)" BLK=1 NET=1
```

No signing step. Point `DECK_VM_KRUN` at `target/release/libkrun.so.1.19.7`
and put the directory holding `libkrunfw.so.5` on `LD_LIBRARY_PATH`; the VMM
forwards it to `libkrun`'s loader:

```sh
DECK_VM_IMAGE=target/vm/base.img \
DECK_VM_JOURNAL=target/vm/journal.img \
DECK_VM_KRUN=/tmp/libkrun/target/release/libkrun.so.1.19.7 \
DECK_VM_EXECUTABLE=$PWD/target/debug/deckd \
DECK_GATEWAY_EXECUTABLE=$PWD/target/debug/deck-gateway \
LD_LIBRARY_PATH=/usr/lib \
cargo test -p deck-runtime --test vm -- --nocapture
```

The prompt + permission run is the same command with `DECK_VM_MOCK=1`,
`CONSTRUCT_BASE_URL` and a `DECK_CREDENTIALS` file holding `OPENAI_API_KEY`. `scripts/vm-image.sh` picks the
host architecture, so a Linux x86_64 host builds an amd64 Debian guest.
Keep `TMPDIR` off tmpfs: the per-task 6 GiB root is a sparse image, and a
plain copy into a RAM-backed `/tmp` exhausts it.

To drive the real window, export the same `DECK_VM_*` values and run
`cargo run --bin deck`; `deckd` inherits them and runs the VM backend. The
window's default command is `construct acp`, and the backend maps it to
`/usr/local/bin/construct` in the guest. `DECK_GATEWAY_EXECUTABLE` defaults to
`deck-gateway` beside the `deckd` the runtime runs, and `CONSTRUCT_BASE_URL` /
`CONSTRUCT_MODEL` in `deckd`'s environment configure the task's model gateway.
The key is a stored credential (`deckd credential set OPENAI_API_KEY`); it is
never passed into the guest, and `deckd` never reads it from its environment.

## Window check

The real window was run against the v0.3.1 guest, not only the runtime tests.
`deck` started a `construct acp` task in the VM, attached, and reached status
`ready`. Typing `read /root/outside.txt` produced the permission card
(`Allow once`, `Allow for this session`, `Always allow`, `Deny`); answering
`1` ran the tool and the transcript showed the prompt, `read` as done, and the
agent's `done` message. The same run's control socket reported geometry
through the normal window path.

## Egress boundary check (ADR 0004, first slice)

The guest has no IP route. The VMM disables libkrun's implicit vsock and adds
an explicit device with no TSI features, so only mapped ports cross. Two
checks were run on macOS/arm64 with the rebuilt image:

- **Model path.** A scripted `construct-dev` endpoint on host loopback is the
  gateway's upstream (`CONSTRUCT_BASE_URL` in the test process). The guest's
  prompt turn completed in 95 ms, and the endpoint's log recorded
  `"auth":"Bearer host…"` — the host key the gateway injected. The guest's own
  `OPENAI_API_KEY` is `deck-placeholder`, whose first four characters differ,
  so a log line starting `Bearer host…` proves the guest value was discarded.
- **No host loopback.** A host listener on `127.0.0.1:45679` was running while
  the scripted model asked the guest to run
  `socat -u /dev/null TCP:127.0.0.1:45679; echo rc=$?`. The tool result was
  `socat[...] E connect(...): Connection refused` and `rc=1`; the host listener
  accepted nothing. This is bypass case 1 in ADR 0004 §9. The run set
  `CONSTRUCT_SANDBOX=off` by hand; the VM backend now sets it for the guest,
  because Construct's shell tool refuses to run when the guest kernel has no
  Landlock.

The end-to-end model request also proves `deck-egress` works: the guest's
Construct is configured with `http://127.0.0.1:8080/v1`, which exists only in
the guest, and the request reached the host gateway.

These are manual runs, not permanent tests yet: the bypass check needs a
scripted endpoint whose script names the listening port, and the VM test
harness does not yet start `construct-dev` itself. The gateway's own tests
cover header stripping, key injection, model/path denials, and the extension
port refusal.

## Measurements

| | |
|---|---|
| Launch to ACP ready, before #30 | **0.60–0.64 s** (four runs: 616.7, 632.5, 635.6, 624.7 ms), with the fixed 300 ms guest sleep |
| Launch to ACP ready, after #30 | **≈300 ms mean, ≈320 ms worst** over 20 consecutive starts (below) |
| Idle CPU / RSS per task | **0.0 % / 179.5 MiB** (`top` second sample, 2 vCPU / 2048 MiB) |
| Prompt turn with the local scripted model | 22–50 ms before SQLite `FULL`; **210–276 ms with v0.3.1** (permission ask 120–162 ms) |
| Cost of `FULL` in the VM | ≈20–30 ms per durable row (`fsync` on the guest journal disk); ≈8 rows per turn |
| Stop (runtime control, grace 3 s) | ≈ 3 s, then the VMM is gone; the guest does not power off (no power-key handler) |
| SIGTERM without the control socket | 2.05 s (the VMM's own grace) |
| Adopted after `deckd`/runtime restart | immediate; live attach unchanged (ADR 0002) |
| Disk per booted task | 240 MiB allocated from a 6 GiB sparse root (APFS clone), journal 528 KiB after a session |
| After destroy | `disk.img`, `journal.img`, console and config gone; only the task record remains |
| Hard host power cut during a write | journal hash unchanged across the cut; 52 events recovered after ext4 replay, integrity `ok` (one run) |

Linux/x86_64 (same 2 vCPU / 2048 MiB, btrfs host):

| | |
|---|---|
| Launch to ACP ready, before #30 | **0.53–0.62 s** (525, 577, 596, 618 ms); not re-measured here after #30 |
| Prompt + permission | 40–60 ms round trip; the permission ask/answer was 20–40 ms |
| Prompt with no ask (scripted `read`) | 20.1 ms |
| Stop (runtime control, grace 3 s) | ≈ 3.4 s; the record lock is free within 6 ms of the group |
| Disk per task | reflink (`FICLONE`) of the 6 GiB sparse base, so only the guest's writes allocate |

The Linux runs used Construct 0.3.1's x86_64 Linux release (also built from
`ff26f88` inside the guest image). A completed turn with one permission ask,
then `kill -9` of the VMM and `e2fsck -fy` on the journal image's copy, left
`journal.sqlite`, a 391 KiB WAL and **18 rows**, `pragma integrity_check` ok —
the same result as the macOS re-run. The ACP handshake and the turn ran with
no non-JSON line on the stream; secretspec's audit note and keyring warning
were on the guest console.

### Launch path after #30 (macOS/aarch64)

`vm_launch_to_ready_is_event_driven_twenty_times` (`--ignored`) launched a
fresh task 20 times, timing `runtime.start` and the ACP handshake separately.
All 20 reached `Ready`; there was no ACP reset or flake.

| stage | macOS |
|---|---|
| `runtime.start` (spawn VMM → relay reported) | ≈160–200 ms |
| VMM: start → guest connected, back-to-back starts | ≈150–170 ms (132 ms on a single cold run) |
| VMM: relay ready → `runtime.start` returns (record fsync) | ≈25 ms |
| guest entry → provisioned / journal mounted / connecting (`/proc/uptime`) | 0.04 s → 0.06 s |
| `attach` → ACP `Ready` | ≈100–115 ms |
| launch → ACP ready | **≈300 ms mean, ≈320 ms worst** |

The fixed 300 ms sleep, the 1025 readiness connection and its separate socat
process, and the 20 ms polls in the old `await_ready`, `Vm::start` and the
test loop are gone. The readiness event is now the guest's own vsock
connection, accepted with `poll(2)`; the VMM reports through the existing
startup pipe and the runtime wakes on the line instead of polling
`is_finished`.

The remaining time is not in the readiness path. The guest entry starts at
0.04 s of guest uptime and connects at 0.06 s, but `krun.start`'s pre-guest
setup plus kernel and init put that entry roughly 100 ms into the host's
launch. The first ACP handshake costs 99–117 ms; a second handshake in the
same Construct process costs 37–50 ms. The difference is Construct's
first-request work, which begins when the host sends `initialize`, so
starting the agent earlier cannot hide it.

The #30 target (<150 ms on both arches) is therefore **not met on macOS**:
guest boot plus Construct's first handshake alone exceeds it, and neither is
in the readiness code. Hiding the boot needs a pre-warmed VM/snapshot
(libkrun 1.19.7 exposes no snapshot API) or a lean libkrunfw; #30's item 3.
Linux was not re-measured on this host after #30.

## Journal placement and flush

Construct's home is `/root/.construct` on a **separate 256 MiB ext4 disk**
(`journal.img`) mounted by the guest entry. The root disk (`disk.img`) holds
the OS and `/workspace`. `destroy` deletes both. Inside the guest the project
is copied once from a read-only virtiofs import; it is never the live
workspace.

Disk modes: root `KRUN_SYNC_RELAXED`, journal `KRUN_SYNC_FULL`. On macOS
`RELAXED` calls `fsync` on the host file but does not ask the drive to flush
its cache; Apple's internal SSD exposes no battery-backed write cache to the
OS, so a host power loss is close to `FULL` in practice, but the distinction
is recorded rather than assumed.

Two experiments, both ending in `kill -9` of the VMM:

1. **Synced root write survives; unsynced data does not.** The guest wrote
   `synced.txt`, called `sync`, then appended 64 MiB in 1 MiB chunks and
   printed `CHUNK n`. It was killed around chunk 42. After the kill,
   `debugfs` on `disk.img` shows `synced.txt` with its content, and
   `stream.bin` **does not exist at all**. Guest page cache and ext4 delayed
   allocation are lost with the VMM; a guest flush reaches the host file.
2. **A guest-wide `sync` makes the journal durable.** A completed Construct
   turn (one permission ask, the tool denied, a final message) wrote 18
   journal rows. Without a guest `sync`, the journal image was still empty
   after `kill -9`: Construct opens SQLite WAL with
   `PRAGMA synchronous=normal`, so commits are not fsynced. With
   `DECK_SYNC=1` (a one-second `sync` loop used only for this measurement),
   the journal image held `journal.sqlite`, a 391 KiB WAL and all 18 rows
   after the kill.

The original second result looked like a Construct-side gap: a hard VMM
death can lose journal commits that are still in guest memory. A normal
runtime stop sends ACP `session/close` first, which closes the journal, but
the backend cannot prove the guest unmounted or checkpointed. Tracked in
construct#161.

**The first conclusion drawn here was wrong: raw `debugfs` on a killed VM
under-reports, because ext4 has not replayed its journal yet.** Re-running the
same images through `e2fsck -fy` before inspecting changes the result.

- Fixed Construct (`f2dd5df`: `synchronous=FULL`, synced directories, first
  commit directory fsync), completed turn, no guest `sync`, VMM `kill -9`:
  after replay the journal image holds `projects/<id>`, `journal.sqlite`, the
  WAL and **18 rows**. Raw `debugfs` showed an empty filesystem; the rows were
  in the ext4 journal.
- The pre-fix build (`883e7fd`, `synchronous=normal`) under the same kill
  leaves an empty database after replay (no `events` table): the fix is what
  makes the rows durable.
- The fsync controls that appeared to lose data were the same artifact. After
  replay, a file appended with `fsync` (and its directory entry) is present.
  The 35 s appearance without a kill is ext4 checkpointing a clean, mounted
  filesystem, not the only durability path.

The harness must replay a killed guest's ext4 journal (`e2fsck -fy` on a copy,
or mount it in a guest) before judging durability. `session/close` on the stop
path remains best effort; with construct#163 the hard-death path is covered by
SQLite's fsyncs plus ext4 recovery.

A hard host power cut was run on 2026-11-03: the VMM was killed mid-turn
(seven transcript entries observed), then the Mac was powered off by holding
the power button. The journal image's SHA-256 was identical before and after
the reboot, and ext4 replay recovered 52 SQLite events with
`PRAGMA integrity_check` returning `ok`. This one run therefore lost no
host-side journal bytes. It cannot distinguish the page cache surviving the
cut from APFS writing the pages in the seconds before shutdown, nor does it
exercise the SSD's own volatile cache, which `RELAXED` does not flush.

## Confirmed stop

libkrun on macOS gives no in-guest shutdown acknowledgement. The VMM's
control socket `Stop` is the runtime's primary stop: it asks Construct to
close the session, writes `krun_get_shutdown_eventfd` (the guest power key),
and waits the grace. The guest image has no power-key handler, so the guest
does not power off; the VMM then exits, and HVF tears the VM down. A
`SIGTERM` to the VMM takes the same path and exits after 2 s. Confirmation is
the recorded process group being gone (`kill(-pgid, 0)` plus a `ps` zombie
check), not a guest filesystem state. A disk is reused only after that.

On Linux, libkrun exports `krun_get_shutdown_eventfd` only on
macOS/aarch64, so `Stop` has no guest power-key request: it waits the grace
and exits the VMM. Confirmation also waits for the **record lock**, not just
the process group. The VMM's threads share one file table, so a vCPU thread
blocked in `KVM_RUN` can outlive the group leader by a few milliseconds, and
the `flock` with it; an immediate resume after stop otherwise failed with
"held by another process". `group_running`'s zombie check is correct for a
single-threaded holder, but not for this multi-threaded one.

**First thing to check when a VM fails to start:** `hv_vm_create` returns a
bare `EINVAL` if `deckd` is not codesigned with
`com.apple.security.hypervisor`. Re-run `scripts/vm-sign.sh` after every
rebuild.

## Limitations

- The guest is a host-architecture Debian bookworm with glibc, built
  natively in a container (`scripts/vm-image.sh`). Construct's release dist
  has x86_64 Linux and both macOS architectures; #10 must own producing this
  artifact.
- The guest has no power-key handler, so stop is VMM death, not guest
  shutdown; this is why the journal needs the Construct-side fix above.
- The root disk's first boot copies `spec.cwd` through a read-only import
  share. That is a one-time provisioning compromise, not a workspace mount;
  #10 replaces it with a real environment/checkout.
- The image has no general network and no IP route. The VMM disables
  libkrun's implicit vsock (which would enable TSI and proxy the guest's
  AF_INET sockets through the host) and adds an explicit vsock device with no
  TSI features. Only the mapped ports cross: ACP (1024), the model gateway
  (1026) and the reserved extension port (1027). `deck-egress` listens on
  guest loopback at 8080 for clients that cannot speak AF_VSOCK, but it is a
  byte carrier: the host gateway applies the same policy to a direct vsock
  connection. That is ADR 0004's boundary, not a policy exception. The image
  also still has no resolver: host names resolve only on the host, in a
  gateway kind that has shipped.
- A data root is served by one backend. With `DECK_VM_IMAGE` set every new
  task is `vm`; records from the previous `local` run are left untouched and
  the VM runtime refuses to destroy them.
- The first guest image merged the agent's stderr into the ACP socket (socat's
  `stderr` address option), so secretspec's note and keyring warning appeared
  inline. Construct's stdout was already clean; construct#164 makes the
  separation structural, and this image now leaves stderr on the guest console
  instead of merging it. Deck still ignores non-JSON lines defensively.
- Linux support is x86_64 only; other Linux architectures are rejected at
  `Vm::from_env`. Snapshots and the QEMU comparison (#12) were not tried.
- libkrunfw 5.6.2's kernel has no Landlock, so Construct's own `workspace`
  sandbox refuses shell and file tools in the guest ("Landlock isn't
  available on this kernel") after the permission answer. The VM backend
  therefore sets `CONSTRUCT_SANDBOX=off` for the guest as host policy (ADR
  0004 §2.1); the guest is the sandbox and the host gateways are the external
  boundary. The local backend keeps Construct's configured sandbox. Revisit
  if the image gets a Landlock-capable kernel or agents share a guest.
- `DECK_SYNC=1` is a measurement aid in the guest image; normal runs leave it
  off.
