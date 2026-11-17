# 0014 — A prompt carries a key, and a restart resends it

Status: accepted, 2026-11-17. Adopts contract 1.3.0 §12
(strf0x1/sprawlcommons-acp `v1.3.0`); Construct's side is its ADR 0045.

## Context

Deck sends `session/prompt` and, under live attach, follows the turn through
turn notifications instead of the prompt's response. When the agent, its VM
or the connection dies after a prompt is sent but before its turn shows, the
prompt is simply lost. Nothing resends it, because resending blind could run
a prompt the agent had already taken a second time, with its effects twice.

Contract 1.3.0 gives `session/prompt` an optional
`_meta["com.sprawlcommons/construct"].idempotencyKey` (ACP v2's name and
rules on v1). An agent that advertises `idempotentPrompts` records the key
with the prompt before the turn starts, and answers a resend from that record
without starting a second turn.

## Decision

- **One key per submission.** Every prompt the window sends (the composer's,
  and a new task's first) gets a fresh random version-4 UUID from
  `deck_acp::key::new_key`. `Conn::prompt` puts it on the wire only when the
  agent advertised `idempotentPrompts` under the current Construct key.
- **Kept until answered.** The task's `Attached` holds the keyed prompt
  (`in_flight`) until `Inbound::PromptSettled` reports the agent's answer or
  refusal for that key. It outlives the connection, which a restart replaces.
- **A restart resends it.** When a rebuilt connection reaches `Ready` (after
  `session/load` has replayed history), the window resends the in-flight
  prompt with the same key. If the replay already shows the prompt, the agent
  had taken it, and its answer only settles the key. If not, the agent never
  took it: the transcript shows it again and the turn runs once.
- **Never to an agent without the capability.** A 1.2 agent ignores the key
  and would run a resend again, so nothing is resent to it (§12.5); that
  prompt stays lost as before, and the user can send it again.

## Consequences

- A crash, `kill -9` or a dead VM between sending a prompt and its answer no
  longer loses the prompt or risks running it twice, once the agent speaks
  1.3.
- The key lives in window memory only. A window that quits while a prompt is
  in flight, at the same time as the agent dies, loses it as before.
  Persisting it in the task record would close that gap; nothing asks for it
  yet.
- The fake agent advertises and honours keys like Construct 1.3, with the
  reverse-DNS names, and logs every prompt it receives with
  `DECK_FAKE_PROMPT_LOG`.

## Evidence

- `crates/deck/tests/view.rs`
  `a_prompt_cut_off_by_a_killed_agent_is_resent_with_its_key_and_runs_once`:
  the first prompt of a fake-agent task, `kill -9` while its turn waits on a
  permission, restart. The agent receives the same prompt and key twice, its
  journal holds one turn, the transcript shows the prompt once, and nothing
  asks again. With the resend switched off, the agent receives the prompt
  once; with the fake's dedupe switched off, the resend starts a second turn.
- `crates/deck-acp/tests/acp.rs`
  `a_prompt_resent_with_its_key_after_a_kill_starts_no_second_turn`: the
  same across fake processes at the protocol level, plus a refused conflict
  and a new key that runs.
- `conn.rs` unit tests: the key is sent only to an agent that advertised the
  capability (not under the bare key, not as a string), and a keyed prompt is
  settled by its answer or its refusal. `attached.rs`: the resend decision.
- `crates/deck-acp/tests/contract.rs` pins the names to the vendored 1.3.0
  matrix.
