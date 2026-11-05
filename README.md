# deck

A native ACP client for directing agents, starting with
[construct](../construct). Part of sprawlcommons.

Spike status: one live agent session over ACP (streamed text, tool calls,
permission questions answered in the UI), daemon-owned tasks that survive closing
Deck, and live reattachment to Construct's running turns. The lane board has
spring-animated drag and angled glass panels; the other lanes are placeholders.
See [the daemon guide](docs/deckd.md) for lifetime and verification details.
See `PLAN.md` for direction and `AGENTS.md` to work on it.

```sh
cargo build && cargo run --bin deck -- target/debug/deck-fake-agent   # try it without a model
```

## License

[MIT](LICENSE) © 2026 Brandon Bosch.
