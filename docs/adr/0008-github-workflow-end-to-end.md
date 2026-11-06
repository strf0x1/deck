# 0008 — The GitHub workflow end to end: the agent's client and Deck's view

Status: proposed, 2026-11-06. Builds on ADR 0007; the remaining half of #6's
"draft PR" step. Amends ADR 0007 §3 (writes to the task's own branch no
longer ask by default, §6 here) and §6 (one approval, not two, §5 here).

## Context

ADR 0007 settled what a VM task may do on GitHub and built the host side: a
closed operation set (`publish`, `refs`, `create_pr`, `update_pr`, `review`)
on vsock port 1029, reachable in the guest as `DECK_GITHUB_URL`, with
bundle-based fast-forward publication to `deck/<task>`, approvals named by
operation and destination, and host-held credentials. The gateway works and
is tested against real `git` and a stub GitHub.

Nothing uses it yet, so #6's completion flow ("… → draft PR → stop/resume")
cannot be run with a real agent. Reading the code at `b8b7c9e` and Construct
at `28e1b8f` shows the gaps:

- **Construct has no gateway client.** Its `developer` tool offers
  `issue_create`, `commit`, `push` and `pr_create` through host `git` and `gh`
  (`construct/src/tools/developer/`). Nothing reads `DECK_GITHUB_URL`, there
  is no `pr_review` or `pr_update`, and in a VM `push` fails on the missing
  route. ADR 0007 §6 says what should happen, not how.
- **The guest cannot learn where it may publish.** The task branch is derived
  from a task id the guest never sees. `refs` marks `task_branch: true` only
  on a branch that already exists, so before the first publish the agent
  cannot name the ref its bundle must carry, and it never learns the base.
- **The repository is daemon-global.** `DECK_GITHUB_REPOSITORY`,
  `DECK_GITHUB_BASE` and `DECK_GITHUB_BRANCHES` are read from `deckd`'s
  environment (`vm.rs`), so every VM task of every project gets the same
  repository. One `deckd` serves every window, so a second project would be
  given the first project's repository.
- **The guest's checkout is not ready to publish.** The project is copied to
  `/workspace`. Whether it has `.git`, an `origin` the guest can fetch
  (`git@github.com:` cannot work: there is no port 22), a commit identity, or
  a branch named for the task is undefined.
- **Deck cannot see the result.** Approvals say "publish to
  owner/name#deck/t-123", and then nothing: no branch, no commit, no pull
  request, no checks. The user goes to GitHub and works out which branch was
  which task.
- **The wire is not in the shared contract.** The operation bodies, the
  responses and the refusal table (ADR 0007 §7) are a Deck–Construct wire as
  much as the ACP extensions are, and ADR 0006 says such a wire lives in
  `strf0x1/sprawlcommons-acp`, not in two repositories' prose.
- **The fake agent cannot publish.** ADR 0007 requires `deck-fake-agent` to
  exercise the path so the UI and CI do not need a model; it does not yet.

## Decision

### 1. The operation wire joins the shared contract

The GitHub operation API becomes part of `strf0x1/sprawlcommons-acp` as an
additive minor release: a schema per request and response, the publish
framing (JSON, 4-byte big-endian length, bundle), the refusal table with
codes and `retryable`, and conformance fixtures for each operation's success
and refusal. Deck's gateway tests and Construct's client tests both run
against the vendored fixtures. The change protocol is ADR 0006's: contract
first, both implementations pinned in one wave.

The agent says it is a client with an `initialize` capability under the
reverse-DNS namespace (ADR 0005), `github_operations: 1`. Deck uses it for one
thing: a VM task with a repository whose agent does not advertise it shows
"this agent cannot publish from a sandbox" rather than letting the user find
out from a failed push.

### 2. The guest discovers its destination from `refs`, not the environment

`refs` answers, alongside the branch list, the host policy the guest needs:

```json
{ "repository": "owner/name", "base": "main",
  "task_branch": "deck/t-123", "writable": ["deck/t-123", "deck/shared"],
  "branches": [ … ] }
```

