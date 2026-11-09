# 0011 — Cooperation is one coordinator over tasks

Status: proposed, 2026-11-08. Answers PLAN.md question 5 and #126. Builds on
0001 (tasks and records), 0002 (live attach), 0004 §3 and §5 (gateways and
approvals), 0008 (one branch per task) and 0010 (projects and attention).
Relies on construct ADR 0042 and 0043 (optchat), which are proposed and being
implemented in construct at `d76b161`.

## Context

PLAN.md question 5 asks how agents cooperate: parent/worker delegation or peer
conversations, who owns a task and approves a worker's requests, and how
context and results are shared. The options:

- **In-task delegation only.** Construct's `delegate` tool is a sandboxed
  child in the same guest (construct ADR 0014, 0015). It shares the task's
  sandbox, checkout, routes and grants, returns its last answer (8 KB), and a
  delegated child has nobody to ask for permission, so it rejects. It needs
  no Deck change, but Deck sees one tool call and has no notion of a worker;
  no two tasks can cooperate, and nothing survives a long body of work.
- **Peer conversations.** ACP has no discovery, identity or routing between
  agents; its architecture leaves coordination to the application. Building
  that before the hierarchical case exists is speculative.
- **A Deck-level coordinator.** ADR 0010 already records the user's
  decision: a coordinator that creates and prompts tasks from the start. It
  also names the first missing piece: turn state when no window is open.
- **A second agent loop in Deck.** Construct ADR 0042 rejects it: the loop,
  tools, journal, recovery and ACP server would be duplicated, and the two
  kinds of agent need different context strategies, not different harnesses.

Facts that bound the design:

- Construct can be the long-running coordinator. ADR 0042 makes context a
  strategy and ADR 0043's `optchat` keeps every message at some resolution,
  with `zoom` to reopen a line. In the coordinator-recall bench
  (`bench/results/task-20261108-161422.json`) optchat recovered every probe
  and `compact` a third, over three runs (p = 0.4: a direction, not yet a
  significance claim). That work is landing in construct now.
- A task's ACP session has one client at a time: a window, connected through
  a holder that only frames lines (ADR 0002 §3). With no window there is no
  initialization, no turn state and no one to prompt. A coordinator cannot
  drive a worker through that.
- A task already has everything a worker needs: its own VM, agent, budget,
  grants, attention and publication branch (ADR 0004, 0008, 0010).
- The boundary is the VM, not the agent. Delegated children in one guest
  share its routes and placeholders, and the gateways cannot tell them apart.

## Decision

### 1. Two levels of cooperation; the user owns both

- **In-task delegation stays inside its task.** Construct's `delegate`
  children are sessions of the task's agent, not Deck tasks. They get no
  card, task id, grant, branch or approval queue of their own, and Deck does
  not break them out until there is data to show (§7).
- **Coordination is across tasks, and a task is the unit.** A coordinator is
  a task whose agent creates and prompts other tasks. It owns no code, no
  branch, no credential and no approval.
- **The user owns every task and every decision.** The coordinator never
  answers an approval or permission question, never grants, never publishes
  and never widens its own authority.

### 2. The coordinator is a Construct task using optchat

A coordinator is a task with `kind = coordinator`, created by the user from a
project (later, from a space, which will supply the project set and the
brief). It runs the project's agent command, which is `construct acp` in the
first slice; the contract below is what lets another agent be a coordinator
later.

- **One per scope.** Its scope is a recorded set of project ids. Starting a
  coordinator for the same scope selects the existing one; there is one long
  conversation per body of work.
- **Its context strategy is `optchat`.** Deck sets
  `CONSTRUCT_CONTEXT_STRATEGY=optchat` for the task, from its recorded kind
  rather than a one-off launch setting, so a resume uses the same strategy.
  Construct refuses a model whose window is under 65536 tokens and names both
  numbers (ADR 0043 §1); the window shows that error rather than starting a
  coordinator that cannot remember. The user's brief is the coordinator's
  first prompt.
