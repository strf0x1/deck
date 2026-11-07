# UI polish

The window works end to end: a Construct agent runs, asks, edits and
publishes a PR. It still looks and reads like the spike it started as. This
is the plan for making a session pleasant to read and to drive before more
features land. Widgets are built on the Zed pin (ADR 0009).

## Where it stands

- **The composer is not a text input.** `Deck::draft` is a `String` drawn
  one line high and truncated (`ui/conversation.rs`, `prompt`). No
  selection, multi-line text, paste, IME, caret movement or history.
- **Agent text is plain text.** No markdown, no code blocks.
- **Tool calls are reduced to a title and a status chip.** `Inbound::Tool`
  keeps `{id, title, status}`. Construct already sends `kind`, `rawInput`
  and the tool's output text (`construct/src/acp/map.rs`); deck drops them.
  Construct does not yet send ACP's `diff` content or `locations`.
- **The transcript is one scrolling `div`.** Every entry is laid out every
  frame, and nothing follows new output or says there is more below.

## Principles

What makes a dense session window readable without being loud:

1. **Prose has no box; machine work is boxed.** You can tell the agent's
   words from its actions without reading either.
2. **Monospace only for machine output:** paths, commands, diffs, output.
3. **Counts first, content on request.** `+12 −3`, `read 4 files`,
   `14 more lines ⌄`.
4. **One accent per state**, as `ui/theme.rs` already spends colour.
5. **Rhythm from tokens.** Type sizes, spacing and radii live in
   `theme.rs` next to the colours, so density is tuned in one place.

## Phases

Each phase is shippable on its own and gets `crates/deck/tests/view.rs`
cases for what it changes (protocol work gets `conn.rs` tests and an
`acp.rs` step).

### 0 — Keep the data deck already receives

No visible change; everything later depends on it.

- `deck-acp`: decode a tool call's `kind`, `rawInput`, `content` (text and
  `diff`) and `locations` into `Inbound::Tool` / `Inbound::ToolStatus`.
- Construct: send ACP `diff` content (`path`, `oldText`, `newText`) and
  `locations` for its edit and write tools. Both are base ACP, not one of
  our extensions, so no contract change is needed.
- `deck-fake-agent` sends the same shapes, so the UI can be driven without
  a model.
- `ui/session.rs`: `Entry::Tool` carries those fields plus start and end
  times, for durations.

### 1 — A real composer

- A multi-line text input on GPUI's `EntityInputHandler`, starting from
  `examples/input.rs` at the pinned tag: selection, clipboard, IME,
  shift-enter for a newline, grows to a few lines then scrolls, up arrow
  recalls earlier prompts.
- A status row under it: agent and model, working directory, branch, task
  state, and cost or context use when the agent reports them.

Landed: the field (`ui/editor.rs`, `composer.rs`, `input.rs`) and a status
row with agent, backend, directory, branch and the task's token count.
The model name and cost wait on the agent reporting them.

### 2 — A transcript you can skim

- Agent messages rendered from markdown (`pulldown-cmark` to a pure
  block/span tree, then to elements), with code blocks in rounded mono
  boxes.
- Tool cards chosen by `kind`:
  - **edit**: path, `+n −m`, a 6–8 line diff preview that expands
    (`similar` for hunks).
  - **execute**: the command, the tail of its output, exit status and
    duration; the rest collapsed behind `N more lines`.
  - **read / search**: small path chips; consecutive ones group into one
    row ("read 4 files").
  - anything else: today's chip.
- Thoughts collapsed to one faint line that expands.
- The transcript on GPUI's `list()` (`ListState`): only visible rows are
  laid out, it follows new output while you are at the bottom, and a
  "jump to latest" pill appears when you have scrolled up.

Landed: `ui/markdown.rs`, `diff.rs`, `transcript.rs` (pure, tested),
`prose.rs`, `cards.rs`, `transcript_list.rs`. `similar` was not needed: a
bounded LCS over the trimmed middle covers agent edits. An edit with no
ACP `diff` is previewed from its arguments (Construct's `edit` and
`write`), without file line numbers.

### 3 — Density without overload

- A per-turn footer: files changed with total `+/−`, each linking to its
  edit card.
- Type, spacing and radius tokens in `theme.rs`; views stop using literal
  `px()` sizes.
- Hover actions: copy a message, copy a command, open a file.

Landed: turn footers (`ui/turn_footer.rs`), hover copy buttons
(`ui/copy.rs`; copying a path stands in for opening a file, since a VM
task's paths are not the host's), and `size`, `radius` and `space` tokens.
Every text size and radius comes from a token; spacing tokens cover the
transcript and cards, and the other panes still use literal padding.

### 4 — Finish

- Syntax highlighting in diffs and code blocks (`syntect` + `two-face`,
  pure: text in, styled spans out).
- Keyboard navigation between tool cards; expand and collapse with keys.
- Springs (`spring.rs`) for expanding cards and the jump pill.

## Not in scope

Docking, split panes, a command palette and themes other than the one
dark glass. ADR 0009 says when to revisit gpui-kit for those.
