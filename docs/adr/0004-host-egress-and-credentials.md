# 0004 — Host gateways enforce guest egress, credentials, and approvals

Status: accepted, 2026-11-03; first and second slices landed 2026-11-03. Answers #9; the input is
`SANDBOX-DESIGN.md`, ADR 0001 and the #16 spike. #8 owns publication
operations; #12 compares runtimes.

## Context

The VM backend has no enforced external boundary today. `vmm.rs` calls
`krun_add_vsock_port2` but never `krun_add_vsock`, and the guest has no
virtio-net device. libkrun's implicit-vsock heuristic then enables TSI with
`HIJACK_INET` (`libkrun` v1.19.7, `src/libkrun/src/lib.rs` around the
`vsock_config` match). The guest's AF_INET sockets are proxied by the host
VMM, so the guest can reach the host's loopback and whatever the host's
network namespace can reach. The spike uses exactly that to point Construct
at `http://127.0.0.1:<port>/v1` (`docs/vm-spike.md`). It is convenient for
the spike and unacceptable for the product: it is a route around any policy
Deck adds later, and it is invisible in the guest's network configuration.

Three more facts constrain the design:

- **Construct's secrets resolve environment-first, then the OS keyring.**
  `secrets.rs` builds a secretspec spec with `["env", "keyring"]`; a
  non-empty environment value wins and the keyring is never consulted. The
  endpoint's secret name and optional extra headers come from `config.toml`
  (`endpoint.rs`, `llm/mod.rs`), and delegated children read only the
  environment their parent hands them (`env_key`). The guest image has no
  keyring service, and any environment value is readable by every process in
  the guest, including a compromised agent.
- **ACP permissions are cooperative.** `session/request_permission` is an
  ask the agent may ignore; the protocol's security guidance says so.
  Forwarding Construct's asks (#13) is a UX fix, not an enforcement
  mechanism. Boundary enforcement cannot depend on the agent.
- **The holder, not the daemon, keeps work alive.** ADR 0001 makes `deckd`
  the authority and each task's holder detached, so a `deckd` crash does not
  stop the agent. Egress that only exists inside `deckd` would fail for a
  running task during a daemon restart. The approval queue and persisted
  grants still belong to `deckd`; the per-task data plane belongs to the
  holder.

## Decision

### 1. The guest has no IP egress; its only external routes are mapped vsock ports

The VMM calls `krun_disable_implicit_vsock(ctx)` and then
`krun_add_vsock(ctx, 0)` (explicit vsock, no TSI flags), and adds no
virtio-net device. The order is required: `krun_add_vsock` refuses with
`EEXIST` while the context is still `Implicit`, and the default is implicit.
The guest keeps a loopback interface for relays inside the guest (the entry
brings `lo` up); loopback is not an external route. Apart from the ACP
stream, every capability that crosses the boundary is a host Unix socket
that libkrun connects to when the guest opens a mapped vsock port:

```text
guest AF_VSOCK connect(2:port) -> libkrun unix_ipc_port_map -> host socket
```

The port-to-socket map is in the VMM's config, written by the runtime, and is
fixed when the VM starts: `krun_start_enter` consumes the context and libkrun
1.19.7 has no add-port-after-start call. The guest cannot add, remove or
redirect a port. A task's VM has only its own map, so it cannot address
another task's socket.

That is a configuration boundary, not a policy boundary. One mapped port
accepts many concurrent connections, each becoming its own host Unix stream,
and the gateway applies its current policy to every new connection or parsed
request. A new destination, port or rule needs no new port; a new *gateway
kind* needs a port reserved at boot (or a VM restart).

So every VM boots with one more mapped port than v1 uses: a reserved
**extension port** whose first bytes name the gateway kind the connection
wants (a short framed header, then the stream). The gateway rejects a kind
it does not implement. A later kind — an SSH agent, a service gateway for
`web_search` — ships as a gateway update served on that port, and running
tasks gain it without a restart. The dedicated v1 ports stay; the extension
port is what keeps the next kind from costing every running task a restart.

Guest relays (`deck-egress`, part of the curated image) expose TCP endpoints
on `127.0.0.1` for clients that cannot speak AF_VSOCK — Construct's HTTP
client, package managers, git. A relay is a byte carrier, not a policy
point: an agent can replace it, ignore it, or open the vsock port directly,
and the host gateway applies the same policy. Replacing the relay therefore
does not widen the boundary.

Consequences:

- No DNS, raw IP, UDP, QUIC, IPv6, direct connection on any port, redirect
  to a new host, or host/private-network destination has a route. Those
  attempts fail closed rather than reaching a host proxy to be logged and
  approved. A port other than 443 is reachable only as an approved CONNECT
  through the gateway.
