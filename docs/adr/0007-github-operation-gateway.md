# 0007 — A GitHub operation gateway, not an authenticated relay

Status: accepted, 2026-11-03. Answers #8 and completes ADR 0004's service
gateway for publication.

## Context

ADR 0004 §3 hands publication to #8 and provides only the transport: a
service-specific gateway on the extension port with approvals, grants and host
credentials. It also states the constraint that shapes this ADR: a hostname
allowlist plus a repository-scoped write token does not confine a task to its
own branch, and merge must never be exposed merely because the credential could
do it.

The task's own route to GitHub is already decided. `repo` covers read-only
`github.com:443` and `raw.githubusercontent.com:443` through the web gateway, so
clone and fetch need no new route. The guest has no IP route at all, so nothing
it runs can push, tag or call the API unless a gateway carries it.

Four more facts from the merged slices matter:

- **An opaque CONNECT cannot see operations.** §8 of ADR 0004 is explicit that
  path-level control of `github.com` requires a terminating gateway or a host
  mirror. SSH on 22 would give the guest an authenticated `git push` with
  arbitrary refs, which is the same bypass in a different protocol.
- **Local tasks have no enforced boundary.** A local task shares the user's
  home, keyring and network, so it can push directly and there is nothing for
  this gateway to enforce. It is a VM-kind feature: a local task gets no
  listener and no `DECK_GITHUB_URL`, and the UI must not claim one for it.
- **The guest already has a developer tool.** Construct's `developer`
  operation takes `push` and `pr_create` locally, through `git` and `gh` with
  the host's credentials, behind its own per-operation approval. In a Deck VM
  there is no such credential and no route, so the tool fails rather than
  falling back; the agent-facing path is this gateway, and the contract it
  speaks is what Construct's side implements.
- **Approvals are already the right shape.** The queue, the bounded hold, the
  once/task/project grants, the attachment lease and the boundary status
  endpoint exist. The GitHub gateway is a policy core, a client, and a small
  HTTP surface on top of them.

## Decision

### 1. One bounded operation set, never a relay

The gateway answers five operations on paths it parses. Nothing else is
proxied, and there is no general `/repos/…` passthrough.

| Operation | Path | What the host does |
|---|---|---|
| `publish` | `POST /operations/publish` | Push a local commit to one allowed remote branch. Creates the branch or fast-forwards it. |
| `refs` | `POST /operations/refs` | List branches: name, commit, whether it is a task branch. Read-only. |
| `create_pr` | `POST /operations/pull_request` | Create a pull request from an allowed head to an allowed base. |
| `update_pr` | `POST /operations/pull_request/update` | Retitle, rebody, `draft: false`, or `base` change forward. |
| `review` | `POST /operations/pull_request/review` | Report PR state: number, head commit, draft, mergeable, checks. |

Merge, close, force push, delete, tag creation, release, workflow dispatch and
every repository-setting write are not in the set. `merge` is a `PATCH` on the
same path with a `merge` field, and it is refused by name —
`deck_not_permitted` — so an agent that tries learns why rather than getting a
404 it might retry.

A request body is a bounded JSON object (64 KiB) with exactly the fields the
operation names; an unknown field is refused, because an agent that asks for a
capability it does not have must not be silently given less and believe it
succeeded.

### 2. The repository and the refs are host policy

A task may publish only to a repository the project selected. The identity is
`owner/name`, lowercased, from `DECK_GITHUB_REPOSITORY` in `deckd`'s
environment; a task whose project names no repository has no GitHub gateway at
all rather than an open one.

Ref rules:

- **Remote branches** are `deck/<task>` — the task's own branch, derived from
  the task id, which the guest cannot choose. The only other writable name is
  a branch the repository's policy file lists under `branches`, for a project
  that wants its tasks to share a PR branch.
- **Base branch** is `main` unless `DECK_GITHUB_BASE` names one; a base outside
  the repository's default or that name is refused before any GitHub call.
- **A push is fast-forward only.** The gateway checks the remote branch's
  current commit is an ancestor of the commit being published. A rewritten
  history (force push) is refused, not truncated; recovering from a bad push
  is a host decision, not an agent one.
