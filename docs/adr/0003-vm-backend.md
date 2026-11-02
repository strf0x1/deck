# 0003 — The VM backend: pinned libkrun, an aarch64 guest, a separate journal disk

Status: accepted, 2026-11-02. Answers ADR 0001's #16 follow-ups and records
the contract change the spike forced. Measurements and the recipe are in
`docs/vm-spike.md`.

## Context

ADR 0001 settled the task contract and said the VM backend would change the
holder, not the contract. #16 is the first real holder: a pinned libkrun VMM
that boots a Construct guest and speaks ACP over vsock. Five questions had to
be answered before the disk layout existed.

## Decision

### 1. aarch64 guest, built for aarch64 Linux

Apple Silicon under Hypervisor.framework runs only aarch64 guests, so the
guest is aarch64 even though Construct's release dist ships only
`x86_64-unknown-linux-gnu`. The spike builds Construct for
`aarch64-unknown-linux-gnu` inside a native arm64 `rust:1.91-bookworm`
container and copies the binary into a Debian bookworm arm64 root filesystem.
No x86_64 Linux host is in scope. #10 owns turning this one-off build into a
real artifact; the container is a build tool, not a runtime dependency.

Docker is used only by `scripts/vm-image.sh`. The guest image is a 6 GiB
ext4 root with Construct at `/usr/local/bin/construct`, socat, and
`/usr/local/bin/deck-guest`; the journal template is a separate 256 MiB ext4
disk. A task clones both with APFS `clonefile(2)`.

### 2. Pin libkrun 1.19.7 and accept only its 1.x ABI

Pinned: libkrun **v1.19.7** (`43b2f5aadeebb90f42053c9df2ec8b16583e554f`)
with **libkrunfw 5.6.2**, built `BLK=1 NET=1`. The 2.0 `main` API is not
used. `deck-runtime` loads the dylib at run time and requires
`krun_add_disk3` and `krun_get_shutdown_eventfd`; those are absent from 2.0,
so a wrong library fails at load, not at first boot.

The surface the backend uses:

| Need | libkrun 1.19.7 |
|---|---|
| Disk with a chosen flush mode | `krun_add_disk3` (`RAW`, `direct_io`, `SYNC_{NONE,RELAXED,FULL}`) |
| Boot from that disk | `krun_set_root_disk_remount("/dev/vda", "ext4")` |
| Project import, first boot only | `krun_add_virtiofs3("import", dir, 0, read_only=true)` |
| ACP into the guest | `krun_add_vsock_port2(port, socket, listen=true)`: libkrun owns the host listener and opens the guest connection when the runtime connects |
| Guest readiness | `krun_add_vsock_port2(1025, socket, listen=false)`: the guest connects out, libkrun connects to the VMM's listener |
| Stop request | `krun_get_shutdown_eventfd` (aarch64 macOS only), written from a signal handler |

`listen` is the easy one to get backwards. True is host-listens/guest-listens:
what an editor needs. False makes the *guest* initiate, which the readiness
port uses.

### 3. Construct's journal gets its own disk

The root disk is the guest OS plus `/workspace`. Construct's home
(`CONSTRUCT_HOME=/root/.construct`) is a second raw ext4 disk mounted there
by the guest entry. `destroy` deletes both, so the transcript still dies with
the task.

Why not a host-side mirror: Construct's journal is SQLite with a WAL. A
virtiofs share would put WAL locking and fsync on the host filesystem, which
is the one part of this stack not designed for SQLite, and the host still
cannot read the guest's ext4 without mounting it. A separate guest disk keeps
journal I/O native, isolates a workspace corruption from the transcript, and
lets the journal use `SYNC_FULL` while the workspace stays `RELAXED`.

The disk mode is not the whole guarantee. Construct originally opened the
WAL with `PRAGMA synchronous=normal`, so commits were not fsynced and a VMM
death could lose them. construct#161 fixed it with `synchronous=FULL`, synced
directory creation and a first-commit directory fsync.

Measured in this VM: with that fix, a completed turn followed by a VMM
`kill -9` and no guest `sync` leaves `journal.sqlite`, the WAL and 18 rows in
the journal image **after ext4 replays its journal** (`e2fsck -fy` on a
copy). The pre-fix `NORMAL` build leaves an empty database. Raw `debugfs` on
an unclean image shows neither; the harness must replay first. `session/close`
on the stop path is best effort; hard-death durability now comes from
SQLite's fsyncs plus ext4 recovery.

