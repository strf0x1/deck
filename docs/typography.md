# Typography and live font trials

Open the sidebar's Settings, then Appearance. Interface and code fonts can be tried independently; changes apply immediately to this window and last until it closes. Close or Escape returns to the conversation without changing the draft. Providers and keys returns to the existing setup pane.

Cmd or Ctrl with plus, equals, or Shift+equals increases text size. Minus decreases it; zero resets size without changing the chosen fonts. The range is 80% through 140% in 10% steps. Resizing also works during approvals and while editing provider or secret fields. It never inserts characters or answers an approval.

## Fonts and hierarchy

The default is Adwaita Sans for interface and prose, with JetBrainsMono Nerd Font for technical text and icons. Appearance also offers the system UI face, plain JetBrains Mono, Geist Nerd, 0xProto Nerd, Obel Mono, and Obel Mono Masked. Missing choices are dimmed and identify GPUI's fallback family. JetBrains Mono, JetBrainsMono Nerd Font, and both Obel builds load from bundled bytes, without installing system fonts. Sources, versions, licenses and checksums are in `crates/deck/assets/fonts/README.md`.

`theme::size` owns the hierarchy. At 100%, UI labels are 14 logical pixels, prose and the composer 15, code and technical detail 13, small hints 11, and headings 17 or 21. Every text style uses `size::scaled` rem values. The root sets the rem base once per render; geometry and icon sizes stay fixed. Text controls use minimum heights where content can grow. The sidebar's drag coordinates stay unchanged.

The selected interface family also reaches Markdown's styled text runs. The selected code family reaches fenced Markdown, command headers, expanded commands, output and diffs. Inline code and editable-value specimens use the regular build when Obel Masked is selected. Settings fields retain their own regular mono font, and secret entry keeps its existing explicit masking.

## Masked code

Obel Mono Masked is an explicit visual choice. Its contextual glyph substitutions hide recognized key patterns; the source and copied text retain their original characters. Preview keys are synthetic.

GPUI splits shaping at syntax-colour changes. Masked code therefore uses one plain-colour text run, which lets substitutions read a key across those boundaries. Other code fonts keep syntax highlighting. The font does not replace credential handling or redact saved transcripts.

## State and measurement

`Deck` owns plain `Typography` state and the active Settings subsection. `appearance_ops.rs` is the one path for font or scale changes. Each change calls `ListState::remeasure`, which preserves the virtual transcript's proportional scroll position while invalidating measured row heights. The composer measures the current inherited rem size and leading when it lays out text, selections, cursor and IME bounds.

Appearance uses the current Settings pane rather than a separate navigation destination. Explicit provider/model/secret navigation returns to Providers and keys. An open form always takes precedence over Appearance.

`fonts.rs` registers application-local bytes and reports the family GPUI selects using `resolve_font` and `get_font_for_id`. The report names a selected family, not an OS PostScript face. Font enumeration alone is insufficient because GPUI adds uninstalled fallback names to its list. Headless GPUI uses a no-op text system, so native selection and shaping require a real window.

## Verification

Run the repository checks after building the helper binaries:

```sh
cargo build --workspace --bins
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
scripts/acp-contract.sh verify
python3 scripts/check-deckd.py
```

View tests cover both shortcut families, plus and Shift+equals, reset, approval routing, draft retention, actual Markdown growth, Appearance at 760 by 480 and 140%, provider navigation after a font trial, secret-field contents across resize keys, and fetched-model row bounds at 140%.

The native macOS window on current main was checked with the fake agent. All six candidate code families resolved exactly on the development machine. Obel Masked selected a regular value face, resizing reached 140%, reset returned to 100%, and JetBrainsMono Nerd Font was restored. Linux font rendering needs a Linux run; CI checks compilation, lint and headless behavior on both platforms.

The dev control socket includes selected typography and GPUI family reports in `state`. It also accepts `{"cmd":"scroll","x":950,"y":500,"delta":{"x":0,"y":-300}}` for inspecting long native font comparisons. Coordinates and deltas are logical pixels dispatched as GPUI scroll input.
