# 0001 — Tasks run under a host runtime, recover from records

Status: proposed, 2026-11-01. Answers #7's "answer now" questions; the input
is `SANDBOX-DESIGN.md`. Deck's first ADR, in construct's format (see
`../construct/docs/adr/0001-record-decisions.md`): not edited after
acceptance except to mark it superseded.

## Context

Deck spawns its agent as a child, holds its stdio, and kills it on exit
(`crates/deck-acp/src/process.rs`). Closing the window ends the work. The
product needs tasks that outlive the window, survive a stopped or crashed VM
with their disk, and come back after a host reboot without losing or
repeating work.

Three facts constrain the shape:

- **Construct speaks ACP over stdio only, and `loadSession` is `false`**
  (`construct/crates/construct/src/acp/server.rs:106`). Whatever keeps a task
  alive while Deck is closed must hold the agent's stdio, and a reconnecting
  client cannot re-run `initialize` and `session/new` on a live stream.
- **Construct already journals.** One SQLite WAL per project, each event
  written before it is sent, resume projects messages back (construct ADR
  0008). The journal is the conversation's record; Deck must not keep a
  second one. It has gaps, listed under Decision 8.
- **Substrate (`../substrate_v3`) ran this before**, with a daemon owning the
  agent loop and the journal. What carried over is below; the daemon running
  the loop did not, because the loop is now Construct's.

Options for what holds a task while Deck is closed:

| | Shape | Why not alone |
|---|---|---|
| A | Runtime as a library in Deck's process | Tasks die with Deck; #7's restart test is meaningless |
| B | One host daemon owns tasks and their processes | A daemon crash or upgrade kills every task |
| C | A detached holder per task, no daemon | Nobody notices a dead agent or reconciles records until Deck opens |

## Decision

### 1. The isolation unit is the task

A project owns a reusable environment definition. Each task owns its VM,
checkout, writable disk, branch and grants. Cooperating agents (Construct
and its delegated children) share their task's VM because they share its
authority. Agents that need distinct authority are distinct tasks. ACP
sessions are not a boundary: processes in one guest can reach each other.

`attach` yields **one ACP stream per task**. Sessions are ACP's business and
multiplex over it; the runtime never sees them except as ids in the record.

### 2. A daemon is the authority; each task has a detached holder (B + C)

- **`deckd`** owns task records, reconciles them at startup, supervises
  tasks, serves Deck, and later hosts the approval queue and #9's gateways.
  One per data root, enforced with `flock(2)` on `<data>/deckd.lock` (the
  kernel drops it however the holder dies; substrate `data-lock.ts`).
- **A holder per task** runs detached (`setsid`) from `deckd`, holds the
  agent's stdio and serves the task's ACP socket. Locally it is a small shim
  process; in #16 it is the VMM, with the guest end over vsock. A `deckd`
  crash or upgrade leaves tasks running; the next `deckd` re-adopts them
  from the record (holder pid, socket path).
- **Deck is a client.** It starts `deckd` when the control socket is absent
  and exits without stopping anything. `deckd` exits on its own once no task
  is running and no client is connected. A launchd/systemd user unit is
  deferred; on-demand start is enough until something must run with no
  window ever opened.

### 3. Lifecycle

States: `created`, `running`, `stopped { clean }`, `failed { reason }`,
`destroyed`.

| From | Op or event | To |
|---|---|---|
| — | `create` | `created`: record and disk exist, no process |
| `created` | `start` | `running` |
| `running` | `stop` | `stopped { clean: true }` |
| `running` | agent or holder exits; found dead at reconciliation | `stopped { clean: false }` |
| `stopped` | `resume` | `running`, after recovery (Decision 6) |
| any but `destroyed` | `destroy` | `destroyed` |
| `created`, `stopped` | start or resume fails | `failed { reason }` |
| any | shutdown not confirmed | `failed { shutdown_unconfirmed }` |

- `failed` means the user must act; an ordinary crash is an unclean stop,
  because the disk is intact and resume is the normal next step.
