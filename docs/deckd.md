# Detached tasks and daemon adoption

Closing Deck disconnects its ACP and control sockets. Each task has a detached
`deckd --holder` process that owns its liveness lock, agent stdio, and ACP relay.
Both the task and an attached ACP connection survive a `deckd` crash.
Reopening the same command and directory finds the same task. When the agent
advertises live attach (`_meta["com.sprawlcommons/construct"].liveAttach`, or
the bare `_meta.construct.liveAttach` from a released Construct), Deck
initializes a new attachment and loads the recorded session. Construct replays the transcript, restores the turn
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
Ready. Later turn notifications (the reverse-DNS method when the Construct
supports it, the bare `_construct/turn` otherwise) drive status, including
completion of a prompt sent by a previous window. Matching prompt responses do not end the
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
connection lives. The snapshot sent on attach includes items that were already
pending, and a waiting change is an update like any other. Dropping the
connection ends the lease. Windows decide with
`decide_approval {id, decision}` where decision is `once`, `task`, `project`
or `deny`; `approvals {task}` lists pending items, `grants {task}` lists
task-scoped grants, and `revoke_grant {task,id}` removes one.
The Access panel uses `access_grants {task}` to list live task allowances and
saved rules for that task's project. Each row shows the destination, gateway,
operation, scope, and expiry. `revoke_access_grant {task,id}` removes the
confirmed grant. For a project rule it also removes matching task grants
throughout that project. Another project's grants cannot be listed or removed
through this task. Pending requests have a Review button that returns to the
existing approval card.

The panel refreshes grants every 30 seconds while open and after approval
decisions. Refresh also reloads credentials; those reads run separately so a
Keychain dialog does not delay the grant listing. Revoke asks for confirmation,
updates after the daemon's reply, and explains that completed operations cannot
be undone. The panel labels local tasks as having unrestricted host network
access; the VM's host gateways enforce its external boundary.

Task-grant and project-rule revocations append `grant_revoked` events to
`approvals/audit.jsonl`, with the removed grant and time, and fsync before a
successful reply. An audit failure reports an error but leaves access removed.

The gateway reads and updates one item over the same socket:
`approval_status {task,id}` returns it (or null once forgotten),
`approval_wait {task,id,wait_ms}` holds a long poll for its decision (capped
at 300 s) and marks the item as waited on while it does, so the card says the
agent is waiting or has moved on, and `withdraw_approval {task,id}` closes a
pending item without granting anything. The status endpoint on the gateway's extension
port is the guest-facing half: `GET /events?after=<seq>`, `GET
/approvals/<id>?wait=<s>` (capped at 300 s) and `POST
/approvals/<id>/withdraw`; none of them can allow anything. Task grants are
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

## Model budgets

A VM task's model gateway enforces one request limit and one token limit. The
gateway counts an admitted chat request before contacting the provider and
reads tokens from the provider's `usage` (the last SSE chunk included); a
spent budget returns `429 deck_budget_exhausted` without a provider request.
After each response it sends `model_usage {task, requests, tokens}` and the
daemon appends a `usage` event, so `task.json` and the `task` control reply
carry the totals. A resumed gateway is started with them.

