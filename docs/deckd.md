# Detached tasks and daemon adoption

Closing Deck disconnects its ACP and control sockets. Each task has a detached
`deckd --holder` process that owns its liveness lock, agent stdio, and ACP relay.
Both the task and an attached ACP connection survive a `deckd` crash.
Reopening the same command and directory finds the same task. When the agent
advertises `_meta.construct.liveAttach`, Deck initializes a new attachment and
loads the recorded session. Construct replays the transcript, restores the turn
state, and renews unanswered permission requests. A streaming or waiting turn
continues in the same agent. A completed turn opens idle.

Agents without live attach, or tasks without a recorded session, open as
**Running, detached**. **Stop and restart**, bound to Cmd-R on macOS and Ctrl-R on
Linux, stops the process and loads its recorded session in a new one.

This completes [B2](https://github.com/strf0x1/deck/issues/24), following
[ADR 0001](adr/0001-task-runtime-and-durability.md) and
[ADR 0002](adr/0002-live-attach.md).

## Ownership and interfaces

`DaemonClient::connect_or_start(data)` connects to the daemon or launches the
sibling `deckd` binary in a separate session. `DECKD` can name another daemon
executable. A successful control request confirms that the daemon accepted the
connection before the client uses it.

`DaemonClient::run(spec)` returns `RunResult { task, started }`. `attach(id)`
returns the stream. A newly started process uses `Client::over`, or `Client::load`
when the record has a session pointer. An existing process with a session pointer
uses `Client::attach`, which requires both live attach and `loadSession` support.
`WindowTask` hides these choices from the view. ACP Ready records that pointer
through the separate `session` control operation.

Every connection replaces the view's `Session` before replay. Namespaced request
IDs and a handshake gate discard output from the previous attachment. The load
response supplies the initial working, waiting, or idle state atomically with
Ready. Later `_construct/turn` notifications drive status, including completion
of a prompt sent by a previous window. Matching prompt responses do not end the
same turn twice. Permission cards retain every renewed request, and answers
consume current request IDs once.

The Runtime remains the lifecycle authority. A daemon service mutex serializes
find-or-create and launch. The detached holder installs the existing relay. ACP traffic and notice
subscriptions run independently.

## Boundary approvals

ADR 0004's approval queue lives in `deckd`, not in a window or the gateway.
A gateway sends `{"op":"approval", task, gateway, operation, destination,
credential?, details:{method,path,size}, reason?}` and blocks for the answer;
the daemon adds the task's project and session, deduplicates, and holds the
call only while a capable window is attached and only for 20 s. A window
attaches by opening a `watch {task}` control connection: it confirms with
`{"ok":null}`, then receives one JSON approval per line for as long as the
connection lives. Dropping the connection ends the lease. Windows decide with
`decide_approval {id, decision}` where decision is `once`, `task`, `project`
or `deny`; `approvals {task}` lists pending items, `grants {task}` lists
task-scoped grants, and `revoke_grant {task,id}` removes one. Task grants are
written to `tasks/<id>/grants.json` before the decision is reported; project
rules are in `approvals/projects.json`; pending items and leases never touch
disk. A new run of a task clears its items and grants. If the daemon cannot
answer, the gateway fails the new scope closed. The alternative command-owner loop added a
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

Stop reserves the ACP transport and disconnects any window. The holder uses
`deck-acp` to initialize a close-only attachment, checks the advertised
`sessionCapabilities.close`, and sends `session/close` for the recorded session.
This exchange has a 500 ms deadline, including unrelated output or partial lines.
The relay still knows only bytes and peer ownership. Takeover never waits for a
blocked agent stdin writer.

The holder then sends SIGTERM to the agent group, followed by SIGKILL after the
grace period. A failed or unsupported close does not prevent termination. The runtime confirms the agent group, holder, and lock are gone
before recording a clean stop. Destroy also removes both sockets and startup identity.
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

## Verification

Run the workspace checks and the standalone daemon driver:

```sh
cargo build
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
python3 scripts/check-deckd.py
```

The daemon and lifecycle tests cover lock ownership, adoption after daemon death,
holder-crash cleanup, process groups, stop deadlines, and destroy. Relay tests
cover fragments, slow readers, bounded output, and shutdown takeover. GPUI tests
exercise prompt submission and permission keys against restored turn states.

`crates/deck/tests/reattach.rs` runs the production `WindowTask` connection against
the fake agent. It checks exact reply text after streaming, waiting, and completed
reattachment, the same agent PID, renewed permissions, raw retired replies, and
suppression of old prompt responses. A daemon crash during the turn also preserves
the holder and transcript.

To run the corresponding real Construct cases, build `construct` and
`construct-dev` in a Construct checkout, then set `CONSTRUCT_BIN`:

```sh
CONSTRUCT_BIN=/absolute/path/to/construct/target/debug/construct \
  cargo test -p deck --test reattach -- --nocapture
```

The tests launch the same scripted model endpoint as Construct's `scripts/mock.sh`.
They use fresh homes and workspaces, make no paid model calls, count model requests,
and inspect the journal for unwanted crash recovery. `construct-dev` defaults to
the binary beside `CONSTRUCT_BIN`; `CONSTRUCT_DEV_BIN` can override it. Without
`CONSTRUCT_BIN`, the real-agent cases print a skip message. They must be run before
merging attach changes, per ADR 0002.

Verified on macOS against the Construct 0.3.0 release binary (`afa8d240…`,
identical to the published aarch64 artifact), launched through Deck's normal
`construct acp` path: recovering mid-stream, while a permission waits, after the
turn completed detached, and after a `deckd` SIGKILL. Each restored the same
agent process with one prompt and one model call. The suite's own real-agent
cases, raw stale permission replies and old prompt response suppression
included, pass with `CONSTRUCT_BIN` set to that binary. The fake-agent and
headless UI cases run in both CI jobs.