- **Stop is confirmed, not assumed.** SIGTERM the agent through its holder,
  wait a grace period, SIGKILL, then check the pid is gone before the state
  changes. A disk whose writer is not confirmed dead is never reused
  (substrate `gondolin.ts` `close()`).
- **No automatic restart.** An agent that exits leaves an unclean stop on
  the record. Restart policy is a later decision, after Construct can load a
  session (Decision 8).
- `destroy` is idempotent. What could not be removed stays on the record as
  `cleanup: pending` and is retried at reconciliation. A destroyed task keeps
  a tombstone record, not its disk.

### 4. What survives what

| Event | Agent / VM | Disk and checkout | ACP session | Turn in flight |
|---|---|---|---|---|
| Cancel turn | lives | kept | lives | ACP `session/cancel`; the agent kills tool process groups, never the VM |
| Deck closes or crashes | lives | kept | lives | keeps running; a pending permission ask waits for the next attach |
| `deckd` crashes | lives | kept | lives | keeps running; the next `deckd` re-adopts |
| `stop` | gone | kept | gone until `session/load` | interrupted |
| Agent crashes | gone | kept | gone until `session/load` | interrupted |
| Host reboots | gone | kept, minus unflushed writes | gone until `session/load` | interrupted |
| `destroy` | gone | gone | gone | — |

**Interrupted work is recorded, never repeated automatically.** Model calls
could be retried safely; tool calls cannot, and the runtime cannot tell them
apart for an arbitrary agent.

### 5. Attach and reattach

`attach(task)` returns the reader/writer pair `Client::over` takes. One
client is attached at a time; a new attach closes the previous one, so two
windows never answer the same ask.

Until Construct supports `session/load`, the holder buffers enough to make
reattach work. It keeps the `initialize` and `session/new` results, every
agent-to-client request still unanswered, and a bounded window of updates
since the last client left; it replays them on attach. `deck-acp` gains an
attach-to-existing mode that takes the session id from the record instead
of sending the handshake. Once `session/load` exists, the holder becomes a
plain byte relay and reattach replays from Construct's journal.

### 6. Recovery runs from records, and is visible

At startup `deckd` takes the lock and reads every record. A `running` task
whose holder pid is gone, or is not the process the record names, becomes
`stopped { clean: false }`. A leftover agent or VMM still holding a task's
disk is killed if it is identifiable as one, and reported if not (substrate
`stale-vms.ts`). `resume` after an unclean stop shows the user what
happened: which turn was cut off and whether the working tree differs from
the last checkpoint (Decision 7). Nothing from memory is trusted across a
restart.

### 7. Durability by layer

| Layer | Failure | Owner |
|---|---|---|
| Conversation | agent dies mid-turn | Construct's journal |
| Liveness | Deck or `deckd` exits | holder per task |
| Execution | VM or agent stops; tool cut off | runtime state plus the journal's interrupted record |
| Workspace | host reboot mid-edit, disk loss | guest filesystem, turn checkpoints, the pushed branch |
| Task records | `deckd` dies mid-write | atomic records under the data root |