- **It has no repository policy.** Even when its project has a GitHub origin,
  a coordinator gets no GitHub gateway, no task branch and no publication
  chip. Its guest copy of the checkout is scratch; it can read the code, and
  nothing it writes there can be published.
- **It is an ordinary task otherwise.** The user starts, stops, resumes and
  destroys it, and it survives a closed window, through the same runtime and
  recovery as any task (ADR 0001, 0002). Destroying it does not touch the
  tasks it created; they lose their coordinator and keep running.

### 3. A task's ACP session has one owner: its holder

This amends ADR 0002 §3. The holder no longer refuses to speak ACP; it is the
session's permanent client and speaks a fixed subset of it.

- **The holder initializes.** At task start, before any window can attach, the
  holder sends `initialize` and then `session/new` (no recorded session) or
  `session/load` (a recorded one), the same choice `deck/src/task.rs` makes.
  It records the session id and reports it to `deckd`.
- **The holder may prompt.** `deckd` sends the holder one prompt at a time;
  the holder writes `session/prompt` with a reserved request id prefix.
  Windows keep their own prefixed ids, and the relay serializes whole lines,
  so two writers share one ACP stream without colliding (the `prefix` in
  `deck-acp/src/conn.rs` is already this mechanism).
- **The holder parses what it needs and no more.** It reads the turn
  notification for `working`, `waiting` or `idle`, and permission requests
  only to route them, never to answer one. The approval queue is still
  `deckd`'s, and ACP permission questions still belong to a window.
- **Replay stays Construct's.** A window's `initialize` supersedes the
  holder's attachment on the same connection and gets Construct's replay and
  re-asked questions, exactly as ADR 0002 §2 promises. The holder's
  outstanding prompt response is retired with it, so a prompt reports its
  turn number at once and completion is read from turn notifications, never
  from a response that a later attachment may replace.
- **Turn state is recorded.** The task record gains the current turn number,
  state and stop reason, and the notice stream and `tasks` replies carry it.
  This is the first piece ADR 0010 named: a coordinator can see a worker turn
  finish while no window is open. An agent that does not advertise the turn
  method ends a turn with its `session/prompt` response; the holder records
  that response's stop reason.

### 4. The coordinator's authority crosses one closed control surface

The coordinator's agent needs to act on the host. It reaches a new kind on
the extension port, mapped from its vsock port like the others (ADR 0004 §1),
exposed by `deck-egress` as `DECK_CONTROL_URL`. The wire joins the shared
contract (ADR 0006) as an additive minor release, with schemas and fixtures,
before either implementation lands.

`POST /control` takes one JSON operation, bounded at 32 KiB, and returns one
reply. The set is closed:

| Operation | Input | Returns |
|---|---|---|
| `tasks` | `project?` | summaries of tasks in scope (at most 200) |
| `task` | `id` | one summary, or `deck_control_unknown` |
| `create` | `project`, `brief`, `title?`, `budget?` | `{ id }` |
| `prompt` | `id`, `text` | `{ turn }`, or `deck_control_busy` |
| `wait` | `id`, `after_turn?`, `wait_ms` | the task's summary, or a timeout reply |
| `stop` | `id` | the task's state |

A task summary has: id, title (the first line of its first prompt), lifecycle
state, turn, attention reason, pending boundary approval count, publication
and pull request state, `created_by`, and the last assistant message capped
at 2 KiB.

`create` uses the project's recorded agent command and GitHub policy; the
coordinator cannot choose an argv, another repository, or another base. A
project with no recorded agent command is `deck_control_refused` until the
user sets one. A requested budget may be lower than the policy's, never
higher. `create` records and starts the task and returns its id; `prompt`
waits, bounded, for the holder's session and then sends, and a task whose
agent fails to start is `failed` in its summary and wakes `wait`. `wait` is a
long poll capped at 300 s, like the approval wait in ADR 0004 §5, and it
wakes on a turn change or a new pending approval.