### 4. The VMM is the holder, and stop is confirmed by the VMM's death

`deckd --vmm <config.json>` is the detached holder. It holds the task's
`flock`, serves the ACP relay socket, and is the pid the record names. The
runtime starts it with `setsid`, transfers the lock through exec, and adopts
it from the record after a `deckd` crash.

Stop has two layers:
1. The runtime best-effort sends ACP `session/close` over the relay, so
   Construct closes its journal.
2. It asks the VMM's control socket to stop. The VMM writes libkrun's
   shutdown eventfd (the guest power key). If the guest powers off, the VMM
   returns from `krun_start_enter`; if it does not within the grace, the VMM
   exits and HVF tears the VM down. Either way the disk's writer is gone.

Confirmation is the recorded process group being gone: `kill(-pgid, 0)`
returning `ESRCH`, with `ps` consulted for orphan zombies, exactly as the
local backend does. **libkrun on macOS gives no in-guest shutdown
acknowledgement**; there is no "filesystem unmounted" proof, only "the VMM
process is gone". A disk whose VMM is not confirmed gone is never reused.
The spike guest has no power-key handler, so the poweroff request is not even
observed by the guest; a later guest init (or systemd) can change that
without changing the contract.

### 5. Contract change: `destroy` takes the task

`Backend::destroy` received only `Spec`, which was enough for the local
backend (it owns nothing) and becomes wrong for any backend with per-task
files. It now receives `&Task`, so the VM backend can remove
`tasks/<id>/disk.img`, `journal.img`, `console.log` and `vmm.json`. This is
the only contract change the spike forced; the local and detached backends
ignore the argument.

### 6. Reattach and the relay

The VMM connects to the guest once and serves that stream through the same
`relay` the local holder uses. Closing Deck's window closes a peer, not the
guest's stdin. A new window attaches to the relay socket; when the agent kept
running it uses ADR 0002's live-attach handshake. The guest connection is
opened only after the guest connects out to the readiness port, because
libkrun accepts a host connection to the ACP forward before the guest's
listener exists and resets it.

## Consequences

- The contract is unchanged apart from `destroy(&Task)`; `deckd`, the
  control protocol and Deck's window do not know a VM is involved.
- The VMM binary is `deckd` itself, so the task record's `backend` is `vm`
  and a record is never run by the wrong backend.
- On macOS, `deckd` must be codesigned with
  `com.apple.security.hypervisor` (`scripts/vm-sign.sh`). Without it
  `hv_vm_create` fails as a bare `EINVAL`, which `docs/vm-spike.md` records
  as the first thing to check.
- A guest image is required for every task; the backend names it by
  environment and refuses to start without it, rather than falling back to
  the local backend.
- One runtime serves one backend. `Runtime::open` skips records whose
  `backend` is another name, and `destroy` refuses them, so switching a data
  root between `local` and `vm` leaves the old tasks alone instead of
  mis-reconciling or deleting them.
- The journal disk is not readable from the host. Backups stay #8's problem.
- Construct should fsync the journal at commit (or checkpoint on
  `session/close`); otherwise a VMM crash loses the last rows. This is a
  construct-side change, not a runtime one.
- Revisit if: libkrun 2.0 stabilises and exposes an equivalent sync-mode and
  shutdown API; #10 produces a signed, content-addressed guest image; the
  guest gains a poweroff handler; or the flush measurement shows `RELAXED`
  is not enough for the journal and the root disk needs `FULL` too.

## Evidence

- #16, ADR 0001 (decisions 3, 6, 7), ADR 0002 (live attach).
- libkrun v1.19.7 `include/libkrun.h`, `src/libkrun/src/lib.rs`,
  `src/devices/src/virtio/vsock/{unix,muxer_thread}.rs`, `hvf-entitlements.plist`.
- `crates/deck-runtime/src/vm.rs`, `vmm.rs`, `krun.rs`,
  `tests/vm.rs`; `scripts/vm-image.sh`.
- Measurements: `docs/vm-spike.md`.