The gateway is the authority, so the guest asks it rather than reading a
second copy of policy from its boot environment that could disagree with it.
No task id, repository or base is added to the guest environment.

### 3. The repository is part of the task, not the daemon

A VM task's GitHub policy (repository, base, extra writable branches,
credential name) is recorded in its task spec at create, so it survives
restarts, appears in `task.json`, and differs per project.

- The repository is the project's `origin` remote when it points at
  `github.com` (HTTPS or SSH form, normalized to `owner/name`). The base is
  the remote's default branch as the host's checkout records it
  (`refs/remotes/origin/HEAD`), else `main`.
- A project file (`.deck/project.toml`, `[github] repository, base,
  branches, approvals`) overrides inference, for a checkout whose GitHub
  remote is not `origin` or a project that wants another base, and sets the
  approval mode (§6).
- One repository receives both the push and the pull request, as in ADR
  0007. A fork therefore publishes to and opens its pull requests on the
  fork; a pull request into the upstream repository is opened by the user on
  GitHub. Pushing to the fork and opening the pull request upstream is
  deferred (see Consequences): it needs a second repository in the policy and
  a token that can write pull requests on a repository the user does not own.
- `DECK_GITHUB_*` in `deckd`'s environment stays as a development override
  that applies to every task and says so in its log line, not as the way a
  user configures a project.
- A project with no GitHub origin and no file has no gateway, as ADR 0007 §5
  requires.

Inference reads the user's own checkout on the host, which is already trusted
input to task creation; nothing from the guest selects a repository.

### 4. The guest checkout is prepared for publication at first start

When the task has a repository, the guest entry, after import:

- keeps `.git` from the imported project;
- rewrites `origin` to `https://github.com/<owner>/<name>.git`, so `fetch`
  goes through the web gateway's read-only `repo` profile;
- creates and checks out `deck/<task>` at the imported `HEAD` if it does not
  exist, so the local branch and the remote branch have the same name and a
  bundle of it carries exactly the ref the gateway accepts;
- sets `user.name`/`user.email` from the host's git identity recorded in the
  spec, so the commits on the PR are the user's, as they would be locally.

The import into the guest leaves out files the project's git ignores
(`git ls-files --others --ignored --exclude-standard`) when the project is a
git checkout. Ignored files are where secrets (`.env`, local credentials) and
build output live; once a task can publish without a card (§6), a file that
reaches `/workspace` is one `git add -A` away from a remote branch. Untracked
files that are not ignored are still imported: they are the user's work in
progress and the agent should see them. This applies to every VM task of a
git checkout, with or without a repository, because it is what a guest can
leak through any route, and it makes the import smaller.

The task branch name reaches the guest entry through the boot configuration
the VMM already writes. It is a label for the checkout; the gateway still
refuses any other ref, so getting it wrong makes publication fail, not widen.

### 5. Construct's `developer` tool speaks the gateway when it is there

With `DECK_GITHUB_URL` set, Construct's `developer` operations map onto the
gateway and never fall back to `git push` or `gh`:

| Tool operation | Gateway | Notes |
|---|---|---|
| `commit` | none | Local, as today. |
| `push` | `publish` | Bundles `refs/heads/<task_branch>`, incremental from the last known remote tip when `refs` reports one. `remote` is ignored; a `branch` other than a writable one is refused before the call. A 409 `current` is reported as "rebase onto `<sha>` and push again", not retried. |
| `pr_create` | `create_pr` | `head` defaults to the task branch, `base` to `refs`' base, `repo` must be absent or equal the task's repository. Draft by default. |
| `pr_update` | `update_pr` | New. Title, body, `draft: false`, base. |
| `pr_review` | `review` | New. Read-only; returns state, checks and mergeability so the agent can react to CI. |
| `issue_create` | none | Refused in Deck mode with the reason; it is not in the operation set. |

The tool description in Deck mode says publication goes through this tool
and that `git push` will fail by design (ADR 0007 §6).