Launch limits come from `DECK_MODEL_REQUEST_BUDGET` and
`DECK_MODEL_TOKEN_BUDGET` in `deckd`'s environment. `set_budget {task,
requests, tokens}` replaces them live by writing `tasks/<id>/budget.json`
(mode 0600, atomic rename); `null` means unlimited. The gateway re-reads that
file before each admission, so the next request uses the new limits without
restarting the VM. The model allowlist is still fixed when the gateway
starts.

## The web and package gateway

A VM task's second gateway kind (ADR 0004 §3) is a proxy: CONNECT tunnels to
any destination the host validated and the task was given, and plain HTTP for
declared mirrors. It has its own vsock port (1028) and answers the `web` kind on
the extension port. The guest image starts two `deck-egress` relays over it —
`127.0.0.1:8082` for `http_proxy`/`https_proxy` and `127.0.0.1:8083` for a
mirror list — but only when the task has a web gateway; a proxy pointing at a
closed port is worse for a package manager than no proxy.

`deckd` passes the policy, never the guest: `--web-profile` for each profile
(`repo`, `registry`, `package-mirrors`, `os-mirrors`, `docs`; the table is
compiled into the binary), `--grants` for the task's `grants.json` and the
project's `approvals/projects.json` (re-read when their identity, size or mtime change),
and no policy that the guest can name. `DECK_WEB_PROFILES` in `deckd`'s
environment selects the profiles (default `repo,registry,package-mirrors,docs`;
`none` starts no web gateway). An undeclared profile refuses to start the
gateway.

Every request is resolved on the host and refused without asking if any
address is loopback, private, link-local, shared, documentation, multicast or
unspecified — on any port, grant or not. A destination the task holds (a
profile entry, a live grant) is dialled; otherwise the gateway asks the
approval queue with `gateway: "web"` and `operation: "connect"` or `"fetch"`.
A plain-HTTP redirect is a new destination and is checked (and asked for) the
same way. `web_profile {task, project?, destinations, revoke}` approves or
revokes a whole profile: the daemon writes one project rule (`web`/`connect`)
per destination, from the host's expansion, so a profile the task did not start
with is one decision and a package install does not ask per package. Task grants
carry a 12-hour expiry; project rules do not.

Each granted CONNECT, plain-HTTP stream or model request holds the exact
rule that admitted it. One monitor checks policy files every 50 ms and closes
both guest and upstream sockets when that rule disappears or expires,
including while `deckd` is down. Unrelated requests stay active. A replacement
rule can admit a new request but cannot keep an older rule's stream alive.
An allow-once decision admits one operation; consuming it does not interrupt
that operation. Launch web profiles and configured models remain fixed for
the gateway's lifetime.

The VMM passes `--grants` and `--project` independently of the optional web
socket, so model grants remain revocable with `DECK_WEB_PROFILES=none`.
The gateway matches task grants to its task and project rules to the host
project identity. The shared project-rule file cannot grant another project's
access. Model names match exactly. Each model request owns a separate HTTP
client pool so revocation interrupts idle upstream reads without closing
unrelated requests. A revocation before the gateway commits the response returns a bounded
`deck_boundary_denied`; one after that point interrupts the response.

The gateway's boundary events reach the agent through the status endpoint's
`/events` beside the model gateway's, with `gateway: "web"`, so a blocked
package fetch is explained in the tool result rather than read as an outage.

## Credentials

ADR 0004 §4's broker lives in `deckd`. It reads one store, chosen at start:
the 0600 JSON file `DECK_CREDENTIALS` names (refused at start with group or
other bits), or otherwise the macOS Keychain / Secret Service under
`com.sprawlcommons.deck`. `deckd` never reads a value from its environment,
and the runtime removes the task's credential names and `DECK_CREDENTIALS`
from the environment it hands the VMM and the gateway. Each read is bounded
at 5 s, with at most one read per name outstanding.

A VM run writes its credential set into `vmm.json` as `gateway.credentials`
(the model key's name, and the GitHub token's when the project names a
repository). These control requests serve it:

- `credential {task, name}` returns `{"value", "fingerprint"}` for one
  operation, only for a running task and a name in its set; anything else is
  refused without reading the store. A failure is an error line starting with
  its code: `deck_credential_missing`, `_unavailable`, `_refused` or
  `_revoked`. Neither the request nor the reply is logged; the log line names
  the task, the name and the fingerprint. The first use of a value in a run
  appends `credential_used {name, fingerprint}` to the task's events.
- `credential_watch {task}` replies `{"revoked": [...]}` for the task's set,
  then pushes each later revocation as one JSON line for the connection's
  life. Each gateway holds one, reopened with backoff.
- `list_credentials` replies with the daemon's store description and every
  stored name's fingerprint, revoked state and read error. It uses the same
  broker and outstanding reads as task listings and gateway fetches, without
  holding the task service lock or returning credential values.
- `revoke_credential {name}` revokes the name's current fingerprint, writes
  it to `credentials/revoked.json` before replying, and pushes it to the
  watching gateways, which close the requests and GitHub operations that used
  it. A new value serves again; the revoked one never does.

`deckd credential set NAME` (value on stdin), `delete NAME`, `list` (names,
fingerprints, store, revoked state) and `revoke NAME` manage it. `list` and
`revoke` connect to the daemon, starting it when necessary. A running daemon's
store selection wins over the CLI's environment. If it cannot connect or
start the daemon, `list` fails without opening an independent store read.
`set` and `delete` manage the store directly.

## The GitHub operation gateway

A VM task's third gateway kind (ADR 0007) publishes to the project's GitHub
repository and nothing else. It has its own vsock port (1029), answers the
`github` kind on the extension port, and the guest image starts one
`deck-egress` relay at `127.0.0.1:8084`, exported as `DECK_GITHUB_URL` — only
when the project named a repository, because a URL pointing at a closed port
is worse than none.

The gateway exists when the task's project has a GitHub destination (ADR 0008
§3), resolved by `deckd` once at create and recorded in the task's spec:

- `origin`, when it is a github.com HTTPS or SSH remote, supplies
  `owner/name`; the base is the branch in `refs/remotes/origin/HEAD`, else
  `main`; the host's `user.name`/`user.email` are recorded for the guest
  checkout.
- `.deck/project.toml` at the checkout's top level `[github]` overrides
  `repository`, `base`, `branches` and `approvals` (`own-branch`, the
  default; `ask`; or `own-branch-public`); a malformed file fails the create
  rather than silently publishing to `origin`. When the file (or the
  development override) names a repository other than `origin`, the base is
  `main` unless set explicitly, because `origin/HEAD` describes the other
  repository.
- A project with neither a GitHub origin nor a file has no gateway.

`DECK_GITHUB_REPOSITORY`, `DECK_GITHUB_BASE`, `DECK_GITHUB_BRANCHES` and
`DECK_GITHUB_TOKEN_SECRET` in `deckd`'s environment remain a development
override for every newly created task and say so in one log line; they are not
how a project is configured. The token is the stored credential named by the
task's policy (default `DECK_GITHUB_TOKEN`), fetched from the broker once per
operation (see Credentials below); its value is never in `vmm.json`, the guest
environment, a log or a response.

Five operations, and no relay:

| Path | Operation | Asks |
|---|---|---|
| `POST /operations/publish` | push a git bundle to one allowed branch | yes |
| `POST /operations/pull_request` | open a pull request to the allowed base | yes |
| `POST /operations/pull_request/update` | title, body, draft, base | yes |
| `POST /operations/refs` | list branches | no |
| `POST /operations/pull_request/review` | read a pull request | no |

`merge`, `close`, `delete`, `tag`, `release`, `force_push`, `workflow` and
`settings` are refused by name so an agent learns the set instead of retrying
a 404. A publish carries one git bundle after its JSON body; the gateway reads
the header itself and refuses a bundle with more than one ref or a ref that is
not the allowed branch, then fetches and pushes exactly that refspec. A push
is fast-forward only (`--force-with-lease` against the sha read a moment
before), so a task cannot rewrite its branch.

A whole GitHub profile is one decision: `github_profile {task, project?,
repository, operations, revoke}` writes one durable project rule per operation
for the repository, and a revoke removes exactly those.

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

### Retrying OS credential access

After a Keychain read is cancelled (`errSecUserCanceled`) or authentication
is refused (`errSecAuthFailed`), the daemon retains that failure by credential
name. Repeated model requests, listings and revocation attempts return the
existing failure without starting another OS read. The error names the
recovery command:

```sh
deckd credential retry OPENAI_API_KEY
```

The command sends private host control `retry_credential {name}` and reports
whether a completed refusal was cleared. It does not fetch a value; the next
operation or listing may ask for OS access. An unanswered read stays in
flight even if its caller timed out, so retry cannot create a parallel dialog.
A retry changes neither task authority nor revoked fingerprints. Successful
reads still go to the store on every request, so rotation is not cached.

This pause lasts for the current daemon's lifetime, across task and window
attachments. It is not a persisted credential or grant. Transient failures,
missing items, unavailable interaction and locked Linux Secret Service
collections continue to recover automatically. Linux does not classify
localized `secret-tool` diagnostics as an explicit human refusal.

The CLI provides the explicit retry in this slice; a dedicated Access-panel
retry action remains follow-up work. [Live development macOS verification](acceptance/2026-11-05-macos-credential-retry.md)
records the test signing identity, dialog results, and remaining findings.
Verification with the supported release identity is still pending.
