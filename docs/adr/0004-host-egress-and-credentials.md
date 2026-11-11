# 0004 — Host gateways enforce guest egress, credentials, and approvals

Status: accepted, 2026-11-03; first and second slices landed 2026-11-03;
§4's credential broker amendment accepted and landed 2026-11-04. The scoped
approval reuse and credential-prompt amendment is accepted 2026-11-05;
implementation has started (§11 item 10). Answers #9; the input is
`SANDBOX-DESIGN.md`, ADR 0001 and the #16 spike. #8 owns publication
operations; #12 compares runtimes. Its extension names are superseded in
part by ADR 0005. ADR 0012 amends §3 (one upstream per daemon becomes one
provider per task, from settings) and §4.2 (a value can be entered in the
window).

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

The 2026-11-05 review adds a usability constraint: repeated work within an
approved capability must not require repeated clicks or system passwords.
A model's next prompt or another package URL can use the same authority.
A new destination, credential or publication action may require more.
Short approval reuse must therefore name that authority and its expiration.

The review also reproduced a Once decision releasing two concurrent held
requests. That is an accidental expansion, not a defined grace period.
Separately, the 2026-11-04 Keychain checks found timed-out CLI processes
leaving dialogs behind (§11 item 7). Credential access and operation approval
need separate fixes. Reusing recent authentication, as sudo does, is useful
only while each operation still passes its authorization policy.

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
is unreachable, cached grants continue to their expiry, every new scope is
denied, and every credential fetch fails as unavailable (§4.4), so nothing
new leaves with a host credential. If the gateway dies, the VMM fails the task closed rather than leave
an agent running with half a boundary.

Policy is live. `deckd` pushes a changed project rule or task grant to the
running gateway, and new connections and parsed requests use it without a VM
restart. The web gateway reads the task's grant file and the project's rule
file itself, on every decision and only when their size or mtime changed, so a
revoked destination stops the next attempt; an approval for the task lives in
the daemon's queue, not in the gateway. Removal is stricter than addition: removing a destination or grant
also closes matching open tunnels. For a CONNECT tunnel the gateway knows
only the destination, so that is the granularity; the model gateway parses
each request and applies a changed budget immediately. A model allowlist
change takes effect on the next gateway process, not a running one.

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

A budget belongs to one task and is made of exact counts, not wildcards. The
gateway counts a chat request when it admits it, before any provider request,
and takes tokens from the provider's own `usage` (the last SSE chunk
included). Once either limit is spent the next request gets
`429 deck_budget_exhausted` and no provider request is made. The gateway
reports its totals to `deckd`, which records them on the task, so a resumed
gateway continues from the same counts. The launch limits come from
`DECK_MODEL_REQUEST_BUDGET` and `DECK_MODEL_TOKEN_BUDGET`; a live
`set_budget` control call rewrites `tasks/<id>/budget.json`, which the gateway
re-reads before each admission. That is the one model policy that changes
without a VM restart; the model allowlist stays fixed at process start.

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
  every redirect. A redirect to a destination the task does not hold asks for
  its own scope rather than inheriting the first one; a redirect that is not an
  absolute `http://` URI, or one to a name with no address on the host, is
  refused rather than guessed. The host follows at most five hops.

The gateway resolves names itself and never trusts the guest's resolver or its
`/etc/hosts`. Every address a name returns is classified before a dial: loopback,
private, link-local, shared (`100.64/10`), documentation, multicast, broadcast
and unspecified space is refused on every port and **without asking anyone** —
a grant never makes a private destination reachable. IPv4-mapped and NAT64 forms
are unwrapped first, so `::ffff:127.0.0.1` is loopback, not a public address. A
name with any refused address is refused whole, so a decision cannot depend on
which address connected first. The dial connects to the validated address, and
the guest's `Proxy-Authorization`/`Proxy-Connection` are dropped from a CONNECT
head before it is forwarded: a proxy credential belongs to the guest's proxy
configuration, not to the tunnel's destination. A CONNECT and a plain-HTTP
request are each one connection; a request is at most 32 MiB and a response head
32 KiB, and one task may hold at most 64 tunnels at once.

Named profiles cover repository reads (`github.com`,
`raw.githubusercontent.com`, `codeload.github.com`), registries
(`crates.io`, `static.crates.io`, `index.crates.io`, `registry.npmjs.org`,
`pypi.org`, `files.pythonhosted.org`), OS mirrors, and documentation. A
project can declare profiles and add a destination through an approval that
becomes a project rule. Hostnames are resolved on the host; the guest has no
resolver and no route for one.

