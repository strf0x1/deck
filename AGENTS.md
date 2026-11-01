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
| `src/ui/root.rs` | The one view entity: owns `Board`, `Session`, the client; routes keys and mouse; steps animation. |
| `src/ui/panels.rs` | Render helpers for header, lanes, session panel. No state. |
| `src/ui/session.rs`, `board.rs`, `spring.rs` | Pure state with unit tests: transcript folding, drag/reorder, spring physics. |
| `src/ui/shape.rs`, `theme.rs` | Angled panels (canvas paths) and colour tokens. Colours come only from `theme.rs`. |
| `src/bin/deck-fake-agent.rs` | Scripted ACP agent: streams, calls a tool, asks permission. For tests and for driving the UI without a model. |
| `tests/acp.rs` | Client against the fake agent as a real subprocess. |
| `tests/view.rs` | The view under GPUI's headless test platform: keystrokes, mouse drags, real layout. |

## Run it

```sh
cargo run --bin deck -- target/debug/deck-fake-agent    # no model, permission dialog included
cargo run --bin deck                                     # construct acp in the current dir
```

construct without spending tokens: start its scripted model (see
construct's AGENTS.md, `scripts/mock.sh`), then
`deck -- env CONSTRUCT_HOME=… CONSTRUCT_BASE_URL=http://127.0.0.1:$port/v1 OPENAI_API_KEY=x construct acp`.

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
- GPUI has no per-element backdrop blur and its shadows are rounded
  rectangles, so angled panels get edges and accent ticks, not glow.
