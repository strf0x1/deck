# 0004 — Host gateways enforce guest egress, credentials, and approvals

Status: proposed, 2026-11-03. Answers #9; the input is
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
request. A new destination or rule needs no new port; a new *gateway kind*
needs a port reserved at boot (or a VM restart). If that becomes common, one
reserved control port can carry framed gateway requests instead of adding a
port per service.

Guest relays (`deck-egress`, part of the curated image) expose TCP endpoints
on `127.0.0.1` for clients that cannot speak AF_VSOCK — Construct's HTTP
client, package managers, git. A relay is a byte carrier, not a policy
point: an agent can replace it, ignore it, or open the vsock port directly,
and the host gateway applies the same policy. Replacing the relay therefore
does not widen the boundary.

Consequences:

- No DNS, raw IP, UDP, QUIC, IPv6, alternate ports, redirects to a new host,
  or host/private-network destination has a route. Those attempts fail
  closed rather than reaching a host proxy to be logged and approved.
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

- **CONNECT**, only to port 443. The host resolves the name, rejects
  loopback, private, link-local, multicast and unspecified addresses, and
  dials the validated address. The TLS stream is opaque: this is
  destination-level control, not URL-level.
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

- **Allow once** — one retry of the exact request fingerprint within a short
  window (for example 60 s).
- **Allow for this task** — matching operations until the task stops or the
  grant expires.
- **Always allow for this project** — a persisted project rule. Rules live
  in Deck's data root keyed by project identity, not in the repository, so
  the guest cannot edit its own policy.
- **Deny** — the operation fails; the agent sees a structured error.

The gateway **fails fast** rather than holding an arbitrary socket. It
returns `403` with a JSON body such as
`{"error":"deck_boundary_denied","approval_id":"…","retryable":true}` and an
`X-Deck-Approval` header. The agent reports the denial; after a decision the
next attempt is a new request. Model access for the task's selected provider
and model is authorized by the task's own model choice, so a first turn does
not fail merely because a model gateway exists; switching model or provider
asks.

Pending items are deduplicated within a short window and expire (five minutes
by default). A late decision reports `expired`; the guest may retry. Pending
items are ephemeral across a `deckd` restart: the in-flight request already
failed, and the guest's retry creates a fresh item. Saved rules and task
grants are persisted.

Control surface: the daemon control protocol is additive. Approvals appear in
the existing notice subscription; Deck gets `approvals` (pending items for a
task), `decide_approval`, `grants` and `revoke_grant`. The gateway is a
long-lived control client and uses `decide_approval` for the queue and a
`credential` request that returns one value; neither request nor reply is
logged. The task record keeps grant events and audit entries, not pending
items.

The UI adds a boundary card and a per-task access panel:

- the card shows task, gateway, destination/model, credential name, the
  reason, and the options above — never a secret;
- the panel lists active grants with their scope and expiry, pending items,
  and a revoke control;
- a denial appears in the transcript with the approval id and a retry hint,
  not as an opaque tool failure.

### 6. Grants and revocation

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

### 7. Threat model

**Trusted:** the host runtime (`deckd`, gateway, VMM) as software, the OS
credential store, and the human decision. **Untrusted:** everything in the
guest, including Construct, delegated agents, MCP servers, shell commands,
files, environment, and any bytes on a gateway socket. The VMM is trusted but
constrained: it maps ports and relays bytes; it does not parse HTTP or make
policy decisions. The gateway is trusted but constrained: no shell, no host
filesystem beyond its config, log, resolver and system trust store, a small
environment, resource limits, and a parser that treats every request as
hostile.

In scope: bypass through TSI, host loopback, private networks, DNS, raw IP,
UDP/QUIC, IPv6, alternate ports, redirects, proxy environment changes,
`/etc/hosts`, direct vsock-port access, cross-task addressing, credential
theft from the guest, parser crashes, grant abuse, and audit tampering.

Out of scope: hypervisor or kernel escapes, a compromised guest image or
supply chain, host compromise, a human approving a harmful destination,
side channels, and data-loss prevention. **Approved egress can exfiltrate
project content** — a model provider or an allowed repository receives what
the agent sends. An allowlist is not DLP, and the UI must not imply it is.

### 8. Acceptance cases

The implementation is not done until these have tests where practical.

