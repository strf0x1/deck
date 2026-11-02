# Daemon-owned tasks in B1

Closing Deck disconnects its ACP and control sockets. `deckd` owns the Runtime,
agent process groups, and per-task ACP relays, so the tasks keep running.
Reopening the same command and directory finds the same task. An already-running
agent opens as **Running, detached**. **Stop and restart**, bound to Cmd-R on
macOS and Ctrl-R on Linux, interrupts it and loads its recorded session.

This implements [issue #23](https://github.com/strf0x1/deck/issues/23), following
[ADR 0001](adr/0001-task-runtime-and-durability.md) and
[ADR 0002](adr/0002-live-attach.md). B1 always treats an existing process as
detached. Construct's live attachment and independent holders belong to B2.

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

The existing Runtime remains the lifecycle authority. A daemon service mutex
serializes find-or-create, launch, and relay installation. ACP traffic and notice
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

Each local agent runs through a non-detached `deckd --child` helper in its own
process group. The helper forwards stdio and monitors the daemon's input pipe
independently, so a blocked agent input cannot hide daemon death. Pipe closure
kills the helper, agent, and tools in that group. A private startup result lets
the backend record a missing executable as a failed start before exposing ACP.

A new daemon waits for any previous recorded group to stop before reconciliation
can mark its task reusable. A group still executing after three seconds causes
startup to fail. No unverified pid is signalled during reconciliation. Zombies
cannot execute and do not prevent recovery.

This helper cannot survive independently of the daemon. B2 replaces that limit
with detached holders and adoption. An open window whose daemon dies currently
needs to reopen to obtain a new control connection.

## Paths and lifetime

The daemon holds `<data>/deckd.lock` until its control socket is removed. A second
daemon on that data root fails clearly. It exits after five seconds with no
running task and no control client. A quiet notice subscription counts as a
client, and disconnecting that subscription permits idle exit.

The default control socket is `$XDG_RUNTIME_DIR/deck/deckd.sock`, or
`/tmp/deck-$UID/deckd.sock`. Task sockets are in `tasks/<id>.sock` beside it.
Nondefault data roots add a stable short hash directory under the runtime root,
so tests and side-by-side builds cannot connect to another root's daemon.

## Verification evidence

Runtime tests use the real `deckd` binary for singleton enforcement, idle
shutdown, concurrent clients, refused transitions, failed starts, detached
survival, hard-crash cleanup, and refusal to reuse a stopped old group. Relay
tests exercise raw bytes, input fragments, detached output, slow clients, agent
fragments, and final output before EOF. ACP tests load completed and interrupted
sessions from the persistent fake. GPUI tests dispatch the restart shortcut and
verify that quitting leaves the process alive.

`scripts/check-deckd.py` repeats the complete fake-agent stop, restart, and
session-load path against built binaries. The live macOS window was also driven
through `DECK_CONTROL`: quit during a permission question, reopen detached, and
Cmd-R to recover the same session with an interruption report. The control
snapshots confirmed those states. Screen capture was unavailable in that session.