The table is compiled into `deck-gateway` (`src/web/profiles.txt`): `repo`,
`registry`, `package-mirrors`, `os-mirrors` and `docs`. The guest may name a
profile, never define one, and an undeclared name is a gateway startup error. A
task gets `repo`, `registry`, `package-mirrors` and `docs` by default and not
the OS mirrors; `DECK_WEB_PROFILES` names another set, and `none` starts no web
gateway at all. A profile entry is one name (`host`) or one port (`host:port`).
It pre-approves a CONNECT to that name on 443 (or on the entry's own port), and
a plain-HTTP mirror read, but never another port: `crates.io:8443` asks. A
profile the task was not launched with is approved as a whole for the project
(`web_profile { destinations }` on the control socket, expanded by the daemon
from the host's table), which writes one project rule per destination, so a
package manager does not ask once per package. Revoking that decision removes
exactly those rules; the gateway sees the change on its next read.

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
  image or an exported task disk. §4.2 settles both.
- **Guest values.** The guest receives a non-empty placeholder for each
  secret Construct requires, so secretspec resolves it from the environment
  and skips the keyring. `CONSTRUCT_BASE_URL` points at the guest relay;
  `CONSTRUCT_MODEL` names the task's model. No real value appears in
  `vmm.json`, the console, the journal, an ACP frame, or a log.
- **Injection.** `deckd` hands the gateway a credential value over the host
  control socket for one operation. The gateway keeps it in memory for that
  request and never writes it. The guest's own `Authorization` is discarded.
  §4.3 and §4.4 settle who may ask and how.
- **Headers from Construct.** Construct sends `[headers]` with every model
  request. The guest config is agent-writable, so the gateway treats those
  headers as untrusted: only the attribution allowlist is forwarded; every
  other header is dropped before the host credential is added.
- **Rotation and revocation.** Replacing a value in the credential store
  affects the next request. Revoking a credential closes open tunnels and
  in-flight operations that used it and records the revocation. Task records
  and project rules store credential *names* and fingerprints, never values.
  §4.1 and §4.5 settle both.
- **Delegated children.** Construct hands a child only the environment its
  agent definition names (`env_key`). Deck supplies placeholders to the
  parent, so a child that asks for a different provider still reaches the
  gateway under the same task policy; it cannot read the host credential.

The rest of this section is `deckd`'s credential broker (amended
2026-11-04). Before it, the model key and `DECK_GITHUB_TOKEN` came from the
daemon's environment, which the VMM and gateway inherited, and the gateway
read them once at start (ADR 0007 §4). The broker replaced that source in one
slice (§11 item 7); there was no period where both were read.

#### 4.1 Names and fingerprints

A credential is addressed by a name, `[A-Z][A-Z0-9_]{0,63}`: the same shape
as the environment variables it replaces (`OPENAI_API_KEY`,
`DECK_GITHUB_TOKEN`). A name is configuration. It may appear in `vmm.json`,
arguments, logs, grants, task events and the UI.

A value exists only in the store, in `deckd`'s memory while it answers one
request, in the gateway's memory for one operation, and on that operation's
outbound request. Any record that must tell two values apart uses the
fingerprint: `sha256:` and the first 12 hex digits of SHA-256 over the value.
That distinguishes machine-generated keys and tokens without saying anything
useful about them. It is not a verifier, and a value a person could guess (a
password) is not something Deck stores.

Grants and project rules name a credential and never a fingerprint, so
rotation keeps them (§7). Fingerprints appear in task events, revocation
records and the access panel.

#### 4.2 Storage

`deckd` uses exactly one store, chosen when it starts:

- **A file**, when `DECK_CREDENTIALS` names one: a JSON object of name to
  value. `deckd` refuses a path that is not a regular file owned by the user
  with mode 0600 (no group or other bits), and logs why. While the variable
  is set, the file is the only store. It is never merged with the OS store,
  so "which source won" has one answer. This is the headless, CI and test
  path.
- **The OS store** otherwise: a macOS Keychain generic password (service
  `com.sprawlcommons.deck`, account = the name), or a Linux Secret Service
  item in the default collection (attributes `service=com.sprawlcommons.deck`,
  `name=<name>`).

`deckd` never reads a credential value from its own environment. The runtime
removes every name in the task's credential set (§4.3) from the environment
it hands the VMM and the gateway. A stray `export OPENAI_API_KEY=…` in the
shell that started the daemon therefore cannot reach a holder.

A store read is bounded at 5 s. The bound is needed because Keychain may show
an access dialog: an ad-hoc signed `deckd` is a new application to Keychain
after every rebuild, because its designated requirement is its cdhash.
Secret Service may ask to unlock a collection, or have no session bus when
the daemon outlived the session that started it. A read that misses the
bound, or a store that cannot be reached, is *unavailable*, not *missing*.
At most one read per name is outstanding: later requests wait on it within
their own bound, and an answered dialog serves whoever asks next.

The store is managed from the `deckd` binary, so the Keychain access list
names the same application that reads the item:

```text
deckd credential set NAME      # value on stdin, never an argument; no echo on a tty
deckd credential delete NAME
deckd credential list          # names, fingerprints, store, revoked state; never values
deckd credential revoke NAME   # a control request to the running daemon (§4.5)
```

`set` on the file store rewrites the file atomically at 0600. Deck's access
panel lists names and fingerprints and offers revoke. Entering a value in
the UI is deferred.

The panel asks the daemon for `credentials {task}`, which lists only the
names in that task's set (so a window causes no read of any other item) with
fingerprint and revoked state, never a value. Revoke asks for confirmation,
sends `revoke_credential`, and shows the new listing from the daemon's reply;
it then says the key is not cancelled at the provider (§4.5).

**Credential prompts and operation approvals are separate.** macOS or Secret
Service authorizes `deckd` to read a stored credential. Deck authorizes a task
to use a capability. An ordinary boundary decision is a Deck UI action and
does not itself require the user's system password. Permission to read a key
does not authorize another task, destination or operation.

Credential reads, including `deckd credential list`, go through the running
daemon's store and share its one outstanding read per name. A CLI invocation
must not create an independent store read or silently fall back to one when
the daemon is unavailable. A caller's timeout does not end the daemon's
outstanding read, so a later caller can join it and receive its answer.
Automatic retries and panel refreshes must not create parallel OS dialogs
for the same item. After the user denies access, automatic retries do not
reopen the dialog; Deck reports the failure and offers an explicit retry.

A distributed macOS `deckd` uses a stable signing identity and supports
Keychain trust for that application and item. Trust is never broadened to
all applications. An ad-hoc development rebuild may still need new consent;
the file store remains the development and CI path. These requirements do
not add a credential-value cache: each operation still reads the store, so
rotation affects the next operation (§4.4). Reusing OS access consent and
reusing a Deck grant are separate from retaining a secret value.

#### 4.3 Which task may use which credential

When the runtime starts a VMM, it writes the task's credential set into
`vmm.json` as `gateway.credentials` (names only): the model key's name, and
the GitHub token's name when the project names a repository. `deckd` answers
a `credential` request only for a name in the requesting task's set, and only
while that task has a live holder. Any other request is refused and recorded,
and the store is not read.

This gives least privilege and protects against a confused gateway. It is
not isolation from a compromised one. The gateway runs as the user, so until
it runs confined as §8 describes, a gateway taken over through its parser has
the user's file access, including the file store and other tasks' records.
This slice claims no more than that.

#### 4.4 Injection

The gateway sends `{"op":"credential","task":…,"name":…}` on the control
socket after policy has admitted the operation and just before the outbound
request. A denied or pending request therefore fetches nothing. The reply is
`{"value":…,"fingerprint":…}`, with room for an `expires_at` that a minted
token will use (§4.6).

The value serves one operation and is then dropped. An operation is one
model request including its streamed response, or one GitHub operation (a
publish's `git push` and its API calls share one fetch). Neither side logs
the reply. `deckd` logs the task, the name and the fingerprint.

Credential values are not cached in the gateway or in `deckd`: each operation
reads the store, joining an outstanding read for that name when one exists.
Rotation therefore needs no extra invalidation machinery. A credential-value
cache would require a separate decision about rotation visibility and
invalidation; approval reuse is not a reason to add one.

Each failure injects nothing and reaches the guest in the gateway's existing
bounded error shape. ADR 0007's GitHub table gains the same rows.

| Case | Status | Code | Retryable by the agent |
|---|---|---|---|
| The store has no value under the name | 503 | `deck_credential_missing` | no; the user's to fix |
| `deckd` or the store unreachable, or the read timed out | 503 | `deck_credential_unavailable` | yes |
| The name is not in the task's set | 403 | `deck_credential_refused` | no |
| The current value's fingerprint is revoked | 403 | `deck_credential_revoked` | no; the user's to fix |

The first time a gateway process uses a fingerprint, `deckd` appends
`credential_used {name, fingerprint}` to the task's events. The task record
then says which value the task ran under, without an event per request.

#### 4.5 Rotation and revocation

Rotating means replacing the value, with `deckd credential set` or by
editing the item in Keychain Access. The next operation reads the new value.
Operations already running keep the value they started with.

Revoking is for a value that must stop being used now, because it leaked or
was withdrawn at the provider. `revoke_credential {name}`, from the CLI or
the access panel, revokes the name's *current fingerprint*:

1. `deckd` appends the revocation (name, fingerprint, time) to
   `credentials/revoked.json` under the data root and to the audit record
   before it replies.
2. Every later `credential` request whose current value has a revoked
   fingerprint is refused with `deck_credential_revoked`. The name comes back
   when a new value is set. A fingerprint cannot be un-revoked.
3. Each running gateway holds a `credential_watch {task}` control connection,
   opened at start and reopened with backoff. It opens with the revoked
   fingerprints for the task's set, and `deckd` pushes each new revocation on
   it. The gateway registers every in-flight operation by (name,
   fingerprint) and closes the matching ones. A model request whose response
   has not started is answered `deck_credential_revoked`. One that is
   streaming has its upstream and guest connections shut down. A GitHub
   operation's `git` process group is killed and its API call abandoned. A
   ref already pushed or a response already delivered is not retracted, and
   the UI says so (§7).
4. While the watch is down, `deckd` is down, so no new fetch succeeds.
   Operations already in flight finish. On reconnect, the snapshot catches
   anything revoked in the gap.

Revoking does not delete the stored value (`delete` does that) and revokes
nothing at the provider. The UI tells the user to rotate it there.

#### 4.6 Not in this slice

- **GitHub App installation tokens** (ADR 0007 §4). The broker would mint
  them from the App's private key, itself a stored credential, and cache
  each until its hourly expiry. The reply's `expires_at` is the hook. Until
  then, `DECK_GITHUB_TOKEN` is an ordinary stored credential.
- **Registry tokens and the SSH-agent kind** (Deferred) use the same names,
  fetch and revocation when they arrive.
- **The local backend** is unchanged: Construct resolves its own secrets
  there, and the broker serves gateways only.

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

The options distinguish one operation from reusable authority:

- **Allow once** authorizes exactly one operation, including its internal
  steps. If no held request claims it, one matching retry may claim it
  within 60 s. Claiming the allowance is atomic. Deduplicating an approval
  card never lets several concurrent requests consume that one allowance.
  Other requests need another grant or a new approval.
- **Allow for 5 minutes** authorizes repeated matching operations for this
  task until a fixed host-controlled deadline, five minutes after the
  decision. Traffic, retries and agent requests cannot extend the deadline.
- **Allow for this task** authorizes matching operations until the task
  stops or the displayed grant expiry, currently 12 h after the decision.
- **Always allow for this project** creates a persisted project rule.
  Rules live in Deck's data root keyed by project identity, not in the
  repository, so the guest cannot edit its own policy.
- **Deny** refuses the operation; the agent sees a structured error.

Five minutes is the initial product choice for short approval reuse, not a
security constant. The UI displays the scope and expiry before the decision
and in Access afterward. Routine model and package work can use a timed or
task allowance without another click for each request. A Once button must
never silently act as a timed allowance.

**Matching means the same authority.** Each request is checked against the
grant even when no new human decision is needed. A reusable grant names its
task or project, gateway, operation, destination and credential name if any.
Provider, repository and ref restrictions still apply. The gateway defines
the authority its operation requires:

| Gateway | What reuse must preserve | Changes that need no new approval within that authority |
|---|---|---|
| Model | Configured provider, allowed model, operation and credential name | Prompts and request sizes, subject to the task's budgets |
| Web | Approved host, port and operation or mirror policy | URLs within a permitted mirror; bytes in an admitted CONNECT tunnel remain opaque |
| GitHub | Repository, operation, approved branch or PR, and ADR 0007's ref rules | Repeated uses allowed by the selected grant; no additional repository, ref or action |

A different method, path or body size is not automatically a new permission.
If that change requires another operation or more authority, it asks. A new
credential name also asks; rotating the value under the same name preserves
the grant (§7). All allowances remain subject to budgets, connection limits,
address checks and explicit revocation. No reuse window overrides a refusal.

Once uses the gateway's identity for the reviewed operation rather than a
generic time window. For a write, that identity includes the validated target
and content the user approved, such as a publish's branch and commit or a
PR change's target and requested edits. A changed write must ask again.
Fingerprints may identify content without persisting bodies or secrets in
the approval or audit records. Reusable grants deliberately cover more than
one request body; they do not require identical bytes on every use.

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
failed, and the guest's retry creates a fresh item. Saved rules, timed grants
and task grants are persisted. A timed grant retains its original deadline
across daemon restarts and window reconnects; it never restarts its timer.

Control surface: the daemon control protocol is additive. A window opens a
`watch {task}` connection, which is both its notice stream for that task and
its attachment lease; Deck gets `approvals` (pending items for a task),
`decide_approval`, `grants` and `revoke_grant` on ordinary control requests.
The gateway asks `approval` with the observed request and the daemon holds
that call. The gateway's `credential` request returns one value for one
operation, and its `credential_watch` connection carries revocations
(§4.4, §4.5). The CLI and the access panel send `revoke_credential`.
Neither a `credential` request nor its reply is logged. Task grants are `tasks/<id>/grants.json`, project rules
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

**Back to the user.** `deckd` knows when a wait is open on an item because
the gateway's long poll is a bounded daemon call, so the boundary card says
**agent is waiting** or **agent moved on**. A withdrawn item leaves the queue. That tells the user
which decisions are holding up work and keeps an exploration burst from
leaving a stack of stale cards.

**Pushed over ACP.** When the user decides after the hold has expired and
nobody is waiting, the agent would otherwise never learn the path is open.
Deck, the ACP client that showed the card, sends an extension notification
`_com.sprawlcommons/deck/boundary`
(`{approval_id, destination, outcome, scope}`) to the agent if the agent
advertised it in its `initialize` capabilities
`_meta["com.sprawlcommons/deck"]` (ADR 0005); the bare `_deck/boundary` and
`_meta.deck` a released Construct advertised still count.
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
scope of once, timed or task, or a persisted project rule. It names a
credential, never a fingerprint, so rotating the value keeps the grant.
Revoking a credential is §4.5 and is separate from revoking a grant.
Revoking a grant:

- stops new matching requests immediately;
- closes existing matching tunnels and streams; a response already returned
  to the guest is not retracted, and the UI says so;
- is appended to the audit log and recorded before the control reply;
- on task stop or destroy, ends every task-scoped grant and terminates the
  gateway with the holder's process group.

Timed grants belong to the task and end on stop or destroy as task grants
do. Their expiry follows the existing grant-expiry behavior: no new matching
operation is admitted and active operations authorized by the expired grant
are closed. Expiry is enforced by the gateway even while `deckd` is down.
Consuming a Once allowance prevents a second admission; it does not cancel
the operation that claimed it. Credential revocation still stops that
operation. A timed or task allowance does not authorize rollback of an
external change already completed.

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
12. A CONNECT or plain-HTTP destination that resolves to loopback, private,
    link-local, shared, documentation, multicast or unspecified space is
    refused on any port, with no item and no dial — a grant does not override
    it. An IPv4-mapped or NAT64 form of a refused address is refused too.
    (`deck-gateway` `web::policy`, `web::resolve`, and the real binary.)
13. A plain-HTTP request to a mirror the task does not hold is refused or held
    before any dial; a request to a held mirror is forwarded in origin form
    without the guest's proxy credential. (`gateway.rs`: mirror read,
    ungranted mirror.)
14. A redirect is a destination of its own: to a name and port the task does
    not hold it is refused and the second name is never contacted; a
    non-`http://` or unresolvable redirect target is refused. (`gateway.rs`:
    the redirect case.)
15. A whole-profile approval writes one project rule per destination, is
    idempotent, and revoking it removes exactly those destinations; a task
    grant carries an expiry. (`approvals` unit tests, `tests/daemon.rs`.)
16. A web denial reaches the agent through the boundary status endpoint's
    `/events`, beside the model's, with `gateway: "web"`. (`gateway.rs`: the
    boundary events case.)