**Bypass**

1. From the guest, `curl http://127.0.0.1:<host-port>/` (the old TSI model
   port) fails; no host loopback is reachable.
2. `curl http://169.254.169.254/`, `10.0.0.1`, and a private IPv6 literal
   fail; if attempted through the proxy they are denied by address class.
3. Direct DNS and a direct connection to an allowed host fail without the
   relay/proxy; with the proxy, the same host prompts or follows its profile.
4. UDP/QUIC and IPv6 have no route.
5. CONNECT to port 8443 is denied; only 443 is accepted.
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

1. A denied model request reports the approval id; Allow once lets exactly
   one matching retry through; a second retry prompts again.
2. Allow for task lets matching requests through until stop or expiry.
3. An expired approval is a denial; a later retry creates a new item.
4. A retried shell script is not replayed by Deck; side effects may repeat
   and the transcript shows both attempts.

**Credentials**

1. The real key is absent from the guest environment, Construct config, ACP
   frames, `vmm.json`, console, journal and gateway log.
2. Rotating a key in the credential store changes the next request; no guest
   restart is needed.
3. A denied request injects nothing.
4. A guest-supplied credential header is stripped, not merged.

### 9. Construct and ACP follow-ups

- **Construct:** surface a gateway denial as a distinct, retryable error that
  includes the approval id and the requested scope; bound model retries so a
  denied call does not spin. The model client already has `Endpoint`
  metadata for a readable prefix; extend it rather than adding a second error
  path.
- **Construct:** keep secretspec environment-first; Deck supplies placeholders.
  `construct secrets set` inside the guest is not a supported path. A guest
  keyring warning is noise, not a working store.
- **Construct:** audit the optional `web_search` and router providers before
  enabling them in a guest. A key in the guest environment is not acceptable,
  even an optional one.
- **ACP:** no new method for boundary decisions. ACP permissions remain
  cooperative; the runtime approval is a Deck/runtime event. #13's forwarding
  is the separate cooperative fix and does not replace this boundary.
- **#8** owns the GitHub operation schema, repository/ref rules, PR state and
  the GitHub App credential; **#10** owns private registries and package
  mirrors; **#12** evaluates an IP-networking backend if the vsock-only
  compatibility cost is too high.

### 10. First implementation slice

1. Add `crates/deck-gateway/` with a pure policy core and a model gateway;
   add `deck-egress` to the guest image (loopback relay, model listener).
2. Point the guest's Construct at the relay with a placeholder key; the host
   gateway injects the scripted provider's key.
3. Add the `krun_disable_implicit_vsock` and `krun_add_vsock(ctx, 0)` wrappers
   in `krun.rs`, call both in `vmm.rs`, and map the model gateway's vsock
   port; verify the VM prompt test still passes and the guest cannot reach
   the host's model port directly.
4. Add the approval queue, model-gateway budget/model checks and the boundary
   card. Then add the web/package profiles; then hand the transport to #8.

## Deferred

- A virtio-net compatibility backend and any UDP/QUIC support. The guest has
  no IP route in v1; adding one means another enforced backend, not a policy
  exception.
- TLS interception for path-level control of arbitrary HTTPS.
- Private package registries and their credentials (needs a terminating
  gateway or host mirror; #10).
- Persisting pending approvals across a daemon restart (the request already
  failed; the guest retries).
- An OS service for gateways when no Deck process is ever opened (ADR 0001's
  deferred item).
- Per-destination data-loss controls; approved channels can carry data.

## Consequences

- The guest loses general networking. Package managers and HTTPS git work
  only through the proxy/relay; tools with their own resolver, raw sockets,
  or Git-over-SSH fail. This is the price of a boundary that does not depend
  on the agent.
- A new host process per task and a guest relay to build and keep updated.
- Construct's guest configuration must use placeholders and the relay; the
  existing TSI shortcut and its VM tests must change in the same slice.
- A gateway crash is a task failure, not a silent loss of egress; the runtime
  must report it as such.
- The first prompt after a new destination is a denial plus a human decision,
  not a hanging request. The agent must cope with a retry; Construct's error
  path is the follow-up that makes that pleasant.
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
- No implementation or security validation has been run for this design yet;
  the acceptance cases in §8 are the first tests.