Workspace checkpoints: at each turn boundary the runtime commits the working
tree to a hidden ref, `refs/deck/checkpoints/<task>/<n>`, using a temporary
index so the agent's index and branch are untouched. It needs no knowledge
of the agent's internals, because it sees each `session/prompt` complete over
ACP. The checkpoint records the journal seq it follows, so a rewind can move
transcript and files together. Pushing the task branch is the off-host copy
(with #8). Implementation waits for task checkouts (#8, #16); the local
backend runs in the user's directory and does not checkpoint it.

### 8. What Construct must provide

Filed against construct; Deck's recovery depends on them:

1. Resume answers each dangling tool call (an assistant `tool_calls` with no
   `tool_result`) with a synthetic "interrupted, not re-run" result, so
   `journal::project` never hands the provider an unanswered call.
2. Resume records the missing `turn_end { interrupted: true }` of a turn
   that never ended.
3. A `job_start` with no `job_end` is closed on resume as ended by the
   interruption.
4. A failed journal write is surfaced, not swallowed (`Writer::emit` drops
   the error today).
5. ACP `session/load`, from the journal.

Also open: where Construct's journal lives under a VM. Inside the guest on
the task disk, `destroy` deletes the transcript and one corruption takes code
and conversation together. #16 picks between a separate journal disk and a
host-side mirror.

### 9. The task record

Deck's, under its data root: `~/Library/Application Support/deck` on macOS,
`$XDG_DATA_HOME/deck` on Linux.

- `tasks/<id>/events.jsonl`: append-only lifecycle events (created,
  started, stopped with reason, adopted, recovered, destroyed), fsynced per
  line. They are rare, so each one is durable.
- `tasks/<id>/task.json`: the state folded from the log, written temp then
  rename. It holds: id; project and environment reference; backend; state;
  holder pid and socket path; ACP session ids; Construct session ids and
  journal path; checkpoint refs; base commit and branch (from #8).
- Sockets live under `$XDG_RUNTIME_DIR/deck/` or `/tmp/deck-$UID/`, not the
  data root, because the macOS path is too long for `sun_path`.

Session ids are pointers. The transcript stays in Construct's journal.

### 10. The contract

`deck-runtime` (no GPUI, like `deck-acp`) holds the backend trait, the
record store, the local backend, the `deckd` binary, and the client Deck
uses. The operations are `create`, `start`, `attach`, `stop`, `resume`,
`destroy`, and an event stream. Events are lifecycle only: state changes
with reasons, adoption, recovery, cleanup. Conversation events stay on the
ACP stream. Control traffic is newline-delimited JSON over a Unix socket,
framed like `rpc.rs`.

### Deferred

- How dirty local folders enter a task (#8).
- Resource limits, disk retention, eviction (measure in #16).
- Environment upgrades for existing tasks (#10).
- Agent restart policy, after `session/load`.
- `deckd` as an OS service, and starting tasks at login.
- Live disk snapshots instead of git checkpoints, if #16 shows the VMM
  supports them.

## Consequences

- Closing Deck never stops work, and a Deck or `deckd` crash costs nothing
  but a reconnect.
- The local and VM backends have the same shape: a detached holder and a
  socket. #16 changes the holder, not the contract.
- Two new long-lived processes (`deckd`, a shim per task) and a data root to
  keep consistent. Startup reconciliation is mandatory, not a nicety.
- Until Construct ships `session/load`, the shim speaks enough ACP to buffer
  and replay. It is throwaway code with a known end.
- Every resume after a crash asks the user something. Recovery is honest
  and therefore not silent.
- Revisit if: `session/load` lands (remove the replay buffer); #9's gateway
  needs to run with no window ever opened (OS service); one VM per task
  costs too much (measure in #16).

## Evidence

- #7, #14 (connection and process split), #16 (first VM backend),
  `SANDBOX-DESIGN.md`.
- Construct at `ed7127c`: `acp/server.rs:106` (`loadSession: false`),
  `:142` (ACP sessions never resume); `journal/read.rs:67` (`project` keeps
  dangling tool calls); `journal/mod.rs` `Writer::emit` (append errors
  dropped); `jobs/info.rs:177` (`unconsumed` sees only ended jobs); ADR 0008.
- Substrate at `eda62a2`: `host/data-lock.ts` (flock, one host per data
  root), `host/stale-vms.ts` (startup sweep), `sandbox/gondolin.ts`
  (confirmed shutdown), proposal 01 (cancel by process group), OVERVIEW
  (interrupted runs never repeated; the window holds no authority).
- Other harnesses, read 2026-11-01:
  [OpenCode on Cloudflare](https://developers.cloudflare.com/agents/harnesses/opencode/)
  records prompts before they run, replays from a seq cursor, and resumes
  interrupted work automatically, with no stated rule for tool side effects.
  [Pi on Cloudflare](https://dev.to/cloudflare/how-i-run-the-pi-coding-agent-on-cloudflare-ld5)
  makes only completed entries durable and loses a turn cut off by a
  restart. Pi Durable (one secondary account, unverified) runs agents
  detached and attachable by id. None documents tool-call idempotency.
- No measurements yet; the restart and cleanup tests in #7 are the first.