**Budgets**

1. Each admitted chat request increments the task's request count before the
   provider is contacted. A request over the request limit, or one made after
   the token total reached its limit, gets `429 deck_budget_exhausted` and
   makes no provider request. Tokens are the provider's `usage.total_tokens`
   (or prompt plus completion), read from a JSON body or the last SSE chunk,
   and both totals appear on the task record. Changing a task's budget applies
   to its next request without restarting the VM.

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
7. Several concurrent held requests may share a card. One Once decision
   admits at most one operation, including when decision, hold timeout and
   cancellation race. If an eligible held request remains, it can claim the
   allowance. Unclaimed Once authority expires after 60 s.
8. A Once retry with a different credential name or changed reviewed write
   target or content cannot consume the original allowance.
9. A five-minute decision admits repeated matching requests without another
   prompt. Requests for another task, destination, model, credential name
   or operation still need their own authority. Different prompts or mirror
   paths within the approved authority do not ask again.
10. Traffic and reconnects do not extend a timed grant. Expiry closes matching
    active operations and blocks new admissions, including during a daemon
    outage. Unrelated operations remain open.
11. The boundary card and Access panel display the reusable scope and its
    expiry. Revocation closes matching operations before the window expires.

**Credentials**

1. The real key is absent from the guest environment, Construct config, ACP
   frames, `vmm.json`, console, journal and gateway log.