- Adding a gateway kind later does not restart running tasks: it is served
  on the reserved extension port.
- The only reachable host endpoints are the mapped gateway ports.
- Tools that insist on direct sockets or their own DNS do not work in the
  guest. That is a compatibility decision, not a bug; #12 can evaluate a
  virtio-net backend behind the same gateway contract if it proves necessary.
- Disabling TSI is independent of the ACP stream: ACP is already a native
  vsock connection (`socat VSOCK-CONNECT:2:1024`), not TSI.

### 2. One gateway process per task, under the holder; policy and queue in `deckd`

Each task gets one `deck-gateway` process with a listener per boundary kind.
The VMM starts it before boot, in the VMM's detached process group, waits for
its readiness line, then maps its sockets. It therefore lives exactly as long
as the task and survives a `deckd` crash, like the holder. The VMM watches the
gateway and exits on its death, which fails the task closed.

`deckd` remains the authority for:

- the task's policy snapshot and persisted project rules;
- the approval queue and the human decision;
- credential storage and lookup;
- the audit record.

The gateway evaluates every request against the policy snapshot and cached
task grants without a daemon round trip. A new scope asks `deckd` over the
control socket; a credential request asks `deckd` for the value. If `deckd`
is unreachable, cached grants continue to their expiry and every new scope is
denied. If the gateway dies, the VMM fails the task closed rather than leave
an agent running with half a boundary.

Policy is live. `deckd` pushes a changed project rule or task grant to the
running gateway, and new connections and parsed requests use it without a VM
restart. Removal is stricter than addition: removing a destination or grant
also closes matching open tunnels. For a CONNECT tunnel the gateway knows
only the destination, so that is the granularity; the model gateway parses
each request and applies a changed model or budget immediately.

Why not put the parser in the VMM: the VMM should stay a small, constrained
component that owns the microVM and relays bytes. Why not put it in `deckd`:
a parser crash would take the control plane for every task, and egress would
stop across a daemon restart. The gateway is a separate crate
(`crates/deck-gateway/`, no GPUI) so the HTTP/TLS dependencies do not enter
`deck-runtime`, and its policy core can be tested without sockets. Its binary
is named by `DECK_GATEWAY_EXECUTABLE`, defaulting to the sibling of `deckd`,
the same way the VMM names its executable.

The local-process backend is not boundary-enforced. It remains a development
backend and is never the VM task's backend; the UI should not present it as
a sandbox.

#### The agent's own sandbox follows the backend

Construct's `[sandbox]` (default `workspace`, also `read-only` and `off`) is a
*process* boundary: Landlock on Linux or a Seatbelt profile on macOS around
the paths tools may touch, and an environment with credentials removed. It is
not egress control; a sandboxed process can still open sockets, which is why
this ADR's boundary cannot be built from it.

The two backends therefore get different policies, and the backend, not the
agent's configuration, decides:

- **Local backend.** Construct's configured sandbox stays exactly as the user
  set it. That process boundary is the right one there: the agent shares the
  user's real workspace, home directories, keyring and environment, and
  `workspace` (read/write the working directory, read system paths and
  toolchains, nothing else under `$HOME`) is what keeps a tool call from
  reading the host's credentials. Deck overrides neither `[sandbox]` nor
  `CONSTRUCT_SANDBOX` for a local task. It remains true that this backend has
  no enforced *external* boundary and the UI must not call it a sandbox.
- **VM backend.** The guest is the isolation boundary and the gateways are the
  external one. The runtime sets `CONSTRUCT_SANDBOX=off` in the guest agent's
  environment. `off` removes Construct's filesystem and exec boundary but
  keeps its process-lifetime helper (a shell call and everything it starts are
  killed when the call ends). Inside the guest that boundary would protect only
  the guest's own files and its journal, which the host already treats as
  untrusted; it would not protect the host, and libkrunfw 5.6.2's kernel has no
  Landlock, so `workspace` refuses every shell command (`exit 126`). Turning
  it off is what makes tools work, not a loosening of the host boundary.

This setting is not itself a boundary in the VM. The guest's `config.toml` is
agent-writable, so reading the mode from it would be agent policy; the runtime
wins by setting the environment before Construct starts. Even that is only
hygiene: the agent can unset the variable for its own children. Nothing in
§1, §3 or §4 depends on it — a guest process with a sandbox on, off, or
absent still has no IP route, no host credential, and only the mapped
gateways.