**One decision per write, at the gateway.** In Deck mode (`DECK_MANAGED=1`
and `DECK_GITHUB_URL` set) Construct does not ask its own approval for the
gateway-backed operations (`push`, `pr_create`, `pr_update`); Deck's
gateway is the only decision point (§6), and `pr_review` is a read that
nobody asks about. This
amends ADR 0007 §6, which kept both. Construct's approval is cooperative: an
agent that skips it skips it, so it cannot be the control, while the
gateway's cannot be bypassed and already names the operation, the
destination and (§7) what is being published. Two cards back to back in one
window for the same push teach the user to click through both. `commit`
stays local to the guest and is the sandbox's business, not the boundary's;
whether Construct still asks for it in Deck mode is Construct's call. Outside
Deck mode nothing changes: the host-credential path keeps Construct's
approval, because there it is the only one.

### 6. Writes to the task's own branch are allowed by default

The human decision in this workflow is the pull request, on GitHub, where the
change can be read. A card at publish time asks for that decision before
there is anything to read it in: a careful user cannot judge "allow
`create_pr`?" without the diff, and a hurried one learns to click Allow,
which protects nothing. What makes a publish safe is §2 of ADR 0007, not the
card: one repository, one branch, fast-forward only, no merge, nothing on
the base. Everything a task can do within that is visible, reversible and
waits for review.

So the default approval mode, `own-branch`, admits without a card:

- `publish` to the task's own branch;
- `create_pr` from the task's own branch as a draft;
- `update_pr` of a pull request whose head is the task's own branch, except
  `draft: false`.

and still asks for:

- any write to a shared branch listed under `branches`, because it changes
  work other tasks or people build on;
- making a pull request ready for review (`update_pr` with `draft: false`,
  or `create_pr` with `draft: false`): it is once per pull request, it is the
  moment other people are notified, and "the agent says this is ready" is a
  question worth one click;
- every write while the repository is public (below).

Everything outside the operation set is refused as before. `approvals =
"ask"` in the project file restores a card for every write, which is ADR
0007 §3's behaviour; this section amends that default.

**The queue still decides.** The gateway asks `deckd`'s approval queue for
every write exactly as now; in `own-branch` mode the queue admits a covered
operation at once and records it with scope `default`. Writes therefore
still reach the boundary `/events`, the task record (§8) and the access
panel, a project or task can be switched to `ask` and the next operation
sees it, and revoking the GitHub credential still stops everything,
including a publish in flight (ADR 0004 §7).

**Public repositories ask.** Pushing a branch to a public repository
publishes it to everyone, so a mistake or an injected instruction becomes a
disclosure rather than a draft. Before a write it would admit by default,
the gateway reads the repository's visibility (`GET /repos/{owner}/{name}`,
a read with the same token, cached for the operation); a public repository,
or one whose visibility it cannot read, turns that write into a card. A
project that publishes in the open and accepts this sets `approvals =
"own-branch-public"` in its project file, explicitly.

### 7. Cards say what is being published

A GitHub ask carries a bounded `summary` in its details: for `publish`, the
commit being published, its subject and the count of new commits; for
`create_pr`/`update_pr`, the title and whether it is a draft. The card shows
it under the destination. With §6 a card is the exception — a shared
branch, ready for review, a public repository, or `ask` mode — so the
summary matters most exactly where a card appears. The summary is guest-supplied description, shown
as such; the decision is still about the destination and operation, and the
gateway, not the summary, decides which commit lands.

### 8. Deck shows each task's publication, read-only

Publication is a property of the task, so it is stored with the task and
shown with it.

- **Record.** After a successful `publish`, `create_pr` or `update_pr` the
  gateway reports the outcome to `deckd` on its control socket (branch and
  commit, or pull request number, URL, title and draft), and `deckd` appends
  it to the task's `events.jsonl`. What a stopped task published is known
  without GitHub or the VM.
- **Live state.** While the task runs, `deckd` reads pull request state
  (open/draft/merged/closed, checks summary, mergeable, head commit) through
  the task's own gateway socket with `review` and `refs`, the same reads the
  guest may make, on a bounded interval while a window shows the task and on
  demand. No new GitHub client, credential path or listener; `deckd` never
  sends a write operation there. A stopped task shows the last recorded and
  last read state, marked as such.
