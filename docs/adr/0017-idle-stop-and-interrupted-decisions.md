# 0017 — Idle stop and interrupted decisions

Status: accepted for opt-in use, 2026-11-25. Amends ADR 0001 and ADR 0003.
Automatic stopping stays disabled until a policy is configured; the suggested
defaults below are documentation, not the shipped setting.

## Context

A detached holder can outlive both the window and `deckd`. An idle VM or
local agent continues to use memory until somebody stops it. A stopped task
can be resumed, but stopping an agent also destroys its in-memory permission
waiters and interrupts any work in progress.

An ACP permission question and a host boundary approval have different
owners. The agent owns the ACP request. `deckd` owns host approval decisions
and grants. An old ACP response cannot answer a request in a new agent
process, and an old gateway connection cannot receive a new host decision.

## Decision

The holder owns each running task's idle deadline. The same policy applies to
local and VM tasks. `deckd` resolves it at launch from `settings.json` and the
checkout's `.deck/project.toml`, then records it in the task directory; a
per-task keep-running flag defers a deadline. Absent values mean never.
Selecting or attaching to a task does not reset its idle deadline; a resumed
task starts a new deadline. An active turn is not idle, but a turn waiting on
an unanswered question may stop after the longer timeout.

Suggested values are 15 minutes idle and two hours while a person is asked;
neither is shipped as a default. `deck task keep <id> [--off]` defers a task.

An idle stop must preserve unanswered questions before the holder exits. The
user sees each preserved question, even while the task is stopped. Answering
one resumes the task and sends the question and the answer as context in a
new prompt. The prompt explains that the original operation was interrupted.
It does not claim that an old ACP request was answered or that a tool was
authorized. If the agent retries an operation, it must obtain any required
permission again. A preserved host approval must not show an Allow button
unless the selected action actually applies the displayed host grant.

Record each answer durably before attempting delivery. A saved answer and a
delivered answer are different states. Concurrent answers to the same
question must have one winner. A retry after an ambiguous delivery must use
the same persisted idempotency key and an agent that advertises support for
it. For an agent that cannot deduplicate, leave the answer visible as an
uncertain delivery rather than resend it blindly. Preserve multiple open
questions independently.

The holder must fence new agent input and recheck its turn and questions
before stopping. If activity wins the race, it keeps the connection and
resets the deadline. A prompt that loses the race must be retried using its
original idempotency key when the agent supports it. The holder writes a
durable stop note; `deckd` remains the sole writer of lifecycle events and
records the note either on notification or during reconciliation.

The host approval snapshot and stop intent form a durable per-run transaction.
While the input and boundary gates are held, `deckd` stages the pending
questions without showing them as interrupted. It records the stop intent
before authorizing the holder to stop. After a crash, `deckd` reconciles the
transaction against the holder's lock and recorded run before allowing the
task to resume or accepting decisions for it. A confirmed stop promotes the
same staged question identities into the recovery queue. An uncommitted or
failed stop discards the staging before the gates reopen. If the holder's
liveness cannot be established, the task remains fenced until reconciliation
can establish it. Repeating promotion must not duplicate questions.

An in-memory approval lock alone is insufficient: a daemon restart loses it,
and gateway traffic may bypass an approval request when an existing rule
allows it. The implementation must also fence gateway admission and account
for operations already in flight. If it cannot establish a safe snapshot, it
must defer the stop rather than lose a decision or expose a stale one.

A VM stop is clean only when the guest has synced, unmounted its journal,
remounted its root read-only, and confirmed that work for the current boot.
The host must also confirm that the VMM is gone before reusing the disks. A
marker from an earlier boot is not confirmation. The pinned libkrun may exit
the VMM process without returning from `krun_start_enter`, so the process
that records the result must survive that exit.

## Consequences

The recovery message is a new turn, not a continuation of the suspended
tool call. The agent may need to inspect the workspace before retrying work
whose outcome is uncertain. An allow-once answer does not silently become
a broader host or tool grant.

Automatic stopping is opt-in. The holder now fences new agent input with the
relay writer, rechecks its turn and saved answers, and leaves a durable stop
note with the reason and shutdown evidence. `deckd` records that note whether
it sees the exit live or reconciles it on restart. An uncertain delivery can
be retried with its saved key or explicitly reviewed; review does not grant
authority or claim that the agent received the answer. The clean VM stop is
implemented but Linux and Construct recovery after a completed turn remain
unverified, so automatic stopping is not enabled by default.

## Evidence

- deck#243, especially the refined plan and the 2026-11-25 discussion.
- `crates/deck-runtime/src/observer.rs`: the holder tracks ACP request IDs.
- `crates/deck-runtime/src/approvals.rs`: pending host approvals are in memory
  and task cleanup removes them and task grants.
- `docs/adr/0014-prompts-carry-an-idempotency-key.md`: keyed window prompts.
- `docs/vm-spike.md`: the disposable guest shutdown probe and libkrun exit.
- `crates/deck-runtime/src/idle.rs`: policy, stop note and per-task override.
- `crates/deck-runtime/src/recovery.rs`: durable decisions and their keys.
- `crates/deck-runtime/tests/daemon.rs`: idle capture, restart, race and
  legacy-agent delivery tests.