The gateway carries; `deckd` decides. It reads the coordinator's policy from
`tasks/<id>/coordinator.json`, written by the user at creation and never
editable by the guest: the project set, a concurrency cap (default 4), a
creation rate (default 20 per rolling hour), and the per-worker budgets.

Refusals are codes, not cards: `deck_control_scope` (outside the project
set), `deck_control_cap` (concurrency or rate), `deck_control_busy` (the task
is not idle), `deck_control_unknown`, `deck_control_refused`. Exceeding a cap
does not ask the user; the coordinator reports it, and the user raises the
cap in the window. That is what "cannot authorize its own expansion" means
for #11.

### 5. What the coordinator may and may not do

- It may create tasks in its scope, prompt and stop tasks **it created**, and
  read summaries of tasks in its scope.
- It may not prompt a task another coordinator or the user created; it has
  no `destroy`, `resume`, `start`, `mark done`, `grant`, `decide_approval`,
  `revoke`, `credential`, `project`, `open_project`, `set_budget` or
  publication operation.
- Its creation budget is the policy's; a created task's model budget is
  fixed at creation and appears in its record like any task's.
- It cannot make another task a coordinator: only the user creates one, so
  coordination depth is one.

### 6. Approvals and permission questions

- **The coordinator never approves.** Boundary requests go to `deckd`'s
  queue and are decided only by an attached window's lease (ADR 0004 §5).
  With no window they fail closed, the worker's agent sees the denial, and
  the coordinator reads the pending count and the turn's outcome.
- **Authority is per task.** A worker's model, web and GitHub requests are
  checked against that worker's task grants and its project's rules. The
  coordinator's grants are not inherited by workers, and a worker's grants
  are not inherited by the coordinator.
- **The card shows host-recorded provenance.** A worker's card says which
  coordinator created it and which project it belongs to. The agent's own
  reason, including a claim about which delegated child asked, is shown as
  untrusted context and never decides matching.
- **Inside a task, delegation grants nothing new.** A `delegate` child uses
  the same routes, placeholders, checkout and grants as its task, because
  the VM is the boundary and the gateways cannot distinguish its processes.
  A task grant therefore covers every agent in that task by construction;
  this is stated, not implied.
- **ACP permission questions belong to the window.** A delegated child has
  none (it rejects). If Construct later forwards a child's question, the
  card names the child but the answer is still the human's.

### 7. What Deck shows for in-task delegation

A `delegate` call is one tool card: its title, the arguments, and the child's
answer. Deck does not invent a task, card or approval entry for the child.

Child tool calls are not shown yet, because Construct does not forward them
over ACP. Construct already wraps them internally as
`Event::Child { id, inner }`, where `id` is the delegating call (construct
`event.rs`). The later contract change exposes that id as
`_meta["com.sprawlcommons/construct"].parent` on the child's standard
`session/update`s and on any permission request it forwards; Deck then nests
by that key and nothing else changes. The lossless event channel is not used
for this, because Deck does not journal construct's vocabulary. That change
and its UI are deferred (§10).

### 8. Results: each task publishes its own branch

- A worker publishes through its own task's GitHub gateway to its own
  `deck/t-<id>` branch and draft pull request (ADR 0008). The coordinator has
  no git route, so it cannot push, merge or rewrite another task's branch.
- The coordinator reads publication and pull request state from `tasks` and
  reports it. The worker's last message carries the summary it decided to
  give.
- Merging is the user's action on GitHub, and a merged pull request settles
  the worker (ADR 0010 §7). Nothing merges a worker's changes into a shared
  branch, and there is no shared checkout for them to reach.
- A coordinator that wants the next change can create the next task from the
  same base. Stacking branches is deferred.
- The coordinator's memory holds the narrative and the decisions; task ids,
  pull request numbers and states stay in `deckd` and are read through the
  `deck` tool each turn (construct ADR 0042 §6).

### 9. What the window shows

- A coordinator sits in the sessions column like any task, with a distinct
  label and no publication chip. Its transcript is its brief, its `deck` tool
  calls and the user's prompts. **Start coordinator** is the project's action
  that creates or selects it.