2. Rotating a key in the credential store changes the next request; no guest
   restart is needed.
3. A denied request injects nothing.
4. A guest-supplied credential header is stripped, not merged.
5. With the credential exported in the shell that starts `deckd`, it is
   absent from the VMM's and the gateway's environment
   (`/proc/<pid>/environ`, `ps eww`), and the task still works from the store.
6. A gateway asking for a name outside its task's set gets
   `deck_credential_refused`, and the store is not read.
7. With `deckd` stopped, a model request returns
   `deck_credential_unavailable` and reaches no provider. After a restart,
   the next request succeeds with no task restart.
8. Revoking during a streamed model response shuts that stream, refuses the
   next request with `deck_credential_revoked`, and records the name and
   fingerprint. Setting a new value restores service.
9. A file store with group or other permission bits is refused at start.
10. Concurrent gateway reads, UI listings and CLI listings for one credential
    share a daemon read and cause at most one outstanding OS access dialog.
    A caller timing out or exiting does not orphan that read or cause the
    next caller to open another dialog. Its eventual answer serves a caller
    still waiting; a later operation reads the store again.
11. OS-store denial does not cause an automatic retry dialog loop. An
    explicit user retry can try again. Deck boundary approval does not
    request a system password unless the separate store access requires it.
12. An authorized, consistently signed macOS daemon can perform successive
    admitted operations without repeated Keychain consent. Another task or
    expanded operation scope still requires its own Deck authority.

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
   agent as `_com.sprawlcommons/deck/boundary` (the bare `_deck/boundary`
   during the transition) and appears once on its next model request; an id
   it already saw resolve is not repeated.
