# 0002 — Construct owns live reattach; the holder only frames lines

Status: accepted, 2026-11-02. Supersedes ADR 0001 Decision 5 (attach and
reattach), and adds an owner to Decision 7's turn checkpoints. Reviewed
with a second model before acceptance; its findings are folded in below.

## Context

ADR 0001 left one case open. When Deck closes and reopens while the agent
keeps running, the agent's stream is the same one, already initialized, and
Construct refuses `session/load` for a session already loaded on that
connection. ADR 0001 planned a buffer in the holder (the handshake,
unanswered asks, a bounded window of updates) to bridge it.

That buffer cannot rebuild a freshly opened Deck: a bounded window does not
hold the whole conversation, so the holder would need a complete transcript
or a truncation policy, and it would become a second ACP client and session
state implementation beside Construct's journal. Construct-first (PLAN.md)
makes the alternative acceptable: Construct already owns the journal, tool
state and permission waiters, so it reconstructs a running session, and
reconnecting to a running agent and recovering a crashed one stay one
implementation that cannot drift.

Construct at `7179f88` cannot do it yet:

- A turn holds the agent's mutex for its whole length
  (`acp/server.rs`, `agent.lock().await.turn(...)`). An attach that needed
  that mutex would wait behind the permission question it must redisplay.
- Pending permission questions are stored as reply channels only
  (`acp/ask.rs`, `HashMap<u64, oneshot::Sender<Answer>>`); the question
  itself is not kept, so it cannot be asked again.
- `session/load` builds the agent through `Writer::resume`, which runs crash
  recovery: a tool still running, or waiting for permission, would be
  reported as an unknown outcome.
- A turn's completion is the response to its `session/prompt` request. A
  new Deck has no such request pending, and its own request ids start over,
  so an old response could collide with a new request.

## Decision

### 1. Guarantees by failure

| Failure | Expected result | Responsible |
|---|---|---|
| Deck closes or crashes | The running turn continues; reopening shows its current state | holder, plus Construct's live attach |
| `deckd` crashes | The agent continues; the next `deckd` adopts the task | detached holder, runtime reconciliation |
| Construct crashes | Work stops; an explicit resume restores recorded history and reports interrupted work | runtime, Construct journal recovery |
| Host restarts | Processes are gone; the record, journal and disk support a later resume | storage, both recovery paths |

Live attach never makes an interrupted turn continue after Construct dies.
A crash is still recovered, recorded as interrupted, and never repeated
(ADR 0001 Decision 4).

### 2. The live-attach contract (Construct)

Advertised in the `initialize` result as
`agentCapabilities._meta.construct.liveAttach: true`. Without it, Deck does
not attempt a live attach (Decision 4).

- **An attachment** is everything after an `initialize`. A repeated
  `initialize` on the same connection starts a new attachment and supersedes
  the previous one.
- **Attach is `session/load` of a session already live on this
  connection**, sent after the new `initialize`. It does not run crash
  recovery and does not wait on the agent's turn mutex: Construct serves it
  from session state kept apart from the running turn.
- **One boundary between replay and live delivery.** Before answering,
  Construct sends the whole transcript as `session/update` notifications
  (committed history from the journal, then the current partial assistant
  text, then each tool call with its current status), then re-asks every
  unanswered permission question. No live update for that session is sent
  between the start of the replay and the response; updates produced
  meanwhile follow it, without gaps or duplicates. Deck rebuilds its view
  from the replay; it never merges it into what it had.
- **Turn state is explicit.** Construct sends
  `_construct/turn { sessionId, turn, state, stopReason? }` whenever a
  turn's state changes (`working`, `waiting` for permission, `idle` with the
  stop reason), on every attachment, and includes the current state in the
  attach response as `_meta.construct.turn`. A turn that finished while no
  one was attached shows as `idle` with its stop reason.
- **No responses across attachments.** Construct never sends a response to
  a request made in an earlier attachment; a superseded `session/prompt`
  ends with `_construct/turn` alone. So an old response never collides with
  a new request id.
- **The newest attachment owns the questions.** Each unanswered question is
  asked again with a new request id; the old id is retired, and a response
  to a retired id is ignored. A question answered before the new
  `initialize` arrived is not asked again.

### 3. The holder frames lines; it does not speak ACP

Whatever holds the agent (the local shim, a VMM's supervisor) keeps the
agent's stdin open and keeps reading its stdout while no client is
attached, discarding it with a bounded buffer: Construct's replay, not the
holder, is what a new client sees. It allows one client at a time and
forwards whole newline-terminated lines only. A line a client was halfway
through sending when it disconnected is dropped, never joined to the next
client's first line, and a line the agent was halfway through writing goes
to whichever side receives its end. It never parses JSON.

### 4. Deck always rebuilds with `session/load`

Deck never uses `session/resume`, live or after a crash: a transcript Deck
kept is not proven complete (it may have missed the last output before
Construct died). Reopening always loads and rebuilds the view; after a
crash, the load runs Construct's recovery.

An agent that does not advertise `liveAttach` still keeps running while
Deck is closed, because the holder keeps it alive. Deck cannot rebuild its
view, so on reopen it offers to stop the task (and, if the agent supports
`loadSession`, start it again from its own record). Nothing more is
promised for agents other than Construct.

### 5. Construct creates turn checkpoints

ADR 0001 Decision 7 had the runtime commit a checkpoint ref at each turn
boundary, seen over ACP. While `deckd` is down nobody would, and replaying
completions later cannot recreate an earlier state of the files. Construct
always sees its own turn end, so Construct commits the checkpoint (a
temporary index, a hidden ref, the agent's index and branch untouched) and
journals the ref with the turn's seq. The runtime reads them; it does not
make them. Implementation stays with task checkouts (#8).

### 6. Testing

The fake agent implements what Deck relies on (`loadSession`,
`session/resume`, `session/close`, and, with B2, `liveAttach`) and refuses
what Construct refuses. Because a fake can agree with Deck while both
disagree with Construct, B2 also has an integration test against a real
`construct acp` and its scripted model. CI does not build Construct, so
that test runs when `CONSTRUCT_BIN` names a binary and is skipped
otherwise; run it before merging anything that touches attach.

## Consequences

- Deck's reattach depends on a Construct release with `liveAttach`. Until
  then, reopening Deck while the agent runs offers stop and restart, as for
  any agent without it.
- The holder stays small and the same for every backend: line framing,
  exclusive attachment, draining. The shim's replay buffer from ADR 0001 is
  not built.
- Construct takes on real state-model work: session state readable during
  a turn, retained question payloads, attachment epochs, and the turn
  notification.
- Revisit if: equal durability for other ACP agents becomes a requirement
  (then the holder becomes a persistent ACP client, a larger design than
  ADR 0001's buffer), or ACP standardizes reattaching to a live session.

## Evidence

- Construct at `7179f88`: `acp/server.rs` (the turn holds the agent mutex;
  `session/load` refuses a session already loaded on the connection and goes
  through `Writer::resume`), `acp/ask.rs` (questions kept as reply channels),
  `acp/map.rs` (`_construct/event` and `_meta.construct`, the namespace this
  contract extends).
- [ACP session setup](https://agentclientprotocol.com/protocol/session-setup):
  `session/load` replays history before answering; `session/resume` does not.
- ADR 0001; construct#153 and its PR #154 (`session/load`, `resume`,
  `close`).
- Second-model review, 2026-11-02, whose findings set Decisions 1 to 6.
  Where this record departs from it: other agents get a stated fallback, not
  a design; `session/resume` is dropped rather than reconciled.