- **View.** The task's card and header show the branch and, once there is
  one, a pull request chip with one state dot (draft, checks running,
  checks failing, ready, merged, closed). Opening it shows a publication
  panel: the branch and its last published commit, the pull request title and
  number, the checks list, and "Open on GitHub". It has no merge, close,
  ready or push control: GitHub owns the merge decision (ADR 0007), and
  marking ready is the agent's `update_pr` or the user's click on GitHub.
- A merged pull request marks the task's card as done and offers destroy;
  it does not destroy anything itself.
- A local task shows none of this (ADR 0007: no enforced boundary, no
  gateway).

### 9. The fake agent and an acceptance run prove the loop

`deck-fake-agent` gains a scripted publication turn (commit, `push`,
`create_pr`, `review`) against `DECK_GITHUB_URL`, advertising
`github_operations`, so `crates/deck/tests/` and CI exercise the default
admission, the card for ready-for-review, the recorded outcome and the panel against a stub GitHub without a model.

The completion evidence is an acceptance run in `docs/acceptance/` with real
Construct in a VM, its scripted model and a scratch GitHub repository: a turn
edits, commits, pushes and opens a draft PR with no card; the PR chip
appears; checks are read; a second push fast-forwards; a forced history is
refused with `current`; marking the PR ready shows a card with the summary
and the user allows; with `approvals = "ask"` the first push shows a card; a
git-ignored `.env` in the host project is absent from the guest; the task is
stopped and resumed and the chip is still there.

## Consequences

- #6's completion flow can be run end to end with a real agent, and every
  piece of it has a test that does not need one.
- The operation API changes only through the contract repository, so Deck's
  gateway and Construct's client cannot drift in one place.
- Projects configure GitHub by having a GitHub origin. Repositories the user
  can write to work without configuration; a fork's tasks open pull requests
  on the fork, and the user carries them upstream.
- Fork support (push to `origin`, pull request on its parent, head
  `owner:deck/<task>`) is a later decision. It widens ADR 0007's policy to a
  push repository and a pull-request repository (the operation set is
  unchanged), should detect the parent from GitHub's fork metadata rather
  than a remote named `upstream`, and first needs evidence of which token
  type can create a pull request on another owner's repository.
- The task's checkout in the guest has a different branch from the host's,
  by name. That is intended: the user's working tree is not the task's.
- Deck gains a read path to GitHub (through each task's gateway) but no write
  path; the token still never leaves `deckd` and the gateways.
- By default a task goes from edit to draft pull request with no prompt at
  all; the human decision is reading the pull request and, once per pull
  request, allowing it to be marked ready. Every write is still admitted by
  the queue, recorded and revocable.
- Where a card does appear it is the only one (Construct no longer asks in
  Deck mode), so it is one decision, not two.
- A public repository asks on every write unless its project opts out by
  name; that is friction on open-source work, accepted because the failure
  is irreversible disclosure.
- A project that needs a file the import now leaves out (an ignored local
  config the build reads) has to un-ignore it or provide it another way;
  Deck does not copy ignored files into a guest that can publish.
- Revisit if: tasks need several repositories (widen the spec, not the
  operation set), a GitHub App replaces the token (ADR 0004 §4.6), or polling
  `review` proves too costly (GitHub webhooks would need a public endpoint
  Deck does not have).

## Evidence

- ADR 0007; `crates/deck-gateway/src/github/` (`policy.rs` `Ask` and the
  operation table; `client.rs` `refs` marks `task_branch` only on existing
  branches); `crates/deck-runtime/src/vm.rs` (`DECK_GITHUB_*` read from the
  daemon environment, `guest_environment` exports no task, repository or
  base); `scripts/vm-image.sh` (exports `DECK_GITHUB_URL`), at `b8b7c9e`.
- Construct at `28e1b8f`: `crates/construct/src/tools/developer/mod.rs`
  (operations `issue_create`, `commit`, `push`, `pr_create`; no reference to
  `DECK_GITHUB_URL`).
- `CONTRACT.lock` (contract 1.1.0) has no GitHub operation schema.
- #6 completion criteria; ADR 0006 change protocol.
