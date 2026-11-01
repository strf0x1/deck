# Working on deck

deck is a native ACP client in Rust on GPUI (Zed's UI framework): it launches
an agent (construct by default) as a subprocess and shows its session, tool
calls and permission questions in a dark-glass window. This is the UI spike;
`PLAN.md` is the product discussion and is not an implementation plan.

## Layout

| Path | What |
|---|---|
| `src/main.rs` | Argument split only: `deck [--] [agent argv…]`, default `construct acp`. |
| `src/app.rs` | Opens the window (blurred background, `app_id = "deck"`). |
| `src/acp/` | The ACP client. `rpc.rs` line framing (mirrors construct's), `conn.rs` the pure protocol state machine, `process.rs` the subprocess and reader thread. The UI only sees `Inbound`. |
| `src/ui/root.rs` | The one view entity: owns `Board`, `Rail`, `Session`, the client; routes keys and mouse; steps animation. |
| `src/ui/chrome.rs`, `panels.rs`, `conversation.rs` | Render helpers: rail and header; sessions column; transcript, permission card, prompt. No state. |
| `src/ui/session.rs`, `board.rs`, `rail.rs`, `spring.rs` | Pure state with unit tests: transcript folding, drag/reorder, rail open/close, spring physics. |
| `src/ui/theme.rs` | Colour tokens, fonts (Adwaita Sans for UI, JetBrainsMono Nerd Font for code and icons), icon glyphs. Colours come only from here. |
| `src/control/` | Dev-only control socket (`DECK_CONTROL`). `protocol.rs` the pure line grammar, `socket.rs` the listener threads, `drive.rs` dispatches commands as GPUI input on the window and reads `state`. |
| `src/bin/deck-fake-agent.rs` | Scripted ACP agent: streams, calls a tool, asks permission. For tests and for driving the UI without a model. |
| `tests/acp.rs` | Client against the fake agent as a real subprocess. |
| `tests/view.rs` | The view under GPUI's headless test platform: keystrokes, mouse drags, real layout. |
| `tests/control.rs` | Control commands run against a headless window (no compositor in CI). |

## Run it

```sh
cargo run --bin deck -- target/debug/deck-fake-agent    # no model, permission dialog included
cargo run --bin deck                                     # construct acp in the current dir
```

construct without spending tokens: start its scripted model (see
construct's AGENTS.md, `scripts/mock.sh`), then
`deck -- env CONSTRUCT_HOME=… CONSTRUCT_BASE_URL=http://127.0.0.1:$port/v1 OPENAI_API_KEY=x construct acp`.

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

## Checks

```sh
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
```

All three must be clean; CI (`.github/workflows/ci.yml`) runs the same on
Linux, and a non-blocking macOS job reports whether the Mac build passes. A UI behaviour change gets a `tests/view.rs` case
(simulate the input, assert on view state); a protocol change gets a
`conn.rs` unit test and, if it crosses the process, a `tests/acp.rs` step.

## Conventions

Same as construct's (see `../construct/AGENTS.md`), in short:

1. `main.rs` only parses arguments; logic lives in the lib.
2. Every module starts with a `//!` paragraph: what it is for and the one design choice a reader must know.
3. One concern per file; soft ceiling 300 non-test lines.
4. No `unwrap`/`expect` outside tests; escape hatches are `#[expect(..., reason)]`, never `#[allow]`.
5. Pure state (no GPUI types) wherever possible, so it is unit-testable; views read it.

## GPUI notes

- Pinned to Zed `v0.233.10` (rev `350f3387…`) as a git dependency, with Zed's
  `[patch.crates-io]` entries mirrored and `Cargo.lock` seeded from Zed's.
  Bump deliberately: change the rev in all three places, re-seed the lock
  from that tag, run the checks, and look at the window.
- Read GPUI's own examples at the pinned tag for API questions; docs.rs lags.
- Window blur is the compositor's call. On Hyprland, blur must be enabled
  (`decoration:blur:enabled`) or set per window for class `deck`.
- GPUI has no per-element backdrop blur, so the only real blur is the
  window's. Panes are translucent white layers over a near-black base;
  anything that floats over other content (the open rail) is opaque.
- Look: dark neutral glass, rounded panes, colour only where the eye should
  land (state dots, the rail marker, the selected session, send). Motion
  is springs everywhere, with one tuning (`spring.rs`).
