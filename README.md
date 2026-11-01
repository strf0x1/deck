# deck

A native ACP client for directing agents, starting with
[construct](../construct). Part of sprawlcommons.

Spike status: one live agent session over ACP (streamed text, tool calls,
permission questions answered in the UI), a lane board with spring-animated
drag, angled glass panels. The other lanes are placeholders until the host
runtime exists. See `PLAN.md` for direction and `AGENTS.md` to work on it.

```sh
cargo build && cargo run --bin deck -- target/debug/deck-fake-agent   # try it without a model
```
