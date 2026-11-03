# Working on deck

deck is a native ACP client in Rust on GPUI (Zed's UI framework): it launches
an agent (construct by default) through `deckd` and shows its session, tool
calls and permission questions in a dark-glass window. This is the UI spike;
`PLAN.md` is the product discussion and is not an implementation plan.

## Layout

A Cargo workspace. Crates are the boundaries the compiler enforces:
`deck-acp` and `deck-runtime` never depend on GPUI or any UI crate (CI
checks `cargo tree`), so protocol and runtime work builds and tests without
compiling GPUI. `deck-runtime` depends on `deck-acp`, and `deck` on both,
never the reverse. Decisions that constrain later work are ADRs in
`docs/adr/`, in construct's format; read them before changing what they
settle.

| Path | What |
|---|---|
| `crates/deck-acp/` | The ACP client, no UI. `rpc.rs` line framing (mirrors construct's), `conn.rs` the pure protocol state machine, `inbound.rs` typed events and payload decoding, `shutdown.rs` the bounded close-only exchange, `client.rs` the connection over any reader/writer pair and its reader thread. The UI only sees `Inbound`. |
| `crates/deck-runtime/` | Tasks, no UI (ADR 0001). `lifecycle.rs` the pure state machine folded from lifecycle events, `record.rs` task records under the data root (`events.jsonl`, `task.json`, the liveness `lock`), `backend.rs` the backend contract, `local.rs` the backend that runs the agent as a child, `runtime.rs` create/start/attach/stop/resume/destroy and the notice stream. `approvals.rs` the host approval queue: pending items, once/task/project grants, attachment leases and the bounded hold (ADR 0004 §5). |
|`crates/deck-runtime/src/daemon.rs`, `daemon_client.rs`, `relay.rs`|The daemon authority, its control client, and the JSON-agnostic line relay. `holder.rs` owns each task independently; `detached.rs` launches and adopts it. `child.rs` ties agent process groups to holder lifetime. See `docs/deckd.md`.|
|`crates/deck-runtime/src/vm.rs`, `vmm.rs`, `krun.rs`|The libkrun backend: `vm.rs` is the `Backend`/`Instance`, `vmm.rs` is the detached holder (`deckd --vmm`) that owns the microVM, and `krun.rs` loads the pinned 1.x C API. The guest opens the ACP stream out to the VMM's forward listener at boot, so that connection is both readiness and the stream; there is no polling or fixed delay. The VMM starts the task's egress gateway, maps its sockets, and disables libkrun's implicit vsock, so the guest has no IP route (ADR 0004). Decisions and measurements: `docs/adr/0003-vm-backend.md`, `docs/adr/0004-host-egress-and-credentials.md`, `docs/vm-spike.md`.|
|`crates/deck-gateway/`|The host-side egress gateway: a pure policy core, a bounded HTTP/1.1 carrier, the OpenAI-compatible model gateway, the daemon approval-queue client, and the extension port's `boundary` kind (`/events`, long-polled `/approvals/<id>`, agent withdrawal). It listens only on Unix sockets the VMM maps from guest vsock ports, strips guest credential headers, and injects the host key; a model the task was not configured with asks `deckd` and reports the approval id in a bounded 403. Its binary is named by `DECK_GATEWAY_EXECUTABLE`, defaulting beside `deckd`.|
|`crates/deck-egress/`|The guest relay: loopback TCP to a host vsock port, with `--kind` naming the gateway before the stream for the extension port. Built for the guest image by `scripts/vm-image.sh`; it is a byte carrier, so bypassing it does not widen the boundary. The host build exists so the workspace checks cover it.|
|`crates/deck-runtime/tests/vm.rs`|The same lifecycle contract against a real guest; skipped unless `DECK_VM_*` names an image, journal, libkrun and a `deckd` that can run `--vmm` (signed on macOS). The ignored 20-launch test prints the launch → ACP-ready stages.|
|`scripts/vm-image.sh`, `vm-sign.sh`|Build the hand-made Debian guest image for the host architecture with Docker (its `deck-guest` entry connects ACP out to the host and marks boot stages from `/proc/uptime`), and codesign a `deckd` with the Hypervisor entitlement (macOS).|
| `crates/deck-runtime/tests/daemon.rs` | The real daemon binary: locking, idle shutdown, detached survival, holder adoption after daemon death, holder-crash cleanup, and concurrent control clients. |
| `crates/deck-runtime/tests/lifecycle.rs` | The contract against real processes: stop with SIGTERM then SIGKILL, unclean exits, the agent's children, reconciliation after a holder dies, reuse, destroy. |
| `crates/deck-acp/src/bin/deck-fake-agent.rs` | Scripted ACP agent: streams, calls a tool, asks permission. For tests and for driving the UI without a model. `--listen <socket>` serves a Unix socket instead of stdio. Supports live attach, `session/load`, `resume` and `close` like construct; with `DECK_BOUNDARY_URL` set it advertises both the reverse-DNS and bare boundary keys and puts a late decision in its transcript. `DECK_FAKE_STATE=<dir>` persists sessions across processes; `DECK_FAKE_NAMES=legacy|reverse` pins it to one extension namespace like a particular Construct release. Keep it in step with what Deck relies on from construct (ADR 0002). |
| `crates/deck-acp/tests/acp.rs` | Client against the fake agent over stdio, and over a Unix socket the client did not launch; the fake's sessions across two processes. |
| `crates/deck/src/main.rs` | Argument split only: `deck [--] [agent argv…]`, default `construct acp`. |
| `crates/deck/src/app.rs` | Opens the window (blurred background, `app_id = "deck"`); app keys: `cmd-q` (macOS) / `ctrl-q` quits, and closing the last window quits. |
| `crates/deck/src/task.rs` | The window's daemon connection: finds or starts a task, live-attaches when supported, and explicitly restarts unsupported detached tasks. Dropping it detaches. |
| `crates/deck/src/ui/root.rs` | The one view entity: owns `Board`, `Rail`, `Session`, the client and the window's task; routes keys and mouse; steps animation. `ui/attachment.rs` connects and restarts its daemon-owned task. |
| `crates/deck/src/ui/chrome.rs`, `panels.rs`, `conversation.rs` | Render helpers: rail and header; sessions column; transcript, permission card, prompt. No state. |
| `crates/deck/src/ui/session.rs`, `board.rs`, `rail.rs`, `boundary.rs`, `spring.rs` | Pure state with unit tests: transcript folding, drag/reorder, rail open/close, the boundary card's pending items, spring physics. |
| `crates/deck/src/ui/theme.rs` | Colour tokens, fonts (Adwaita Sans for UI, JetBrainsMono Nerd Font for code and icons), icon glyphs. Colours come only from here. |
| `crates/deck/src/control/` | Dev-only control socket (`DECK_CONTROL`). `protocol.rs` the pure line grammar, `socket.rs` the listener threads, `drive.rs` dispatches commands as GPUI input on the window and reads `state`. |
| `crates/deck/tests/view.rs` | The view under GPUI's headless test platform: keystrokes, mouse drags, real layout. |
| `crates/deck/tests/reattach.rs` | Production window connections against daemon-owned fake and optional real Construct agents. Set `CONSTRUCT_BIN` to run the real scripted-model cases. |
| `crates/deck/tests/control.rs` | Control commands run against a headless window (no compositor in CI). |

A connection and the process behind it are separate things. `Client` speaks
ACP over any reader/writer pair — a child's stdio, or a socket a VM-hosted
agent listens on — and dropping it closes that stream. Processes belong to
`deck-runtime`: a task's record says what ran and how it ended, and only a
runtime stop or destroy ends the agent. `deckd` supervises detached per-task holders, which own the agents and keep
their ACP streams open while Deck or `deckd` is closed. The next window with the same
command and directory finds the running task and rebuilds it with `session/load`
when the agent advertises live attach. Otherwise the window offers explicit stop
and restart. Runtime stop sends advertised `session/close` before termination. Records live
under the data root (`~/Library/Application Support/deck`, `${XDG_DATA_HOME:-~/.local/share}/deck`;
`DECK_DATA` overrides it). A future transport implements `deck_acp::Writer`
to say how closing works for it.

Shared versions, lints and profiles live in the root `Cargo.toml`
(`[workspace.*]`, taken with `.workspace = true`); `[patch]` and
`[profile.*]` only work there. A dependency goes in the crate that uses it.

## Run it

```sh
cargo build                                              # every crate, the fake agent included
cargo run --bin deck -- target/debug/deck-fake-agent    # no model, permission dialog included
cargo run --bin deck                                     # construct acp in the current dir
```

construct without spending tokens: start its scripted model (see
construct's AGENTS.md, `scripts/mock.sh`). The local backend takes the
endpoint from its own environment:
`deck -- env CONSTRUCT_HOME=… CONSTRUCT_BASE_URL=http://127.0.0.1:$port/v1 OPENAI_API_KEY=x construct acp`.
With `DECK_VM_*` set, the same `CONSTRUCT_BASE_URL`, `CONSTRUCT_MODEL` and
`OPENAI_API_KEY` configure the task's host gateway instead; the guest gets the
relay URL and a non-empty placeholder, never the key.

### The libkrun backend (macOS/arm64, Linux/x86_64)

Build the pinned libkrun 1.19.7 and the guest image once
(`docs/vm-spike.md` has the commands), then on macOS:

```sh
scripts/vm-sign.sh target/debug/deckd          # hv_vm_create needs this
export DECK_VM_IMAGE=target/vm/base.img DECK_VM_JOURNAL=target/vm/journal.img
export DECK_VM_KRUN=$HOME/src/libkrun/target/release/libkrun.1.19.7.dylib
cargo run --bin deck                           # construct acp in a VM
```

`DECK_GATEWAY_EXECUTABLE` defaults to `deck-gateway` beside the `deckd` the
runtime runs, so `cargo build` is enough for `cargo run`. A test harness that
points `DECK_VM_EXECUTABLE` at a copy elsewhere must name the gateway too.
The VM backend sets `CONSTRUCT_SANDBOX=off` for the guest: the VM is the
sandbox and the host gateways are the external boundary (ADR 0004 §2.1). It
also sets `DECK_MANAGED=1`, which tells Construct the guest's credentials are
host-owned and injected at the gateway (ADR 0004 §4). The local backend leaves
Construct's configured sandbox and credential mode alone.

On Linux/x86_64 the same variables point at the host-arch image and
`libkrun.so.1.19.7`; no signing step, but `/dev/kvm` must be accessible and
`libkrunfw.so.5` must be reachable (the VMM forwards `LD_LIBRARY_PATH`):

```sh
export DECK_VM_IMAGE=target/vm/base.img DECK_VM_JOURNAL=target/vm/journal.img
export DECK_VM_KRUN=$HOME/src/libkrun/target/release/libkrun.so.1.19.7
export LD_LIBRARY_PATH=/usr/lib   # directory holding libkrunfw.so.5
cargo run --bin deck
```

`deckd` inherits `DECK_VM_*`, so the window's task is a VMM. The project is
copied into the guest's writable disk on first start; it is not a host
mount. The lifecycle contract runs with
`cargo test -p deck-runtime --test vm` when the same variables plus
`DECK_VM_EXECUTABLE` (a `deckd` binary) and `DECK_GATEWAY_EXECUTABLE` are set.
A bare `EINVAL` at VM start on macOS is almost always an unsigned daemon.

### Driving the live window (agents)

Wayland only sends keys to the focused window, and `wtype`/`ydotool` type
into whatever the user has focused. Instead, set `DECK_CONTROL` to a socket
path and deck takes newline-delimited JSON commands there, one JSON reply
per line. Input is dispatched through GPUI's own `Window::dispatch_event`, so
it hit-tests, hovers and focuses like real input, and the window never needs
focus. No socket without the variable. Keep the path short (Unix sockets cap
at ~108 bytes): use `$XDG_RUNTIME_DIR`, not a deep scratch directory.

```sh
export DECK_CONTROL=$XDG_RUNTIME_DIR/deck-ctl.sock
# On Hyprland (0.56+, Lua dispatch), open it without taking the user's focus:
hyprctl dispatch "hl.dsp.exec_cmd(\"env DECK_CONTROL=$DECK_CONTROL $PWD/target/debug/deck -- $PWD/target/debug/deck-fake-agent\", { no_initial_focus = true })"
c() { printf '%s\n' "$1" | socat -t 30 - UNIX-CONNECT:"$DECK_CONTROL"; }
c '{"cmd":"type","text":"Ship the spike"}'; c '{"cmd":"key","key":"enter"}'
c '{"cmd":"key","key":"1"}'                     # answer the permission card
c '{"cmd":"drag","from":{"x":200,"y":150},"to":{"x":200,"y":310},"steps":20}'
c '{"cmd":"hover_rail"}'; c '{"cmd":"select_rail","index":3}'
c '{"cmd":"settle"}'; c '{"cmd":"state"}' | jq .
```

Commands: `type {text}`; `key {key, modifiers?}` (`enter`, `escape`,
`backspace`, arrows, a single character; modifiers `ctrl`, `alt`, `shift`,
`super`); `click {x,y}`; `move {x,y}`; `drag {from, to, steps?}` (one move per
frame); `hover_rail`; `select_rail {index}`; `wait {ms}`; `settle` (until no
spring moves, at most 2 s); `state` (board order, card ys, dragging; rail
width/active/pinned/expanded; session status, entries, ask; draft;
animating); `geometry`. Coordinates are window-relative logical pixels, as
in layout. Errors reply `{"ok":false,"error":…}`.

Screenshots: Wayland hides the window's position from the client, so
`geometry` reports bounds at 0,0 plus its `pid`; ask the compositor where
that pid's window is and hand that to grim. The window must be on a visible
workspace for frames (and the screenshot) to draw.

```sh
pid=$(c '{"cmd":"geometry"}' | jq .pid)
grim -g "$(hyprctl clients -j | jq -r ".[] | select(.pid==$pid) | \"\(.at[0]),\(.at[1]) \(.size[0])x\(.size[1])\"")" deck.png
```

On macOS none of that is needed: `geometry` reports the real window frame in
points, so hand its bounds straight to `screencapture` (`-o` drops the
shadow). There is no `$XDG_RUNTIME_DIR` and `socat` is not installed by
default, but `nc -U` speaks the same protocol and a short path under `/tmp`
is fine.

```sh
export DECK_CONTROL=/tmp/deck-ctl.sock
./target/debug/deck ./target/debug/deck-fake-agent &   # a window in this session
c() { printf '%s\n' "$1" | nc -U -w 2 "$DECK_CONTROL"; }
b=$(c '{"cmd":"geometry"}' | jq -r '.bounds | "\(.x),\(.y),\(.w),\(.h)"')
screencapture -x -o -R"$b" deck.png
```

Both platforms need a real window to draw into: a headless shell has no
display, and macOS captures nothing while the session is locked. On macOS,
`screencapture` also fails ("could not create image from rect") until the
app the shell runs under (Terminal, the Claude app, …) is allowed in System
Settings → Privacy & Security → Screen & System Audio Recording; reopen
that app after granting it.

## Checks

```sh
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
```

All three must be clean, on Linux and on macOS; CI runs them on both, blocking. A UI behaviour change gets a `crates/deck/tests/view.rs` case
(simulate the input, assert on view state); a protocol change gets a
`conn.rs` unit test and, if it crosses the process, a
`crates/deck-acp/tests/acp.rs` step; a lifecycle change gets a
`lifecycle.rs` unit test and, if it involves a process, a
`crates/deck-runtime/tests/lifecycle.rs` case. `cargo test -p deck-acp` and
`cargo test -p deck-runtime` run those alone, without building GPUI.

## Conventions

Same as construct's (see `../construct/AGENTS.md`), in short:

1. `main.rs` only parses arguments; logic lives in the lib.
2. Every module starts with a `//!` paragraph: what it is for and the one design choice a reader must know.
3. One concern per file; soft ceiling 300 non-test lines.
4. No `unwrap`/`expect` outside tests; escape hatches are `#[expect(..., reason)]`, never `#[allow]`.
5. Pure state (no GPUI types) wherever possible, so it is unit-testable; views read it.
6. ACP extension names and payloads are a shared contract
   (`strf0x1/sprawlcommons-acp`, pinned by `CONTRACT.lock`); see
   "The ACP extension contract" below.

## The ACP extension contract

Deck and Construct's shared ACP wire lives in one private, versioned
repository: `strf0x1/sprawlcommons-acp`. Its `SPEC.md`, `schema/` and
`conformance/matrix.json` are normative; the prose is not vendored here.
`CONTRACT.lock` names the release, and
`crates/deck-acp/tests/fixtures/acp-contract/<version>/` holds the vendored
matrix and schemas. ADR 0005 records the names; ADR 0006 records the pin.

1. A change to an extension method, `_meta` key, capability flag or payload
   starts in the contract repo, not here. Add the matrix/schema/CHANGELOG
   entry and bump `VERSION` by semver (patch = prose/fixture, minor =
   additive, major = rename/removal or changed meaning).
2. Tag `v<VERSION>` there; here, run `scripts/acp-contract.sh update <version>`
   and extend `crates/deck-acp/tests/contract.rs` if the matrix grew.
3. Construct pins the same release in the same wave. Never land a wire change
   in one implementation only.
4. `scripts/acp-contract.sh verify` runs in CI and fails on a stale vendored
   copy or a hand edit.
5. The bare names are removed in contract 2.0.0, one released pair after both
   speak 1.x.

Read `SPEC.md` before changing `crates/deck-acp/src/names.rs` or the
`initialize` negotiation in `conn.rs`: its §4 table is the rule both sides
implement.

## GPUI notes

- Pinned to Zed `v0.233.10` (rev `350f3387…`) as a git dependency, with Zed's
  `[patch.crates-io]` entries mirrored and `Cargo.lock` seeded from Zed's.
  Bump deliberately: change both revs in the root `[workspace.dependencies]`,
  re-seed the lock from that tag, run the checks, and look at the window.
- Read GPUI's own examples at the pinned tag for API questions; docs.rs lags.
- Window blur is the compositor's call. On Hyprland, blur must be enabled
  (`decoration:blur:enabled`) or set per window for class `deck`.
- On macOS there is no compositor rule to write: GPUI draws
  `WindowBackgroundAppearance::Blurred` itself, as an `NSVisualEffectView`
  behind the scene. deck's base is 90–95% opaque, so it reads as near-black
  glass rather than a visible blur. macOS also draws the standard title bar
  with traffic lights; deck has no custom window chrome there.
- GPUI has no per-element backdrop blur, so the only real blur is the
  window's. Panes are translucent white layers over a near-black base;
  anything that floats over other content (the open rail) is opaque.
- Look: dark neutral glass, rounded panes, colour only where the eye should
  land (state dots, the rail marker, the selected session, send). Motion
  is springs everywhere, with one tuning (`spring.rs`).
