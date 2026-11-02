# Detached tasks and daemon adoption

Closing Deck disconnects its ACP and control sockets. Each task has a detached
`deckd --holder` process that owns its liveness lock, agent stdio, and ACP relay.
Both the task and an attached ACP connection survive a `deckd` crash.
Reopening the same command and directory finds the same task. An already-running
agent opens as **Running, detached**. **Stop and restart**, bound to Cmd-R on
macOS and Ctrl-R on Linux, interrupts it and loads its recorded session.

This extends [B1](https://github.com/strf0x1/deck/issues/23) with the holder and
adoption slice of [B2](https://github.com/strf0x1/deck/issues/24), following
[ADR 0001](adr/0001-task-runtime-and-durability.md) and
[ADR 0002](adr/0002-live-attach.md). Deck still offers stop and restart for an
existing process. Live ACP initialization, session replay, turn-state handling,
ACP `session/close` before stop, and real-Construct acceptance tests remain in B2.

## Ownership and interfaces

`DaemonClient::connect_or_start(data)` connects to the daemon or launches the
sibling `deckd` binary in a separate session. `DECKD` can name another daemon
executable. A successful control request confirms that the daemon accepted the
connection before the client uses it.

`DaemonClient::run(spec)` returns `RunResult { task, started }`. If `started` is
false, the caller sends no ACP handshake. Otherwise, `attach(id)` returns the
stream for `Client::over`, or `Client::load` when the task has a session pointer.
`WindowTask` hides these choices from the view. ACP Ready records that pointer
through the separate `session` control operation.

The Runtime remains the lifecycle authority. A daemon service mutex serializes
find-or-create and launch. The detached holder installs the existing relay. ACP traffic and notice
subscriptions run independently. The alternative command-owner loop added a
queue and scheduling model without changing the serial control workload.

`relay.rs` knows bytes and line endings. Each attachment owns its input fragment.
The agent's partial output line goes to the peer attached when its newline
arrives. Complete output while detached is discarded. The relay permits one
client, caps each line at 1 MiB, and queues at most eight output lines. It gives a
writer 200 ms to catch up before disconnecting it. Oversized lines are drained
and dropped, and any attached peer is disconnected. No fragment crosses into
the next attachment, and no holder parses ACP JSON.

## Daemon death and reuse

`deckd` starts each holder with `setsid` and transfers an already-held task lock
through exec. The parent closes its descriptor without unlocking the shared
flock. The holder restores close-on-exec before launching any children, so only
the holder keeps the task lock alive.

The holder starts a `deckd --child` helper in another process group. Before the
helper may launch the agent, the holder fsyncs `tasks/<id>/holder.json`, containing
its pid, ACP socket, and helper process-group id. A one-byte launch gate then
releases the helper. This startup identity covers crashes before deckd receives
readiness or appends `Started`. Every launch checks any previous startup identity
and confirms teardown before starting another writer.

`Started` records the helper group in `task.pid` and the holder pid and ACP socket
in `task.holder`. The holder waits for that event before serving control requests.
If deckd dies before recording it, the holder stops after five seconds. Holder
observation ignores an incomplete lifecycle-log tail without truncating an
append that deckd may still be writing.

A restarted daemon adopts tasks with held locks. It verifies the recorded identity
through a separate `<id>.holder` control socket and records `Adopted`. Adoption
never connects to ACP, so it cannot displace an attached client or inject bytes.
Exit callbacks carry a launch generation so an old watcher cannot stop a resumed
task. Socket cleanup holds the task lock and checks watcher cancellation before
removing paths.

The child helper watches the holder's pipe independently of input forwarding.
Holder death kills the agent and tools even if agent input is blocked. A free lock
alone does not permit reuse: reconciliation also waits for the recorded group to
have no executing members. An unconfirmed group causes an error, and no recorded
pid is signalled during reconciliation. Zombies cannot execute and do not prevent
recovery.

Stop asks the holder to send SIGTERM to the agent group, then SIGKILL after the
grace period. It confirms the agent group, holder, and lock are gone before
recording a clean stop. Destroy also removes both sockets and startup identity.
A failed shutdown retains its identity and supervision; destroy retries it.

An open window whose daemon dies still needs to reopen for a new control
connection. Its ACP stream can remain connected throughout the outage.

## Paths and lifetime

The daemon holds `<data>/deckd.lock` until its control socket is removed. A second
daemon on that data root fails clearly. Normal lock release explicitly unlocks
before closing its file, so an unrelated concurrent fork cannot extend task or
daemon ownership until exec. It exits after five seconds with no
live task and no control client. A quiet notice subscription counts as a
client, and disconnecting that subscription permits idle exit.

The default control socket is `$XDG_RUNTIME_DIR/deck/deckd.sock`, or
`/tmp/deck-$UID/deckd.sock`. Task sockets are in `tasks/<id>.sock` beside it.
Nondefault data roots add a stable short hash directory under the runtime root,
so tests and side-by-side builds cannot connect to another root's daemon.

## Verification evidence

Runtime tests use the real `deckd` binary for singleton enforcement, idle
shutdown, concurrent clients, refused transitions, failed starts, detached
survival, adoption with an uninterrupted ACP stream, holder-crash cleanup,
unrecorded startup cleanup, and refusal to reuse an executing startup group. Relay
tests exercise raw bytes, input fragments, detached output, slow clients, agent
fragments, and final output before EOF. ACP tests load completed and interrupted
sessions from the persistent fake. GPUI tests dispatch the restart shortcut and
verify that quitting leaves the process alive.

`scripts/check-deckd.py` drives the fake through a waiting permission question,
kills deckd, finishes the turn over the surviving ACP stream, and verifies
adoption of the same holder. It also checks stop, restart, and session load
against built binaries. B1's live macOS window was also driven
through `DECK_CONTROL`: quit during a permission question, reopen detached, and
Cmd-R to recover the same session with an interruption report. The control
snapshots confirmed those states. Screen capture was unavailable in that session.
