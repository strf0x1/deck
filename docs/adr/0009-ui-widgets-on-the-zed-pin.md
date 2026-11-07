# 0009 — UI widgets are built on the Zed pin, not taken from gpui-kit

Status: accepted, 2026-11-07.

## Context

The UI polish work (`docs/ui-polish.md`) needs widgets deck does not have:
a real multi-line text input, markdown rendering, diff previews, a
virtualized transcript and syntax highlighting. `longbridge/gpui-kit`
(v0.9.0, Apache-2.0) ships all of them.

It cannot be dropped in. gpui-kit depends on
`gpui = { package = "gpui-pre", version = "=0.3.11" }` from crates.io, an
exact pin. Deck depends on Zed's own `gpui` at `v0.233.10` (rev
`350f3387…`) from git. Cargo treats these as two different crates, so a
gpui-kit element cannot be a child of a deck window. gpui-kit also asks to
own the window: `gpui_kit::init`, `gpui_kit::open_window` mounting its
`Root`, and its own `Theme`, which competes with `ui/theme.rs` as the one
source of colours.

The options were: move deck to `gpui-pre` and adopt gpui-kit whole, or keep
the Zed pin and build the few widgets deck needs.

## Decision

- Deck keeps the Zed git pin for `gpui` and `gpui_platform`. It does not
  depend on `gpui-kit`, `gpui-component`, `gpui-base` or `gpui-pre`.
- Widgets are built in `crates/deck/src/ui/`, starting from GPUI's own
  examples at the pinned tag (the text input from `examples/input.rs`).
  Code ported from gpui-kit keeps its Apache-2.0 notice at the top of the
  file and names the gpui-kit version it came from.
- Parsing, diffing and highlighting use crates with no GPUI dependency
  (`pulldown-cmark`, `similar`, `syntect` with `two-face`), so that logic
  is pure state with unit tests, per convention 5.
- Colours, type sizes and spacing come only from `ui/theme.rs`.

## Consequences

- One GPUI, bumped deliberately as AGENTS.md already says; no second pin to
  keep in step with someone else's snapshot schedule.
- Deck owns its input handling, IME and accessibility bugs. The text input
  is the largest single piece of work this rules in.
- No dock, data table, command palette or dialog system for free. When deck
  needs several of those at once, revisit: re-measure what a move to
  `gpui-pre` breaks, and whether gpui-kit's root can coexist with deck's
  theme.

## Evidence

- `longbridge/gpui-kit` `Cargo.toml` and `crates/component/Cargo.toml` on
  `main`, read 2026-11-07 at release v0.9.0 (2026-11-02): the exact
  `gpui-pre =0.3.11` pin, with a comment that a caret requirement let a
  newer snapshot break gpui-component; the README's `open_window` "always
  mounts a Base `Root`".
- Deck root `Cargo.toml`: `gpui` at rev
  `350f33871e84d5ed83a403523e9490b9bf014a94` (Zed v0.233.10).
- No spike was run: the type split decides it without one.
