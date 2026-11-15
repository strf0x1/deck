# 0013 — Transcript selection is held as text positions, not pixels

Status: accepted, 2026-11-14.

## Context

The transcript had no text selection: copying meant a hover button that
copies a whole message, path or command. gpui-kit v0.9.1 has window text
selection in `gpui-base` (`text_selection.rs`, `selectable_text.rs`), and
ADR 0009 lets deck port such code onto the Zed pin.

gpui-base anchors each end of a selection at a pixel point inside an
element that registered itself while painting, and copy asks every live
element for its text. Deck's transcript is a virtualized `list`
(`transcript_list.rs`): a row scrolled out of view has no element, so it
can neither hold an endpoint nor answer for its text. gpui-base works around
that with participant "content keys" and fallback copy text, layered on the
geometry model.

## Decision

- A selection endpoint is a position in the transcript's text:
  `(row key, run, byte offset)`. The row key is `transcript::Row::key`; a
  run is one piece of text the row draws (a markdown paragraph, a code
  block, a command, an output body, one diff line).
- `ui/runs.rs` lists a row's runs and their text, purely from the session.
  The renderers number the runs they draw in the same order, and a view test
  checks that what was drawn equals what `runs.rs` lists. Copy reads
  `runs.rs`, so rows that are not drawn copy correctly.
- Only text that is shown is a run: a folded card's hidden lines, diff
  gutters (line numbers and signs), card headers and chips are not.
- Pixels exist only for one frame: each drawn run records its text layout
  and bounds in a hit map while it prepaints, mouse events turn window
  points into positions through it, and the highlight is drawn as the
  selected range's run background.
- The leaf logic is ported from gpui-base with its notice (word and line
  boundaries); its window layer, scopes, touch handles and per-element
  registration are not.

## Consequences

- A selection survives scrolling, streaming into the selected reply, and
  cards opening elsewhere; copy never depends on what is on screen.
- A new transcript text surface is selectable only once it is a run in
  `runs.rs` and drawn through `ui/selectable.rs`; the consistency test
  fails when the two drift.
- Positions are only as stable as row keys. Replacing the entries (a
  history load, switching task) clears the selection.
- Selection stays inside the transcript: the sidebar, panels and composer
  are not part of it (the composer keeps its own).

## Evidence

- gpui-base 0.9.1 `src/text_selection.rs`: `TextSelectionEndpoint` holds an
  entity id and a participant-relative point; `TextSelectionContentKey` is
  the "stable participant-defined identity for virtualized participant
  content". Read from the crates.io source, 2026-11-14.
- Zed `v0.233.10` (rev `350f3387…`) `gpui/src/elements/text.rs`:
  `StyledText::layout`, `TextLayout::index_for_position`; no
  `TouchDragEvent` (as for the scrollbar port).