- **Bundles are bounded.** `POST /operations/publish` takes a binary git
  bundle in the same body as its JSON object: the JSON, a 4-byte big-endian
  length, then the bundle (16 MiB), all covered by one `content-length`. The
  bundle is v2 with the exact ref name `refs/heads/<branch>`, optionally with
  prerequisite lines (an incremental `base..branch` bundle); the gateway
  refuses any other header line, a bundle whose refs are not that one, or one
  that carries more than one ref. The host fetches the remote tip into its
  scratch repository first, so prerequisites and the ancestry check resolve
  however the branch was last pushed, and publishes run one at a time. Work moves between guest and
  host as git objects, so no filesystem is shared and no protocol frame has to
  be invented for one commit.
- **Merge authority is GitHub's.** Nothing here can merge. GitHub's protected
  branches and rulesets remain the enforcement for `main`; the gateway's job
  is that a guest can only ever add to its own branch.

### 3. Approvals name the operation, and a project rule is durable

`publish`, `create_pr` and `update_pr` ask the approval queue with
`gateway: "github"`, `operation` equal to the operation name, and the
destination `owner/name#branch` or `owner/name#pr`. The card therefore says
"publish to strf0x1/deck#deck/t-123" rather than "some network write". Allow
once retries the exact operation; Allow for this task lasts twelve hours like a
web grant; Always allow writes a project rule on `owner/name`, which covers
every `owner/name#…` destination of that operation. `update_pr` is refused
before any card unless the pull request's head is a branch the task may write
in this repository.

`refs` and `review` are reads of the task's own repository and run without an
approval, the same way a profile-covered read does. They still use the host
token, so they are attributed as the user's; the gateway never returns anything
the response did not contain.

A whole GitHub profile is one decision: `github_profile { task, project?,
revoke }` on the control socket writes one durable project rule per operation
for that repository, so a project that trusts its own repository does not
answer a card on every push.

Every denial reaches the boundary status endpoint's `/events` with
`gateway: "github"`, so a failed push in a shell tool carries the note that
says policy rather than outage.

### 4. The token is the daemon's environment until the credential store lands

`DECK_GITHUB_TOKEN` in `deckd`'s environment is the v1 credential. It is the
same shape as the model key: read once at gateway start, held in memory, never
in `vmm.json`, the guest environment, a console line or a log. It reaches `git`
as environment-supplied config (`GIT_CONFIG_COUNT`/`KEY`/`VALUE`, Basic auth as
`x-access-token`), never as an argument a process listing would show. A personal
token (fine-grained, restricted to one repository with `contents: write` and
`pull_requests: write`) is enough for the first prototype.

A GitHub App installation token is the target identity, because it is
repository-scoped and expires hourly; it changes where the value comes from, not
the operation set, and the credential broker ADR 0004 §4 defers is the right
place to put it. A missing token makes the gateway refuse every operation with
`deck_credential_missing` instead of starting without one.

ADR 0004 §4 (amended 2026-11-04) settles the broker, and it has landed.
`DECK_GITHUB_TOKEN` is a stored credential name fetched once per operation
(after the queue admits a write; before the ownership check for `update_pr`,
which already needs it), and the broker's `deck_credential_unavailable`,
`deck_credential_refused` and `deck_credential_revoked` join this ADR's error
table. A revocation kills the operation's `git` process group and discards
its API result. App tokens remain that ADR's §4.6 follow-up.

The guest never receives a token. `git push` inside the task fails, and so does
`gh`; the agent's tools call the gateway, which is the point.

### 5. Transport: a dedicated socket per task, and the extension port

The gateway listens on its own Unix socket, mapped to guest vsock port 1029
(1027 is the extension port, 1028 the web gateway), and answers the `github`
kind on the extension port for a client that speaks the framed kind. A client
that can only open a TCP socket reaches it through a `deck-egress` relay at
`127.0.0.1:8084`, exported as `DECK_GITHUB_URL`; like every relay it is a byte
carrier, so skipping it meets the same gateway.