- A worker is an ordinary card in its project's column, with the provenance
  from §6. The **Needs you** strip and attention reasons cover coordinator
  and workers unchanged.
- A `deck` tool call that names a task id offers that task: selecting it
  switches the window to the worker's card, across projects if needed.
- In-task `delegate` calls remain folded into their task's transcript (§7).

### 10. First slice, and what is deferred

The first slice is one working loop: a coordinator starts a worker with a
brief, follows its turn, reads its result, and the window shows both. It
needs, in order:

1. the holder-owned session, prompt operation and recorded turn state (§3);
2. the coordinator task kind, its policy record and worker provenance (§2,
   §4);
3. the contract release, the `control` gateway kind and `deckd`'s operations
   and caps (§4);
4. a Construct `deck` tool over that wire (construct repository);
5. the window's Start coordinator, provenance and task selection (§9).

These are filed as deck#150, deck#151, deck#152, deck#153, construct#222 and
sprawlcommons-acp#7.

The coordinator's optchat strategy is already being implemented in
construct. Until it ships, a coordinator task cannot start with that strategy
(construct refuses an unknown strategy name), so this slice lands after it.

Deferred:

- nested in-task worker tool calls and forwarded child questions (§7);
- peer conversations between tasks;
- reading a worker's full transcript through the control surface (the
  bounded summary is the first slice);
- shared or stacked branches, and a coordinator that edits code;
- spaces as the coordinator's home (ADR 0010's next ADR);
- any coordinator operation over credentials, grants, approvals or project
  rules.

## Consequences

- **One loop.** Deck adds no agent loop; the coordinator is Construct with a
  strategy and a tool set, as construct ADR 0042 anticipated.
- **Workers stay ordinary.** Grants, budgets, branches, attention and
  recovery are the existing task machinery, not coordinator-specific.
- **The holder grows a small ACP client.** That is the cost of turn state
  without a window and of prompting a task from outside a window. ADR 0002
  named this as the revisit condition for its "frames lines only" decision,
  and this ADR takes it. The relay's exclusive attachment, line limit and
  queueing stay.
- **One turn at a time remains the rule.** A coordinator that wants a second
  prompt waits; the control surface answers `busy` rather than queueing, so
  the model cannot build a hidden prompt queue.
- **Ruled out:** a Deck agent loop; a coordinator that approves; cascade
  destroy; shared branches; guest-editable coordinator policy; agent-level
  attribution inside a task's VM.
- **Revisit if:** optchat's model window requirement cannot be met by the
  coordinator's model, if a space needs more than one coordinator, or if
  ACP standardizes client-provided tools that would remove the separate
  control port.

## Evidence

- PLAN.md question 5; Deck issues #126, #11, #127.
- ADR 0002 §2–§4 (attach, replay, the holder's line framing), 0004 §3 and §5
  (routes, approvals, window leases), 0006 (shared contract), 0008 §1 and
  §6 (operation wire, one branch per task), 0010 (projects, attention, the
  coordinator's first missing piece).
- Construct ADR 0014 (delegation is a sandboxed child), 0015 (ACP both ways,
  child permissions), 0042 (context is a strategy; Deck's coordinator needs
  no loop of its own), 0043 (optchat and its 65536-token floor).
- Construct `d76b161`: `context/optchat/`, `acp/map.rs`, `child/run.rs`;
  coordinator-recall results in `bench/results/task-20261108-161422.json`.
- Deck `a2e4d8f`: `crates/deck-runtime/src/holder.rs` (holder control
  socket), `relay.rs` (one attachment, line limit), `lifecycle.rs` (`Spec`,
  no turn field), `deck-acp/src/conn.rs` (`prefix`, turn state),
  `crates/deck/src/task.rs` (the window's client choices).
- Contract `strf0x1/sprawlcommons-acp` 1.2.0 (`CONTRACT.lock`): the additive
  home for the control wire.