Approvals are unchanged by this split. ACP `session/request_permission`
remains cooperative UX in both backends; in the VM the enforcement surface is
the host boundary in §5, not Construct's process sandbox.

Revisit if the guest image gains a Landlock-capable kernel or another working
process sandbox: keeping Construct's sandbox on inside the VM is useful
defense in depth, and it becomes necessary if several agents ever share one
guest. Until then, the local backend keeps the process boundary and the VM
backend keeps the hypervisor boundary, and neither claims the other's.

### 3. Three gateway kinds in v1

#### Model gateway

An OpenAI-compatible reverse proxy for the endpoint the task was configured
with. It accepts only the paths Construct uses (`/v1/chat/completions`,
`/v1/models`, and the provider's equivalent if a dialect needs one), parses
the model field, and maps it to a host-side provider entry. It rejects a
model the task is not allowed to use, enforces per-task request/token budgets,
streams the response, and records usage.

It **strips every credential-bearing header from the guest request** —
`Authorization`, `Proxy-Authorization`, `Cookie`, `x-api-key`, `api-key`, and
any header whose name is configured as a credential — then injects the host
key for the selected provider. A short allowlist of non-secret attribution
headers (`HTTP-Referer`, `X-Title`) survives. The guest sees a placeholder
API key, never the real one.

A runtime model switch is checked against the same allowlist; it is not a
bypass because Construct's model selector is a request to the gateway, not a
new endpoint. `/v1/models` is served through the gateway with the host key,
or from the host's cached catalog; the guest cannot disable discovery in a
way that avoids the gateway, because the gateway is the only route.

#### Web and package gateway

An HTTP proxy with two modes:

- **CONNECT**, to port 443 by default. The host resolves the name, rejects
  loopback, private, link-local, multicast and unspecified addresses, and
  dials the validated address. The TLS stream is opaque: this is
  destination-level control, not URL-level. Another port is a policy
  decision, not a new route: a CONNECT to `host:port` outside the profiles
  asks like any new destination, and the grant names the port, so
  `example.com:22` allowed does not allow `example.com:2222`. The address
  checks apply whatever the port. This is how SSH reaches a server
  (`ProxyCommand` or git's `core.sshCommand` through the relay); it gets
  network reachability only, and SSH credentials are a separate question
  (below).
- **Plain HTTP**, only for declared mirrors. The gateway parses the
  absolute-URI request, applies the same destination checks, and re-checks
  every redirect.

Named profiles cover repository reads (`github.com`,
`raw.githubusercontent.com`, `codeload.github.com`), registries
(`crates.io`, `static.crates.io`, `index.crates.io`, `registry.npmjs.org`,
`pypi.org`, `files.pythonhosted.org`), OS mirrors, and documentation. A
project can declare profiles and add a destination through an approval that
becomes a project rule. Hostnames are resolved on the host; the guest has no
resolver and no route for one.

Path-level and credential-bearing package traffic is not solved by CONNECT.
A private registry needs a TLS-terminating gateway or a host mirror, which is
#10/#8 work; v1 does not put registry credentials in the guest.

SSH keys follow the same rule: v1 puts no private key in the guest. A
tunnel to `host:22` is useful for host-key checks and keys the agent was
given for that server alone; it does not carry the user's identity. Pushes
to GitHub go through the operation gateway below. Signing in to other SSH
servers with the user's key is a later SSH-agent kind on the extension port:
the host's agent signs, per approved destination, and the key never leaves
the host.

#### GitHub operation gateway

#8 owns publication, PR creation and the ref rules. #9 provides the
transport, approval and credential machinery: a service-specific gateway on
its own vsock port that accepts the small structured operations #8 defines
(`publish`, `create_pr`, `update_pr`) and never a general GitHub API relay.
Read-only clone/fetch can use the web profile; push, tags, branch updates and
PR state go through this gateway, with the host token and the repository/ref
checks enforced on the host. Merge is never exposed merely because the token
could do it.

#### Construct's network surfaces

| Surface | v1 route |
|---|---|
| primary model calls, compaction summaries, learning | model gateway |
| delegated agents | model gateway; they inherit placeholders, not keys |
| model catalog/discovery | model gateway `/v1/models` |
| `web_search` (Exa/Brave/Tavily) | disabled in v1, or a later service gateway; never a guest key |
| router (`TYPESAFE_API_KEY`) | disabled in v1; a later service gateway |
| MCP servers | no IP route; they may use the web proxy profile if they honor it; a server needing its own credential needs a later gateway |
| `construct secrets set` | unsupported in the guest; Deck owns secrets |

### 4. Credentials live on the host and are injected at the gateway

- **Storage.** Provider keys, registry tokens and the GitHub credential live
  in the OS credential store (macOS Keychain, Linux Secret Service) under a
  Deck-owned service name. A headless fallback is an explicitly configured
  file with mode 0600; it is not the default and is never inside the guest
  image or an exported task disk.
- **Guest values.** The guest receives a non-empty placeholder for each
  secret Construct requires, so secretspec resolves it from the environment
  and skips the keyring. `CONSTRUCT_BASE_URL` points at the guest relay;
  `CONSTRUCT_MODEL` names the task's model. No real value appears in
  `vmm.json`, the console, the journal, an ACP frame, or a log.
- **Injection.** `deckd` hands the gateway a credential value over the host
  control socket for one operation. The gateway keeps it in memory for that
  request and never writes it. The guest's own `Authorization` is discarded.
- **Headers from Construct.** Construct sends `[headers]` with every model
  request. The guest config is agent-writable, so the gateway treats those
  headers as untrusted: only the attribution allowlist is forwarded; every
  other header is dropped before the host credential is added.
- **Rotation and revocation.** Replacing a value in the credential store
  affects the next request. Revoking a credential closes open tunnels and
  in-flight operations that used it and records the revocation. Task records
  and project rules store credential *names* and fingerprints, never values.
- **Delegated children.** Construct hands a child only the environment its
  agent definition names (`env_key`). Deck supplies placeholders to the
  parent, so a child that asks for a different provider still reaches the
  gateway under the same task policy; it cannot read the host credential.

### 5. Approvals are host decisions, distinct from ACP permissions

A boundary approval is a runtime event, not an ACP `session/request_permission`.
The two can appear together in a transcript, but they answer different
questions: ACP asks whether the agent may run a tool; the host asks whether
an external operation may cross the boundary. Agent-supplied explanations are
context, never authority.

An approval item records: task, agent/session, gateway, operation, destination
(host, port, path or model), credential name if any, observed request details
(method, path, size — not a body), requested duration, policy version, a
deduplication key, and the agent's untrusted reason.

Options:

- **Allow once** — the held request continues; if it already failed, one
  retry of the exact request fingerprint within a short window (for example
  60 s).
- **Allow for this task** — matching operations until the task stops or the
  grant expires.
- **Always allow for this project** — a persisted project rule. Rules live
  in Deck's data root keyed by project identity, not in the repository, so
  the guest cannot edit its own policy.
- **Deny** — the operation fails; the agent sees a structured error.

The gateway **holds briefly, then fails**. While a Deck window is attached
to the task, a request that needs a new scope waits up to a short hold
window (20 s by default) for the decision: Allow continues the same request
or tunnel, so `cargo fetch` simply succeeds after the click. Deny, expiry of
the hold, or no attached window returns `403` with a JSON body such as
`{"error":"deck_boundary_denied","approval_id":"…","retryable":true}` and an
`X-Deck-Approval` header; the item stays pending until its own expiry and a
later decision applies to the next attempt. The hold is bounded so a socket
never waits on a human indefinitely, and it is per request, so a slow
decision cannot stall unrelated traffic.

**What counts as attached.** Attachment is an explicit lease from a window
that can answer, not a connection to the task socket. A Deck window registers
a watch on a task over its long-lived control connection (the notice
subscription), carrying its boundary-card capability; detaching, closing the
window or losing that connection drops the lease, and the window re-registers
after it reconnects. `deckd` is the source of truth: the gateway asks it both
for decisions and for whether a capable window is attached. A raw ACP
connection is not attachment — a test harness, a CLI or an older window
without the card would otherwise cause a 20 s hold for a human who is not
there. Multiple capable windows count as attached; the first decision wins
and every attached window sees the item. Leases are ephemeral like pending
items, never persisted. If `deckd` is unreachable the gateway fails fast,
because the same failure makes a decision undecidable.

The hold matters most for CONNECT. Most clients report a refused tunnel as
"CONNECT failed: 403" and never show the body, so the agent cannot see the
approval id or know that a retry would now pass; the boundary card is the
only place the decision is visible. When the hold expires, the denial in the
transcript (below) tells the user a retry is needed, and §6 tells the agent.

Model access for the task's selected provider and model is authorized by the
task's own model choice, so a first turn does not fail merely because a model
gateway exists; switching model or provider asks.

Pending items are deduplicated within a short window and expire (five minutes
by default). A late decision reports `expired`; the guest may retry. Pending
items are ephemeral across a `deckd` restart: the in-flight request already
failed, and the guest's retry creates a fresh item. Saved rules and task
grants are persisted.

Control surface: the daemon control protocol is additive. A window opens a
`watch {task}` connection, which is both its notice stream for that task and
its attachment lease; Deck gets `approvals` (pending items for a task),
`decide_approval`, `grants` and `revoke_grant` on ordinary control requests.
The gateway asks `approval` with the observed request and the daemon holds
that call; a future `credential` request returns one value. Neither request
nor reply is logged. Task grants are `tasks/<id>/grants.json`, project rules
are `approvals/projects.json`, and pending items are memory only.

The UI adds a boundary card and a per-task access panel:

- the card shows task, gateway, destination/model, credential name, the
  reason, whether the agent is waiting or has moved on (§6), and the options
  above — never a secret;
- the panel lists active grants with their scope and expiry, pending items,
  and a revoke control;
- a denial appears in the transcript with the approval id and a retry hint,
  not as an opaque tool failure.

### 6. The agent is told why it was blocked, and may wait

A blocked connection looks like a network failure to the tool that made it:
cargo, git and pip report "CONNECT failed" or "connection refused" and drop
the `403` body. An agent that reads that as an outage goes looking for
another mirror, vendors the dependency, or concludes the package does not
exist — wasted turns at best, a workaround at worst. The agent needs three
facts: this is policy, not an outage; a human has been asked; and, later,
what they decided. Construct is the agent this is designed for; another ACP
agent gets the hold and the `403` and nothing more until it implements the
same contract.

**Boundary status kind.** The gateway serves a read-mostly status endpoint
as the first kind on the extension port (§1), which also exercises that port
from v1. `deck-egress` exposes it on guest loopback as plain HTTP:

- `GET /events?after=<seq>` — boundary events for this task: sequence
  number, approval id, gateway, destination, state (`pending`, `allowed`,
  `denied`, `expired`, `withdrawn`), scope if decided.
- `GET /approvals/<id>?wait=<s>` — the item's state, long-polling up to `s`
  seconds for a decision; the host caps the wait (300 s).
- `POST /approvals/<id>/withdraw` — drop a pending item.

The sequence is the gateway's counter, not a guest clock. The endpoint sees
only its own task's items, since the VM has only its own map. Nothing on it
can allow anything: withdraw only removes a pending item, and the decision
path stays host-side. Requests are rate-limited like any gateway traffic.

**In the tool result.** Construct takes the current sequence before a tool
runs and asks for `/events` after it exits. If anything was blocked, it
appends a note to the tool result the model is about to read:

```text
[deck boundary] blocked while this ran: index.crates.io:443 (approval a1f3,
pending with the user). This is the user's policy, not a network failure;
do not look for another route to the same resource. Call boundary_wait if
the task needs it, or carry on and boundary_withdraw if it does not.
```

"While this ran" is literal: with concurrent tools or delegated children the
note can include another process's block. It is attribution by time, stated
as such, not proof of which process asked.

**Agent tools.** Construct offers three ordinary tools, so waiting is the
agent's choice rather than forced on it — a casual lookup can be dropped, a
build that needs the crate can wait:

- `boundary_wait(id, timeout_s?)` — blocks until a decision or the timeout;
  returns `allowed` (with scope), `denied`, `expired`, `withdrawn` or
  `timeout`. ACP `session/cancel` interrupts it like any tool.
- `boundary_status(id)` — the same answer at once, for an agent that keeps
  working and checks back.
- `boundary_withdraw(id)` — the agent no longer needs it; the card closes
  and the audit records the withdrawal as the agent's.

Their descriptions carry the guidance the model needs at the point of
choice: wait if the task needs the destination, otherwise continue or
withdraw, and never try another route to the same resource. An `allowed`
result is the cue to retry; `denied` means choose another approach, not
retry.

**Back to the user.** The gateway knows when a wait is open on an item and
reports it to `deckd`, so the boundary card says **agent is waiting** or
**agent moved on**. A withdrawn item leaves the queue. That tells the user
which decisions are holding up work and keeps an exploration burst from
leaving a stack of stale cards.

**Pushed over ACP.** When the user decides after the hold has expired and
nobody is waiting, the agent would otherwise never learn the path is open.
Deck, the ACP client that showed the card, sends an extension notification
`_deck/boundary` (`{approval_id, destination, outcome, scope}`) to the agent
if the agent advertised it in its `initialize` capabilities `_meta`.
Construct queues it as a short note on its next model request, within the
current turn or the next, and skips it for an id the agent already saw
resolve through `boundary_wait` or `boundary_status`. The notification
informs; it grants nothing and the agent cannot answer it with a decision.

The model gateway's own denials (a model or provider switch) reach Construct
directly as a readable `403`; Construct puts them through the same note and
tools rather than a separate path. The gateway does not inject boundary
notes into model requests itself: it would work for any agent, but it would
change the conversation behind the agent's back, and the agent's transcript
would no longer be what the model saw.

### 7. Grants and revocation

A grant is `(task, gateway, operation, destination, credential_name?)` with a
scope (once/task) or a persisted project rule. Revocation:

- stops new matching requests immediately;
- closes existing matching tunnels and streams; a response already returned
  to the guest is not retracted, and the UI says so;
- is appended to the audit log and recorded before the control reply;
- on task stop or destroy, ends every task-scoped grant and terminates the
  gateway with the holder's process group.

Closing Deck does not widen anything. Pending items stay in the daemon and
expire; saved rules and task grants keep their documented expiry; new scopes
cannot be approved without the UI and fail closed.

### 8. Threat model

**Trusted:** the host runtime (`deckd`, gateway, VMM) as software, the OS
credential store, and the human decision. **Untrusted:** everything in the
guest, including Construct, delegated agents, MCP servers, shell commands,
files, environment, and any bytes on a gateway socket. The VMM is trusted but
constrained: it maps ports and relays bytes; it does not parse HTTP or make
policy decisions. The gateway is trusted but constrained: no shell, no host
filesystem beyond its config, log, resolver and system trust store, a small
environment, resource limits, and a parser that treats every request as
hostile.

In scope: the boundary status endpoint used to read another task's items,
to approve anything, or to flood the queue with withdrawals; bypass through
TSI, host loopback, private networks, DNS, raw IP,
UDP/QUIC, IPv6, alternate ports, redirects, proxy environment changes,
`/etc/hosts`, direct vsock-port access, cross-task addressing, credential
theft from the guest, parser crashes, grant abuse, and audit tampering.

Out of scope: hypervisor or kernel escapes, a compromised guest image or
supply chain, host compromise, a human approving a harmful destination,
side channels, and data-loss prevention. **Approved egress can exfiltrate
project content** — a model provider or an allowed repository receives what
the agent sends. An allowlist is not DLP, and the UI must not imply it is.

### 9. Acceptance cases

The implementation is not done until these have tests where practical.

**Bypass**

1. From the guest, `curl http://127.0.0.1:<host-port>/` (the old TSI model
   port) fails; no host loopback is reachable.
2. `curl http://169.254.169.254/`, `10.0.0.1`, and a private IPv6 literal
   fail; if attempted through the proxy they are denied by address class.
3. Direct DNS and a direct connection to an allowed host fail without the
   relay/proxy; with the proxy, the same host prompts or follows its profile.
4. UDP/QUIC and IPv6 have no route.
5. CONNECT to `host:8443` with no grant asks rather than passing; after
   Allow, `host:8443` passes and `host:8444` still asks. CONNECT to a
   private or loopback address is denied on any port, with no prompt.
6. A redirect from an allowed host to a denied host is denied (plain HTTP)
   or the client's new CONNECT is denied (TLS).
7. Changing proxy environment variables, `/etc/hosts`, or guest firewall
   rules does not change host policy; direct attempts still fail.
8. Connecting straight to a gateway vsock port with a denied destination
   gets the same denial as the relay.
9. A request to another task's port fails; no map exists.
10. A model request for a disallowed model is rejected before any provider
    request; a guest-supplied `Authorization` never reaches the provider.
11. Malformed HTTP, oversized headers, slowloris, and fuzzed git pkt-lines
    get bounded responses; a gateway crash fails the task closed.

**Revocation**

1. Revoking a task grant closes an open tunnel and blocks the next request.
2. Revoking a project rule also revokes matching active grants after an
   explicit confirmation.
3. A policy edit that adds a destination applies to the next connection; one
   that removes a destination closes matching open tunnels. Neither restarts
   the VM.
4. Stop/destroy terminates every tunnel, grant and gateway with the task.
5. Closing Deck leaves saved rules and task grants until expiry and lets
   pending items time out; it never grants.

**Retry**

1. With a window attached, a request needing a new scope is held; Allow
   within the hold completes that same request (a CONNECT tunnel included)
   without a retry.
2. A hold that expires, or a request with no window attached, gets the
   `403` with the approval id; Allow once then lets exactly one matching
   retry through; a second retry prompts again.
3. A held request does not delay other requests on the same port.
4. Allow for task lets matching requests through until stop or expiry.
5. An expired approval is a denial; a later retry creates a new item.
6. A retried shell script is not replayed by Deck; side effects may repeat
   and the transcript shows both attempts.

**Credentials**

1. The real key is absent from the guest environment, Construct config, ACP
   frames, `vmm.json`, console, journal and gateway log.
2. Rotating a key in the credential store changes the next request; no guest
   restart is needed.
3. A denied request injects nothing.
4. A guest-supplied credential header is stripped, not merged.

**Agent feedback**

1. A shell command whose CONNECT is blocked returns a tool result with the
   boundary note naming the destination and approval id, although the tool's
   own output only says the tunnel failed.
2. `boundary_wait` returns `allowed` with its scope when the user allows
   during the wait, `timeout` at its own timeout, and never exceeds the host
   cap; `session/cancel` ends it.
3. While a wait is open the card shows the agent waiting; after
   `boundary_withdraw` the item leaves the queue and the audit names the
   agent.
4. A decision made after the hold expired, with no wait open, reaches the
   agent as `_deck/boundary` and appears once on its next model request; an
   id it already saw resolve is not repeated.
5. No call on the status endpoint can allow an item or read another task's.
6. An agent that did not advertise `_deck/boundary` receives no
   notification and still gets the hold and the `403`.

### 10. Construct and ACP follow-ups

- **Construct:** surface a gateway denial as a distinct, retryable error that
  includes the approval id and the requested scope; bound model retries so a
  denied call does not spin. The model client already has `Endpoint`
  metadata for a readable prefix; extend it rather than adding a second error
  path.
- **Construct:** the boundary note on tool results, the `boundary_wait`,
  `boundary_status` and `boundary_withdraw` tools, the `_deck/boundary`
  capability and its queued note (§6). Their tool descriptions are part of
  the contract: they are what keeps the model from routing around a block.
- **Construct:** keep secretspec environment-first; Deck supplies placeholders.
  `construct secrets set` inside the guest is not a supported path. A guest
  keyring warning is noise, not a working store.
- **Construct:** audit the optional `web_search` and router providers before
  enabling them in a guest. A key in the guest environment is not acceptable,
  even an optional one.
- **ACP:** no ACP method carries a boundary *decision*; the decision stays
  host-side. The one addition is the informational `_deck/boundary`
  extension notification from Deck to an agent that advertises it, which
  grants nothing. ACP permissions remain cooperative; the runtime approval
  is a Deck/runtime event. #13's forwarding
  is the separate cooperative fix and does not replace this boundary.
- **#8** owns the GitHub operation schema, repository/ref rules, PR state and
  the GitHub App credential; **#10** owns private registries and package
  mirrors; **#12** evaluates an IP-networking backend if the vsock-only
  compatibility cost is too high.

### 11. First implementation slice

1. Add `crates/deck-gateway/` with a pure policy core and a model gateway;
   add `deck-egress` to the guest image (loopback relay, model listener).
2. Point the guest's Construct at the relay with a placeholder key; the host
   gateway injects the scripted provider's key.
3. Add the `krun_disable_implicit_vsock` and `krun_add_vsock(ctx, 0)` wrappers
   in `krun.rs`, call both in `vmm.rs`, and map the model gateway's vsock
   port and the reserved extension port (which answers every kind as
   unsupported until one ships); verify the VM prompt test still passes and
   the guest cannot reach the host's model port directly.
4. Add the approval queue with the bounded hold (the gateway learns from
   `deckd` whether a window is attached to the task), model-gateway
   budget/model checks and the boundary card. Queue, lease, hold and card
   landed 2026-11-03; budgets are still open.
5. Add the boundary status kind on the extension port and its relay
   listener, the card's waiting/moved-on state, and `_deck/boundary` in
   Deck; teach `deck-fake-agent` the note, the three tools and the
   notification (ADR 0002 keeps it in step with Construct), then land the
   Construct side.
6. Add the web/package profiles; then hand the transport to #8.

## Deferred

- A virtio-net compatibility backend and any UDP/QUIC support. The guest has
  no IP route in v1; adding one means another enforced backend, not a policy
  exception.
- TLS interception for path-level control of arbitrary HTTPS.
- Private package registries and their credentials (needs a terminating
  gateway or host mirror; #10).
- Persisting pending approvals across a daemon restart (the request already
  failed; the guest retries).
- The SSH-agent kind on the extension port: host-side signing for approved
  SSH destinations, so the user's key never enters the guest.
- An OS service for gateways when no Deck process is ever opened (ADR 0001's
  deferred item).
- Per-destination data-loss controls; approved channels can carry data.
- Boundary feedback for ACP agents other than Construct. The status
  endpoint and `_deck/boundary` are written as a contract so another agent
  can implement them; until one does, it gets the hold and the `403` only.

## Consequences

- The guest loses general networking. Package managers and HTTPS git work
  only through the proxy/relay; tools with their own resolver or raw
  sockets fail. SSH works only through the proxy (`ProxyCommand`) to an
  approved `host:port`, and without the user's keys until the SSH-agent
  kind exists. This is the price of a boundary that does not depend
  on the agent.
- A new host process per task and a guest relay to build and keep updated.
- Construct's guest configuration must use placeholders and the relay; the
  existing TSI shortcut and its VM tests must change in the same slice.
- A gateway crash is a task failure, not a silent loss of egress; the runtime
  must report it as such.
- A VM task's agent runs with Construct's process sandbox off, because the
  guest is the boundary; a local task keeps Construct's configured sandbox.
  Neither backend inherits the other's claim, and the UI must label them
  differently.
- The first request to a new destination waits briefly for a human
  decision, then fails rather than hanging. Construct is told why and
  chooses whether to wait (§6); other ACP agents see only the failure.
- Construct gains three boundary tools and a capability Deck relies on, so
  the two must move together and `deck-fake-agent` must track them.
- Revisit if: an IP backend is needed for compatibility (#12); the gateway
  parser grows enough to justify a separate service; private registries
  become a first-flow requirement; or a provider needs operation-level
  control that the model gateway cannot express.

## Evidence

- #9, #16, #13; `SANDBOX-DESIGN.md` (external boundary, gateways, Minimal and
  Coop notes); ADR 0001 (deckd authority, holder lifetime, gateways) and ADR
  0003 (the libkrun backend and TSI).
- libkrun v1.19.7 `src/libkrun/src/lib.rs`: `krun_add_vsock(ctx, tsi_features)`
  sets an explicit vsock config only after `krun_disable_implicit_vsock` (the
  default is `Implicit` and `krun_add_vsock` returns `EEXIST` otherwise); the
  implicit branch enables `TsiFlags::HIJACK_INET` when no network device
  exists. `krun_add_vsock_port2` maps a guest vsock port to a host Unix
  socket with `listen = false`.
- Construct `8836ed0`: `crates/construct/src/secrets.rs` (env then keyring,
  `env_key` for children), `endpoint.rs` and `llm/mod.rs` (endpoint key and
  headers), `config.rs` (`CONSTRUCT_BASE_URL`, `CONSTRUCT_MODEL` overrides).
- `docs/vm-spike.md`: the current `http://127.0.0.1:<port>/v1` measurement
  path and the TSI note in Limitations.
- First slice (2026-11-03): `crates/deck-gateway/` (policy core, bounded
  HTTP/1.1 carrier, model gateway, extension port), `crates/deck-egress/`
  (guest loopback relay), `krun_disable_implicit_vsock` + `krun_add_vsock(ctx, 0)`
  in `krun.rs`, the gateway process under the VMM with its sockets mapped
  (ACP 1024, model 1026, extension 1027), and the guest image building and
  starting the relay. The runtime passes the guest `CONSTRUCT_MODEL`, a
  non-empty `OPENAI_API_KEY` placeholder and the relay port; the host's
  `CONSTRUCT_BASE_URL` / `CONSTRUCT_MODEL` / `OPENAI_API_KEY` configure the
  gateway instead. Verified on macOS/arm64: the VM lifecycle contract with TSI
  disabled, the scripted prompt round trip through the gateway, and the
  gateway's own tests (guest credential headers stripped, host key injected,
  model/path denials, framed extension refusal). Not yet implemented: the
  approval queue and hold, budgets, grants, `deckd`'s credential broker (the
  key is still the daemon's environment), the boundary status kind and
  `_deck/boundary`, and the web/package gateway. No security validation
  against the bypass cases beyond TSI being off.
- Second slice (2026-11-03): `crates/deck-runtime/src/approvals.rs` (pending
  items, dedup, expiry, once/task/project grants, attachment leases and the
  bounded hold), the daemon's `watch`/`approval`/`approvals`/
  `decide_approval`/`grants`/`revoke_grant` control operations, the gateway's
  `control` client (a disallowed model asks and returns the bounded `403`
  with `approval_id` and `X-Deck-Approval`), the VMM passing the daemon
  socket and task to the gateway, and the window's boundary card and
  decision keys. Verified by approval unit tests, the real daemon over its
  control socket (lease hold, fail-fast without a lease, durable task and
  project grants, stop clearing task grants), the gateway against a fake
  queue (pending/allowed/denied/unreachable, and waiting for a held reply),
  and the window answering a live daemon approval by key. Budget enforcement
  remains unimplemented.