The gateway exists only when the project has a repository: no repository, no
listener, no relay, and `DECK_GITHUB_URL` unset. A task that trusts a project
by mistake cannot get a route that a task without one also gets.

### 6. The agent reaches it through Construct's `developer` tool

The operations are HTTP, so any tool or ACP agent can call them; Construct is
the first client. In a task whose environment carries `DECK_GITHUB_URL`, the
`developer` tool's GitHub operations (`push`, `pr_create`, `pr_review`) call the
gateway instead of `git` and `gh`, with the same per-operation approval
Construct already asks for in front of it. The two approvals answer different
questions — Construct asks whether the agent may run the operation; Deck asks
whether this task may cross into GitHub — and the agent sees the gateway's
`deck_boundary_denied` body with its approval id exactly as the boundary tools
do (§4 of ADR 0004).

`git push` from a shell stays impossible: there is no credential and no route.
The tool is the path, and its description says so, because a model that reads a
failed `git push` and then looks for another remote is the failure mode this
whole design exists to prevent.

### 7. Every refusal says what the agent should do next

A failure is sorted by what the agent can do about it, and the kind alone sets
the status, the code and `retryable`. Every body has the boundary denials'
shape — `error.{message,type,code}` and `retryable` — plus `current` when the
branch's commit is the reason.

| Kind | Status | Code | Retryable | Meaning |
|---|---|---|---|---|
| Refused | 403 | `deck_github_refused` | no | Policy: a branch, base or pull request the task may not touch, or a publish that would rewrite the branch (`current` names the commit to build on). |
| Conflict | 409 | `deck_github_conflict` | yes | The branch moved during the publish; `current` is where it is now. Rebuild, then retry. |
| Invalid | 400 | `deck_bad_request` | no | The request or bundle is malformed, or its prerequisites are not on the branch. |
| Not found | 404 | `deck_github_not_found` | no | GitHub has no such pull request. |
| Rejected | GitHub's (409, 422) | `deck_github_rejected` | no | GitHub refused it, e.g. a pull request already exists or a protected-branch hook declined the push; GitHub's reason is the message. |
| Credential | 502 | `deck_github_credential` | no | GitHub refused the host's token. The user's to fix, not the agent's. |
| Unavailable | 502 | `deck_github_failed` | yes | GitHub or git could not be reached or answer, or a rate limit. |

The approval denials (`deck_boundary_denied`) keep their own shape, and so
do the broker's refusals (ADR 0004 §4.4), each with `retryable`:
`deck_credential_missing` (503, no), `deck_credential_unavailable` (503,
yes), `deck_credential_refused` (403, no) and `deck_credential_revoked`
(403, no). GitHub's and git's text
is passed through for its reason, bounded and with the token redacted.

## Consequences

- Publication is one branch per task, reviewed and merged on GitHub. Deck never
  shows a merge button and the token never needs merge scope.
- An agent that runs `git push` fails the same way it would in any sandboxed
  environment; the operation set is reachable through the documented tools, and
  Construct is the first client of it.
- Force push is impossible from a task. A task that rewrote its history must be
  recreated or fixed by a person with the host credential.
- A bundle per push costs an extra copy of the objects. It is bounded and only
  on publish; incremental bundles would be a later optimization, not a
  behavior change.
- Construct needs the `github` module set and the tools; `deck-fake-agent`
  must advertise the same capability so Deck and CI can exercise the path
  without a model (ADR 0002).
- Revisit if: a task legitimately needs several repository branches (widen the
  policy file, not the operation set), or a GitHub App replaces the token
  (same operations, different credential source).

## Evidence

- ADR 0004 §3 (service gateway), §5 (approvals), §7 (revocation),
  §9 (acceptance cases); `SANDBOX-DESIGN.md` "GitHub should own the merge
  decision"; deck#8.
- `crates/deck-gateway/src/web/` (address policy, grants re-read, approvals,
  boundary events) and `crates/deck-runtime/src/approvals.rs` (queue, scopes,
  leases) as the machinery this reuses.
- GitHub App installation authentication and protected branches, cited by
  deck#8.