5. No call on the status endpoint can allow an item or read another task's.
6. An agent that did not advertise the boundary capability receives no
   notification and still gets the hold and the `403`.

### 10. Construct and ACP follow-ups

- **Construct:** surface a gateway denial as a distinct, retryable error that
  includes the approval id and the requested scope; bound model retries so a
  denied call does not spin. The model client already has `Endpoint`
  metadata for a readable prefix; extend it rather than adding a second error
  path.
- **Construct:** the boundary note on tool results, the `boundary_wait`,
  `boundary_status` and `boundary_withdraw` tools, the
  `_com.sprawlcommons/deck/boundary` capability and its queued note (§6).
  Their tool descriptions are part of the contract: they are what keeps the
  model from routing around a block.
- **Construct:** keep secretspec environment-first; Deck supplies placeholders.
  `construct secrets set` inside the guest is not a supported path. A guest
  keyring warning is noise, not a working store.
- **Construct:** audit the optional `web_search` and router providers before
  enabling them in a guest. A key in the guest environment is not acceptable,
  even an optional one.
- **ACP:** no ACP method carries a boundary *decision*; the decision stays
  host-side. The one addition is the informational
  `_com.sprawlcommons/deck/boundary` extension notification from Deck to an
  agent that advertises it, which
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
   budget/model checks and the boundary card. Queue, lease, hold, card and
   budgets landed 2026-11-03.
5. Add the boundary status kind on the extension port and its relay
   listener, the card's waiting/moved-on state, and the boundary
   notification in Deck; teach `deck-fake-agent` the note, the three tools
   and the notification (ADR 0002 keeps it in step with Construct), then
   land the Construct side.

   Landed 2026-11-03. The gateway serves `boundary` on the
   extension port: `/events`, long-polled `/approvals/<id>` (300 s host cap)
   and `/approvals/<id>/withdraw`. A long poll is one `approval_wait` control
   call that `deckd` holds, counting the wait on the item, so the card says
   the agent is waiting or has moved on without the gateway polling.
   Deck sends `_com.sprawlcommons/deck/boundary` to an agent that advertised
   `_meta["com.sprawlcommons/deck"].boundary` (or the bare names, ADR 0005)
   when a decision lands after no wait was open. The
   guest image runs a second `deck-egress` with `--kind boundary` and exports
   `DECK_BOUNDARY_URL`; the VMM maps the same extension port as before.
   `deck-fake-agent` advertises the capability when that URL is set and puts
   the note in its transcript. The three boundary tools and the tool-result
   note are Construct's side, merged in its `deck/boundary-feedback` work.
