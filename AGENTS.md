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
| `crates/deck-acp/` | The ACP client, no UI. `rpc.rs` line framing (mirrors construct's), `conn.rs` the pure protocol state machine, `inbound.rs` typed events and payload decoding, `tool.rs` a tool call's kind, input, content (text, diffs) and locations, with updates as patches, `shutdown.rs` the bounded close-only exchange, `client.rs` the connection over any reader/writer pair and its reader thread. The UI only sees `Inbound`. |
| `crates/deck-runtime/` | Tasks, no UI (ADR 0001). `lifecycle.rs` the pure state machine folded from lifecycle events, `record.rs` task records under the data root (`events.jsonl`, `task.json`, the liveness `lock`), `github.rs` the project's GitHub policy resolved at create (origin, `.deck/project.toml`, the dev override), `backend.rs` the backend contract, `local.rs` the backend that runs the agent as a child, `runtime.rs` create/start/attach/stop/resume/destroy and the notice stream. `approvals.rs` the host approval queue: pending items, once/task/project grants, attachment leases and the bounded hold (ADR 0004 §5). `settings.rs` the named providers, models and default (names, never values), validated and resolved into a task's choice; `settings/file.rs` keeps them in `<data>/settings.json` with a revision per save (ADR 0012). `credentials/` the one credential store (a 0600 `DECK_CREDENTIALS` file, or Keychain / Secret Service), the bounded read, revoked fingerprints and the `deckd credential` commands; `broker.rs` who may fetch what and the revocation push (ADR 0004 §4). `catalog.rs` a provider's `/models` fetch, parsed entries, and the per-URL cache under the data root (ADR 0012 §4). |
|`crates/deck-runtime/src/daemon.rs`, `daemon_client.rs`, `daemon_cli.rs`, `relay.rs`|The daemon authority, its control client, and the JSON-agnostic line relay. Every connection starts with a `hello` handshake that refuses a stale or other-protocol daemon; the client reconnects after a restart and a watch re-leases the task. `daemon_cli.rs` is `deck daemon status \| stop \| restart`. `holder.rs` owns each task independently; `detached.rs` launches and adopts it. `child.rs` ties agent process groups to holder lifetime. See `docs/deckd.md`.|
|`crates/deck-runtime/src/vm.rs`, `vmm.rs`, `krun.rs`|The libkrun backend: `vm.rs` is the `Backend`/`Instance`, `vmm.rs` is the detached holder (`deckd --vmm`) that owns the microVM, and `krun.rs` loads the pinned 1.x C API. The guest opens the ACP stream out to the VMM's forward listener once its workspace is ready, and `deck-egress --acp` then becomes the agent with that stream as stdio, so the connection is both readiness and the stream; there is no polling or fixed delay. The VMM starts the task's egress gateway, maps its sockets, and disables libkrun's implicit vsock, so the guest has no IP route (ADR 0004). Decisions and measurements: `docs/adr/0003-vm-backend.md`, `docs/adr/0004-host-egress-and-credentials.md`, `docs/vm-spike.md`.|
|`crates/deck-gateway/`|The host-side egress gateway: a pure policy core, a bounded HTTP/1.1 carrier, the OpenAI-compatible model gateway, the daemon approval-queue client, and the extension port's `boundary` kind (`/events`, long-polled `/approvals/<id>`, agent withdrawal). `github/` is the publication gateway (ADR 0007): a closed operation set on its own vsock port (1029), a git-bundle publish with host-side ref rules, and `github_profile` project rules. It listens only on Unix sockets the VMM maps from guest vsock ports, strips guest credential headers, and injects a host value it fetches from `deckd` per operation (`credential.rs`, which also closes operations whose value is revoked); a model the task was not configured with asks `deckd` and reports the approval id in a bounded 403. Its binary is named by `DECK_GATEWAY_EXECUTABLE`, defaulting beside `deckd`.|
|`crates/deck-egress/`|The guest relay: loopback TCP to a host vsock port, with `--kind` naming the gateway before the stream for the extension port. Built for the guest image by `scripts/vm-image.sh`; it is a byte carrier, so bypassing it does not widen the boundary. `--acp` is the agent's transport: connect the host ACP port, then become the agent with that stream as stdin and stdout (the connection is readiness; no separate socat). The host build exists so the workspace checks cover it.|
|`crates/deck-runtime/tests/vm.rs`|The same lifecycle contract against a real guest; skipped unless `DECK_VM_*` names an image, journal, libkrun and a `deckd` that can run `--vmm` (signed on macOS). The ignored 20-launch test prints the launch → ACP-ready stages.|
|`scripts/vm-image.sh`, `vm-sign.sh`|Build the hand-made Debian guest image for the host architecture with Docker (its `deck-guest` entry connects ACP out to the host and marks boot stages from `/proc/uptime`), and codesign a `deckd` with the Hypervisor entitlement (macOS).|
| `crates/deck-runtime/tests/daemon.rs` | The real daemon binary: locking, idle shutdown, detached survival, holder adoption after daemon death, holder-crash cleanup, and concurrent control clients. |
| `crates/deck-runtime/tests/lifecycle.rs` | The contract against real processes: stop with SIGTERM then SIGKILL, unclean exits, the agent's children, reconciliation after a holder dies, reuse, destroy. |
| `crates/deck-acp/src/bin/deck-fake-agent.rs` | Scripted ACP agent: streams, reads and edits a file (ACP `locations` and `diff` content), calls a tool, asks permission. For tests and for driving the UI without a model. `--listen <socket>` serves a Unix socket instead of stdio. Supports live attach, `session/load`, `resume` and `close` like construct; with `DECK_BOUNDARY_URL` set it advertises both the reverse-DNS and bare boundary keys and puts a late decision in its transcript. `DECK_FAKE_STATE=<dir>` persists sessions across processes; `DECK_FAKE_NAMES=legacy|reverse` pins it to one extension namespace like a particular Construct release. Keep it in step with what Deck relies on from construct (ADR 0002). |
| `crates/deck-acp/tests/acp.rs` | Client against the fake agent over stdio, and over a Unix socket the client did not launch; the fake's sessions across two processes. |
| `crates/deck/src/main.rs` | Argument split only: `deck [--] [agent argv…]`, default `construct acp`, or `deck daemon status\|stop\|restart` (which `deck -- daemon …` escapes). `daemon.rs` runs the daemon commands against the selected data root. |
| `crates/deck/src/app.rs` | Opens the window (blurred background, `app_id = "deck"`); app keys: `cmd-q` (macOS) / `ctrl-q` quits, and closing the last window quits. |
| `crates/deck/src/task.rs` | The window's daemon connection: finds or starts a task, live-attaches when supported, and explicitly restarts unsupported detached tasks. Dropping it detaches. |
| `crates/deck/src/ui/root.rs` | The one view entity: owns `Board`, `Rail`, `projects` (the open project and the sidebar's filter), the selected task's transcript and composer, and one `ui/attached.rs` per task it is connected to; routes keys and mouse; steps animation. `ui/attachment.rs` connects, restarts and switches tasks and projects, opens a project from the launch checkout or the folder picker, creates a project's first task from its first prompt, and at launch attaches every other running task with a recorded session; switching moves the selection and never drops the previous connection (ADR 0010 §1, §3, §5, §8). |
| `crates/deck/src/ui/chrome.rs`, `panels.rs`, `conversation.rs`, `project_view.rs`, `prompt.rs` | Render helpers: the header across the transcript (the selected task's title, its project's faint chip and the agent's status, then the agent pill, publication and error chips, New session) and shared glyphs, state colours and panes; `header_title.rs` is the plain value the header and the control socket read the selected task, project and status from; the transcript, permission card and the no-project empty state; the project view (its header, its unsettled tasks as cards, the agent and Rules and grants footer), drawn when a project is selected and no task is; the prompt box and its status row (agent, backend, directory, branch, tokens from `deckd`'s task record). No state. |
| `crates/deck/src/ui/sidebar.rs`, `sidebar_view.rs`, `rail.rs`, `attention.rs`, `attention_state.rs` | The one sidebar (ADR 0010 §1, §7, §9). `sidebar.rs` is pure: the open rows ("Needs you", each project, the selected project's tasks, the settled fold) and the narrow glyphs; `sidebar_view.rs` draws both, the compact task rows and the fold, floating over the transcript or taking its reserved width when pinned; `rail.rs` is its open/close state. `attention.rs` is pure: one reason per task (approval, permission question, unseen finished turn, lifecycle failure), oldest-first items with the `since` the window first saw them, and when a task settled (destroy, Mark done, merged pull request); `attention_state.rs` folds the items, records seen turns through `deckd` at most once per (task, turn), selects a task from an item, and Marks done or clears the mark. |
| `crates/deck/src/ui/turn_footer.rs`, `copy.rs` | Under a finished turn that changed files, its files with `+n −m` (a chip opens that file's last edit card); copy buttons that show while a row is hovered (message text, a command, an edit's path). |
| `crates/deck/src/ui/transcript_list.rs`, `cards.rs`, `card_body.rs`, `folds.rs`, `prose.rs` | The transcript: a virtualized GPUI `list` that follows new output until scrolled up (then a jump pill), re-measuring from the first row whose signature changed; `cards.rs` draws rows (tool cards by kind: edit diffs with `+n −m`, a command's output tail, folded reads, folded thoughts; click a header to expand); `card_body.rs` the diff and output bodies, `folds.rs` folded reads and thoughts, `prose.rs` agent markdown with highlighted code. Keys: alt-up/down select a card, tab opens it, escape lets go. |
| `crates/deck/src/ui/composer.rs`, `input.rs` | The composer field: a GPUI element that wraps, paints and scrolls the editor's text and keeps what it drew for hit tests; `input.rs` the keys that edit without typing and the platform input handler (typed and IME text arrive there, after the key handler declines the key). |
| `crates/deck/src/ui/session.rs`, `editor.rs`, `markdown.rs`, `diff.rs`, `highlight.rs`, `motion.rs`, `transcript.rs`, `board.rs`, `rail.rs`, `projects.rs`, `boundary.rs`, `access.rs`, `spring.rs` | Pure state with unit tests: transcript folding, agent markdown as blocks, bounded line diffs with previews, syntax classes from syntect (bundled syntaxes, coloured by `theme.rs`, cached), card fade-in and jump-pill springs, transcript rows and what each tool card shows, the composer's text, selection, IME marked range and prompt recall, drag/reorder, rail open/close and its active project, the project list, selection and task resolution, the boundary card's pending items, the access panel's credential listing and revoke confirmation, spring physics. |
| `crates/deck/src/ui/access_panel.rs`, `access_grants.rs`, `credentials.rs` | The access panel (header shield button): credentials, task grants and saved project rules, plus pending requests that return to the boundary card. Daemon calls (list, revoke) run off the UI thread; grant listings refresh while open. The panel changes from daemon replies and rejects stale listings. |
| `crates/deck/src/ui/settings.rs`, `settings_rows.rs`, `settings_ops.rs`, `settings_panel.rs`, `settings_form.rs`, `model_menu.rs` | The settings panel (ADR 0012 §4; the sidebar foot's gear): providers, the default model and stored secrets. `settings.rs` and `settings_rows.rs` are pure: the snapshot from `deckd`, one form at a time checked with the runtime's own `Settings::validate`, a stale save that reloads, and a secret field emptied the moment it is submitted (never kept as editor history); `settings_ops.rs` makes the daemon calls off the UI thread and routes the panel's keys. While a form is open, typed text goes to its focused field through the composer's own element and input handler (`input_editor`), masked for a secret. `model_menu.rs` is the header chip's menu: what the next New session starts on (a selector in its create spec), plus Manage models…; `model_search.rs` filters a provider's fetched list (tool-capable only, every word, prefix first) for the form's discovery block, whose clicks toggle ids in the Models text. |
| `crates/deck/src/ui/tasks.rs`, `publication.rs`, `publishing.rs`, `publication_panel.rs` | The sidebar's task rows from `deckd`'s task list, filtered to the selected project (the window's own task is the live row; clicking another row switches the window to it; New session, in the header, asks `deckd` to `create` a task with the selected project's command and first checkout and switches to it; `new_session.rs` is the button state), each task's publication chip and the read-only publication panel (ADR 0008 §8). `tasks.rs` and `publication.rs` are pure state with unit tests; `publishing.rs` refreshes tasks, `pull_state` and the pending approvals of tasks the window does not watch off the UI thread every 15 s while the window is open, folds settled tasks out of the column after 14 days, and keeps a settled task's lane so its chip still opens the publication panel; destroy is offered only after a merge, behind a second click. |
| `crates/deck/src/ui/theme.rs` | Colour tokens, type sizes (`size`), radii (`radius`) and transcript spacing (`space`) by role, fonts (Adwaita Sans for UI, JetBrains Mono for code), icon names (`icon`, asset paths into `assets/icons`), `glow`. Colours, text sizes and radii come only from here. Each hue means one thing: purple the user, light teal the agent at work, amber waiting on a human, green done or allowed, red failed or denied, lavender file changes, steel blue reads, orange the network; `Tokens::hue` gives a project its own. An edged surface keeps a dark body (`Tokens::well`) and wears its hue in text and border, not a fill; an icon mark wears its hue in the glyph alone. `panels.rs` builds the shared marks from them (`outlined`, `state_mark`, `icon_mark`, `pill`, `primary`, `eyebrow`); `ui/glyph.rs` draws an icon, tinted by the row's text colour. |
| `crates/deck/src/ui/typography.rs`, `appearance.rs`, `appearance_ops.rs`, `fonts.rs` | Window-local font and size trials under Settings → Appearance. Shared `theme::size` tokens use rems; font and scale changes remeasure the virtual transcript. Editable values keep unmasked families. `fonts.rs` loads bundled fonts and reports GPUI family fallback. |
|`crates/deck/assets/`|Embedded assets: `fonts/` the bundled TTFs, `icons/` the Material Symbols Outlined SVGs a view names through `theme::icon` (Apache-2.0; each README there records provenance, hashes and licence).|
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
With `DECK_VM_*` set, `CONSTRUCT_BASE_URL` and `CONSTRUCT_MODEL` configure the
task's host gateway instead, and the key is a stored credential, never an
environment variable: `deckd` reads it per request from one store (ADR 0004
§4) and strips the name from the VMM's and gateway's environment. The guest
gets the relay URL and a non-empty placeholder, never the key.

```sh
export DECK_CREDENTIALS=$PWD/target/credentials.json   # the file store; omit for Keychain / Secret Service
echo x | target/debug/deckd credential set OPENAI_API_KEY   # value on stdin, file written at 0600
target/debug/deckd credential list                          # names and fingerprints, never values
target/debug/deckd credential revoke OPENAI_API_KEY         # closes running requests that used it
target/debug/deckd credential retry OPENAI_API_KEY          # permits another OS read after cancellation
```

Without `DECK_CREDENTIALS`, `deckd` uses the macOS Keychain or the Secret
Service (`secret-tool`) under `com.sprawlcommons.deck`. A rebuilt, ad-hoc
signed `deckd` meets a Keychain access dialog once per item, so development
and CI use the file store. `deckd` refuses to start on a credential file with
group or other permission bits.

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
c '{"cmd":"hover_rail"}'; c '{"cmd":"select_rail","index":0}'
c '{"cmd":"settle"}'; c '{"cmd":"state"}' | jq .
```

Commands: `type {text}`; `key {key, modifiers?}` (`enter`, `escape`,
`backspace`, arrows, a single character; modifiers `ctrl`, `alt`, `shift`,
`super`); `click {x,y}`; `move {x,y}`; `drag {from, to, steps?}` (one move per
frame); `hover_rail`; `select_rail {index}`; `wait {ms}`; `settle` (until no
spring moves, at most 2 s); `state` (board order, cards with detail and
publication chip, dragging; `main` as `project`, `task` or `none`; the
header's title, project and detail;
`needs_you` items with task, project and reason;
`sidebar` rows with kind and title; open publication panel, publish notice;
new session creating/error; settings (open, providers with key state, secrets by name and fingerprint, form kind and focused field, never a field's text); rail width/active/pinned/expanded; session
status, entries, ask; draft; animating); `geometry`. Coordinates are window-relative logical pixels, as
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
matrix, payload fixtures and schemas.
ADR 0005 records the names; ADR 0006 records the pin.

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

### Reviewing a contract patch before release
`scripts/acp-contract.sh update <version> <commit>` prepares a candidate pin
from an immutable contract commit, including payload fixtures. Keep candidate
implementation PRs draft until the contract is merged and tagged, then rerun
`update <version>` in both repos and verify that their hashes match. The
release-tag form remains the default; a candidate commit is not a release.

## GPUI notes

- Pinned to Zed `v0.233.10` (rev `350f3387…`) as a git dependency, with Zed's
  `[patch.crates-io]` entries mirrored and `Cargo.lock` seeded from Zed's.
  Bump deliberately: change both revs in the root `[workspace.dependencies]`,
  re-seed the lock from that tag, run the checks, and look at the window.
- Read GPUI's own examples at the pinned tag for API questions; docs.rs lags.
- No widget kits: gpui-kit pins a different GPUI (`gpui-pre`), so widgets
  are built here on the Zed pin (ADR 0009). `docs/ui-polish.md` is the
  current UI plan.
- Window blur is the compositor's call. On Hyprland, blur must be enabled
  (`decoration:blur:enabled`) or set per window for class `deck`.
- On macOS there is no compositor rule to write: GPUI draws
  `WindowBackgroundAppearance::Blurred` itself, as an `NSVisualEffectView`
  behind the scene. deck's base is 90–95% opaque, so it reads as near-black
  glass rather than a visible blur. macOS also draws the standard title bar
  with traffic lights; deck has no custom window chrome there.
- GPUI has no per-element backdrop blur, so the only real blur is the
  window's. Panes are translucent neutral-white layers over a charcoal base;
  anything that floats over other content (the open rail) is opaque.
- Look: charcoal glass with purple accents. Neutral glass panes over a
  near-black base, and colour that always means something (the table's
  `theme.rs` row lists the hues), spent in text and borders over dark
  bodies rather than in fills; the one live thing in a region glows softly. A new surface picks its
  colour by meaning, never for variety. Motion is springs everywhere, with
  one tuning (`spring.rs`).