6. Add the web/package profiles; then hand the transport to #8.

   Handed over. ADR 0007 settles #8's slice: a bounded GitHub operation
   gateway on its own vsock port (1029) and the `github` extension kind, a git
   bundle publish with the ref rules enforced on the host, the five-operation
   set with merge refused by name, and `github_profile` for a whole-profile
   decision.

   Landed 2026-11-03. `deck-gateway`'s `web` module is the address policy
   (`policy`), bounded host resolution (`resolve`), the opaque CONNECT tunnel
   (`connect`), the plain-mirror proxy with a checked redirect chain
   (`mirror`), its own boundary events (`status`) and its daemon client
   (`control`). It listens on a dedicated socket (vsock 1028, a separate port
   because a client's proxy setting cannot send the extension port's framed
   kind) and on the extension port as the `web` kind. The VM backend starts it
   for every task and passes `--web-profile`, `--web-grants` (the task's
   `grants.json`, then the project's `approvals/projects.json`) and the
   compiled-in profile table; `DECK_WEB_PROFILES` names the profiles (default
   `repo,registry,package-mirrors,docs`; `none` serves no web traffic). The
   guest image starts two loopback relays over that port and exports
   `http_proxy`/`https_proxy` for the task, only when the gateway exists.
   Task-scoped grants now carry a 12 h expiry, and a whole-profile decision is
   `web_profile { destinations }` on the control socket, expanded by the daemon
   and written as project rules. Verified by policy/resolver/tunnel/mirror unit
   tests, the real gateway binary against an echoing local listener (address
   refusals with no dial and no item, a profile-covered CONNECT, a granted
   tunnel carrying bytes both ways, another port refused, a mirror read
   forwarded without the guest's proxy credential, a redirect to another name
   denied and never contacted, a web denial visible in the boundary events,
   an undeclared profile refusing to start), and the real daemon (profile rules
   written, re-approval idempotent, one destination revoked leaving its
   sibling). A public destination is reached in a test through
   `--web-test-connect`, which redirects only the dial after the policy
   admitted the resolved address; the binary never passes it. Not covered: a
   real TLS CONNECT to a public host, and SSH through the relay, which need a
   live VM and a network the CI host does not have.
7. Add the credential broker (§4.1–§4.5): the store module in
   `deck-runtime` (file and OS store, the bounded read), the `deckd
   credential` commands, the daemon's `credential`, `credential_watch` and
   `revoke_credential` requests, `gateway.credentials` in `vmm.json`, and
   credential names stripped from the VMM's environment. The gateway then
   fetches per operation instead of reading its environment, and keeps an
   in-flight registry that revocation closes. The VM tests and `AGENTS.md`
   move from `OPENAI_API_KEY` in the environment to a `DECK_CREDENTIALS`
   file in the same change. CI covers the file store. The Keychain and
   Secret Service paths are verified by hand, like the VM tests.

   Landed 2026-11-04. `deck-runtime`'s `credentials` module is the store
   (`file`, `os` over the Security framework on macOS and `secret-tool` on
   Linux, the 5 s single-flight read, `revoked`) and the `deckd credential`
   commands; `broker` checks the task's `vmm.json` set before any read,
   refuses revoked fingerprints after it, records `credential_used` once per
   run, and pushes revocations to each gateway's watch. The gateway's
   `credential` module fetches per operation after policy (and after the
   budget check), registers each running operation, and on revocation
   answers a model request that has not started with
   `deck_credential_revoked`, shuts one that is streaming, and kills a
   GitHub operation's `git` process group. Verified by store, broker and
   gateway unit tests; the real daemon (served only to a running task that
   names the credential, refused otherwise without a read, `credential_used`
   once, a watch snapshot and push, revoked then restored by a new value, a
   group-readable file refusing start, the CLI taking values on stdin and
   listing only fingerprints); and the real gateway behind a fake daemon
   (rotation per request with a decoy value in its own environment, nothing
   fetched for a denied or budget-refused request, unavailable with `deckd`
   down and recovered without a restart, a stream shut by revocation and the
   next request refused, a revoked GitHub token refused before GitHub and
   restored by a new value). The VM prompt test runs under its own `deckd`
   with a `DECK_CREDENTIALS` file and asserts that a key exported to that
   daemon is absent from the VMM's and gateway's environment. Not covered in
   CI: the Keychain and Secret Service stores and the VM tests, which are run
   by hand.

   Checked by hand 2026-11-04 on macOS/arm64 (libkrun 1.19.7): the VM prompt
   test passes with a `DECK_CREDENTIALS` file, the upstream receiving the
   stored value. Revoking during a streaming VM turn closes the stream within
   ~45 ms (file store and Keychain), and the next turn gets
   `deck_credential_revoked` without reaching the model. In the Keychain, the
   `deckd` that set the item reads it without a dialog. A differently signed
   `deckd` daemon gets one dialog; its first fetch is `unavailable` after 5 s,
   the read stays in flight, and once the dialog is allowed the next fetch in
   the same turn succeeds, after which that binary reads without asking. Not
   yet checked: the Secret Service store. Two rough edges: a short-lived
   `deckd credential list` that times out leaves its dialog behind, and an
   answer given after it exits is discarded, so repeated CLI reads pile up
   dialogs. And a stream closed by revocation reaches the agent as a generic
   interrupted stream, not `deck_credential_revoked`; only the next request
   names it.

8. Close active web connections on grant revocation (§7). The gateway keeps
   the exact authorizing rule for each CONNECT tunnel and plain-HTTP stream.
   One monitor checks changed policy files and grant expiry every 50 ms,
   closing both sockets when that rule is removed or expires. It works while
   `deckd` is down and leaves unrelated connections open. An allow-once
   operation continues after its allowance is consumed; launch profiles stay
   fixed. The VMM passes the host project identity as `--web-project`, and
   the gateway selects only its task's grants and that project's rules.

   Task-grant and project-rule revokes append the removed grant and time to
   `approvals/audit.jsonl` and fsync before a successful control reply. A
   failed audit write reports an error without restoring access. Verified by
   real gateway socket tests for task and project removal, daemon outage,
   expiry, mirror response cancellation, unrelated traffic and allow-once;
   grant-selection and admission-race unit tests; and real daemon tests for
   persisted removal, audit contents and a new request needing approval.
   The Access panel lists live task allowances and the task's saved project
   rules, with scope and expiry, and pending requests that return to the
   boundary card for review. A revoke confirms its scope before asking the
   daemon. Revoking a project rule also removes matching task allowances
   throughout that project. Listings and revokes are filtered by the task's
   host project identity; another project's rules are not exposed. The view
   updates from daemon replies and discards stale listings. GPUI tests drive
   real clicks through confirmation, cancellation, task and project removal,
   and pending-request review against a real daemon. Checked in a live macOS
   window through `DECK_CONTROL`: prompt entry, ACP permission handling,
   opening Access, cancellation and confirmed task-grant revocation, with
   removal verified at the daemon. Window-only screenshots were inspected.
   Project-rule removal was verified headlessly, not in this live pass.
   Model and GitHub
   grant revocation during an operation are not covered by this web slice;
   credential revocation for those operations is §4.5.

9. Close active model requests on grant revocation (§7). The shared gateway
   grant monitor now retains exact task/project model rules as well as web
   rules. Each model request owns its HTTP client pool. Revocation shuts
   that pool and the guest socket, interrupting idle header and body reads.
   Before response commitment the guest receives `deck_boundary_denied`;
   after commitment it sees an interrupted stream. Credential revocation uses the same
   request cancellation, preserving `deck_credential_revoked` before headers.
   Unrelated model grants, configured launch models and admitted once
   requests continue. Request and token accounting keep their existing rules.

   The VMM passes `--grants` and `--project` outside optional web configuration,
   so model grants work with web disabled. Saved VMM records using the old
   `web_grants` and `web_project` fields still load. Real gateway socket tests
   cover idle provider closure before and after headers, isolated streams,
   once consumption, expiry with the daemon down, and missing durable grants.
   GitHub operations admitted by a persisted timed, task or project rule take
   part too: the gateway holds that exact rule, and deleting or expiring it
   interrupts the operation's `git` and API calls with `deck_boundary_denied`
   ("the authorizing GitHub grant was revoked or expired"). A rule already
   gone when the operation would dial refuses it before GitHub, and removing
   an unrelated rule leaves the operation running. Real-socket cases in
   `crates/deck-gateway/tests/gateway.rs` cover deletion, expiry and the
   unrelated rule.

10. Implement scoped approval reuse and separate credential-prompt handling.
    Accepted 2026-11-05; Once admission and review matching implemented 2026-11-05. The
    remaining work below is pending. This amendment changes the target
    behavior in §§4.2, 5 and 7, not the historical results above.

    - Once decisions now create one allowance whether or not requests are
      held. Each admission consumes it under the queue lock. Other held
      requests share a fresh pending card and keep their original hold
      deadline. Runtime and real-daemon regressions cover concurrent requests;
      runtime tests also cover late retries, expiry and clearing a held task.
      A disconnected caller leaves the allowance for a live request or retry;
      a client that closes only its write half can still receive the reply.
    - Once matching also preserves the credential name and the host gateway's
      reviewed-write fingerprint. GitHub fingerprints include the validated
      branch and commit for publish, or the PR target and effective edits.
      Identical retries share authority; changed commits, PR edits and
      credential names ask again. Model and web requests retain their
      gateway/operation/destination authority without requiring identical
      prompt bodies or mirror paths. An older GitHub gateway without a
      fingerprint must restart its task before a Once decision can apply.
      Runtime and real-gateway tests cover these identities. These fields are
      private daemon control metadata; ACP notifications are unchanged.
    - The five-minute grant landed in PR #75, coordinated with Construct
      #183 and released contract 1.1.0. It reaches persistence, gateway
      monitors, boundary card, Access and agent-visible status/notifications.
      Live Linux testing found missing persisted task identity (#76), a
      credential-name matching gap (#77), and a VM ACP working-directory
      mismatch (#78). With the local #76 fix, real five-minute expiry closes
      model/web operations during daemon outage without deadline renewal;
      unrelated traffic survives. UI/Construct feedback requires the
      documented path fixture for #78. This slice is not fully accepted:
      [live Linux report](../acceptance/2026-11-06-timed-linux.md).
    - The follow-up for #77 persists the credential name for all decision
      scopes and checks it at runtime and cached gateway admission. Legacy
      rules without a name authorize only credential-free requests; credential
      rules require renewed approval rather than inferred migration. Profile
      decisions bind the configured host credential name. The #78 window fix
      addresses VM ACP with `/workspace` while retaining host project identity.
      The #71 classifier requires an entirely empty successful Secret Service
      search to prove absence. Targeted live results and limits:
      [bug-fix report](../acceptance/2026-11-06-live-bug-fixes.md).
    - CLI credential listings use the daemon's shared store (PR #60).
      The host retry slice retains cancelled/refused Keychain reads per name for
      the current daemon lifetime and adds `deckd credential retry NAME`.
      Retry clears only a completed refusal, never a pending flight, task
      authority or revocation. Runtime tests cover automatic-read suppression,
      concurrent retry, late refusal after timeout, transient recovery and
      fresh values; daemon tests cover CLI routing and unchanged revocation.
      macOS maps `errSecUserCanceled` and `errSecAuthFailed` to this pause;
      interaction-unavailable and Linux locked-store errors remain transient.
      Apple's [cancellation](https://developer.apple.com/documentation/security/errSecUserCanceled)
      and [authentication failure](https://developer.apple.com/documentation/security/errSecAuthFailed)
      statuses distinguish those cases. The Access panel offers the same retry
      for a paused credential. Interactive Linux refusal classification and
      live Keychain verification with the supported signing identity remain
      pending. No ACP field changes are introduced by the host-only
      CLI/control operation.

    The timed scope reaches agent-visible boundary status and notifications.
    Names and payloads are settled in released shared contract 1.1.0 §5.4.
    Deck and Construct pin the same release and checksum (ADRs 0005 and
    0006). The additive scope/expiry fields retain both 1.x method names.

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
  endpoint and the boundary notification are written as a contract so
  another agent can implement them; until one does, it gets the hold and
  the `403` only.

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
- Repeated work within a timed or task allowance needs no repeated approval.
  Every operation still passes host policy. The UI makes broader reuse
  explicit, and the user can revoke it before expiry. A five-minute grant
  also closes operations still running at expiry, as existing task expiry
  does; the displayed deadline makes that interruption predictable.
- Credentials leave the environment. A value is set once with `deckd
  credential set` (or a 0600 file for headless hosts and CI) instead of
  exported, and every model request or GitHub operation costs a control
  round trip and a store read. A rebuilt, ad-hoc signed `deckd` meets a
  Keychain access dialog once per item; development use takes the file
  store. Until the gateway runs confined, the broker limits each task to its
  own names but cannot keep a compromised gateway from the user's files.
- Construct gains three boundary tools and a capability Deck relies on, so
  the two must move together and `deck-fake-agent` must track them.
- Revisit if: an IP backend is needed for compatibility (#12); the gateway
  parser grows enough to justify a separate service; private registries
  become a first-flow requirement; or a provider needs operation-level
  control that the model gateway cannot express.

## Evidence

- Scoped reuse amendment accepted by the project owner, 2026-11-05. Review
  at `560b9e9` reproduced one Once decision admitting two of two concurrent
  held requests in five rounds through the public `Boundary` API. Sequential
  retries admitted one request only. A changed credential name, method, path
  and size still matched a late Once allowance. Sources:
  `crates/deck-runtime/src/approvals.rs` and `approvals/types.rs` at that commit.
  These observations are not permanent regression tests; §11 item 10 adds
  those tests as implementation work.
- [PR #35](https://github.com/strf0x1/deck/pull/35) introduced the approval
  queue on 2026-11-03. [PR #47](https://github.com/strf0x1/deck/pull/47)
  recorded the Keychain CLI dialog problem on 2026-11-04. The recorded Once
  behavior predates that observation.
- [Apple's Keychain access options](https://support.apple.com/en-gb/guide/mac-help/kychn002/26/mac/26)
  distinguish access once from ongoing trust for an application and item.
  [The sudoers manual source](https://github.com/sudo-project/sudo/blob/main/docs/sudoers.mdoc.in)
  describes authentication timestamp reuse separately from command policy.
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
  approval queue and hold, grants, `deckd`'s credential broker (the
  key is still the daemon's environment), the boundary status kind and
  `_com.sprawlcommons/deck/boundary`, and the web/package gateway. No
  security validation against the bypass cases beyond TSI being off.
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
  and the window answering a live daemon approval by key.
- Fourth slice (2026-11-03): the web and package gateway — CONNECT with
  host-side resolution and address-class rejection, per-port grants, plain HTTP
  for declared mirrors with every redirect re-checked, profiles, a dedicated
  vsock port and the `web` extension kind, the guest's two relays and proxy
  environment, and a whole-profile approval that writes project rules.
- Third slice (2026-11-03): per-task model budgets in `crates/deck-gateway`
  (`policy::Budget`, `Ledger` admission and token charge, `usage::Meter` for
  JSON and SSE), `DECK_MODEL_REQUEST_BUDGET` / `DECK_MODEL_TOKEN_BUDGET` as
  launch limits, and `tasks/<id>/budget.json` as the live override re-read
  before each admission. The gateway reports totals with `model_usage`; the
  daemon records them as a `usage` event on the task, and a resumed gateway
  starts from those totals. Verified by ledger and meter unit tests, the real
  gateway (request denial before the provider, SSE usage in the final chunk,
  usage recording, a budget file change between two requests), and the real
  daemon (`model_usage` totals, stale reports ignored, `set_budget` file).

- Linux acceptance started 2026-11-05 at `560b9e9`, then reran at merged
  PR #58 (`c820a94`) with libkrun 1.19.7, libkrunfw 5.6.2 and Construct
  v0.4.0. All 56 live probes pass after the fixes for #54–#57, including
  unattended Secret Service rotation/revocation/deletion, public guest TLS,
  approved SSH transport and total CONNECT header deadlines. The initial
  full test run hit an intermittent approval-wait assertion (#61); the
  isolated rerun and subsequent full suite pass. This is not full matrix
  or scoped-reuse amendment acceptance. Evidence and remaining live gaps:
  [Linux acceptance report](../acceptance/2026-11-05-linux-adr-0004.md).

- Broader Linux guest coverage at `c820a94`, 2026-11-05: 59 of 61 new live
  assertions pass, covering concurrent task isolation, upstream model headers,
  live VM credential rotation/revocation, real Construct denial feedback,
  gateway-crash containment, mixed-address resolution and project-profile
  persistence/revocation. Mirror body corruption (#62) and unsupported-scheme
  redirect handling (#63) remain failures. A checked-in harness and exact
  limitations are recorded in the
  [broader Linux report](../acceptance/2026-11-05-broader-linux.md).

- Linux follow-up at `78127b3`, 2026-11-05: merged #62/#63 fixes pass the
  61-check harness. Thirty-three added resolver-cache, daemon-outage expiry
  and malformed GitHub assertions pass. Isolated real Secret Service and
  trickled GitHub-header probes find two nonblocking failures (#66/#67),
  for 104/106 live assertions passing overall. The user's keyring and host
  resolver are untouched. Exact fixture boundaries and remaining limits:
  [Linux follow-up report](../acceptance/2026-11-05-follow-up-linux.md).

- Linux merged-fix rerun at `337db12`, 2026-11-05: all 107 current live
  assertions pass, including real isolated locked-item classification,
  genuine absence, pending-read single-flight/recovery, and a trickled
  GitHub header receiving 408 after ~5.1 s. Workspace checks pass (389 tests).
  No open reproduced failure remains in this suite; fixture boundaries and
  untested limits still apply. Assets now live outside the collected build
  cache. See the [final Linux rerun](../acceptance/2026-11-05-final-linux-rerun.md).
