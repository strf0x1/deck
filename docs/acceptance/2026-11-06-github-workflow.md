# GitHub workflow end to end: real Construct publishes a draft PR from a VM

Deck **`bc1254b`** plus the gateway fix in this PR, Construct **`255de54`**,
shared contract **1.2.0** at `4728285`. A **private** scratch repository
(`strf0x1/deck-acceptance-scratch`), real Construct 0.4.0 in a libkrun guest
with its scripted model, and the real Deck window driven through
`DECK_CONTROL`. **52/52 assertions pass**, including the optional merge and
two-click Destroy.

Three product bugs were found: **#106** (marking a pull request ready through
REST did nothing) is fixed in this PR; **#105** (guest provision markers are
untracked in `/workspace`) and **#107** (a merged pull request's own card does
not say done) are open. One fixture limit is material: GitHub-hosted Actions
jobs do not start in the private scratch repository for this account (this
repository's public CI does run), so the scratch check is a queued job the
driver cancels; the deck → gateway → GitHub read path is real (see
[Limits](#limits)).

| Case | Result | Evidence |
|---|---|---|
| Edit, commit, push, draft PR with no card; admitted with scope `default` on the boundary `/events` feed | PASS | [Results](evidence/2026-11-06-github-workflow.json), `/events` in the first turn, GitHub branch and PR |
| PR chip on the card and in the header; the panel opens | PASS | [Panel](evidence/2026-11-06-github-panel.png) |
| Checks are read; the chip goes from running to failing; the panel lists the runs | PASS (check fixture) | [Running](evidence/2026-11-06-github-running.png), [Checks](evidence/2026-11-06-github-checks.png) |
| A second push fast-forwards: incremental bundle, previous set | PASS | "fast-forwarded from `dc1bdfe`", GitHub ref `f6254cf` |
| A forced history is refused with `deck_github_refused`, `current` set, and the agent reports "rebase onto `<sha>`" without retrying | PASS | `06-refused` tool result; one publish call |
| Marking ready shows a card with the summary; allow; the PR is ready on GitHub | PASS after #106 | [Ready card](evidence/2026-11-06-github-ready-card.png), GitHub `draft: false` |
| With `[github] approvals = "ask"`, the first push shows a card | PASS | [Ask card](evidence/2026-11-06-github-ask-card.png) |
| The ignored `.env` is absent from the guest's `/workspace` | PASS | First-turn shell result: `env-absent` |
| Stop and resume keep the chip and panel (`pull_request.json`) | PASS | [Stopped](evidence/2026-11-06-github-stopped.png), same chip after resume |
| Merge on GitHub; card shows done; Destroy asks for a second click | PASS, with #107 | [Destroy confirmation](evidence/2026-11-06-github-destroy-armed.png) |

The [manifest](evidence/2026-11-06-github-workflow-manifest.json) records
revisions, binaries, guest/image hashes, credential names and fingerprints,
and the harness hashes. No credential value was read or written.

## Findings

- **[#106](https://github.com/strf0x1/deck/issues/106), fixed in this PR.**
  `update_pr` with `draft: false` was expected to mark the task's pull request
  ready. The gateway sent `PATCH /repos/{owner}/{repo}/pulls/{number}` with a
  `draft` field; GitHub's REST update has no such field and ignored it, so the
  reply still said draft and the pull request stayed a draft. The fix uses
  GitHub's GraphQL `markPullRequestReadyForReview` / `convertPullRequestToDraft`
  when the requested state differs from GitHub's, then re-reads the pull request
  for its reply. A regression test fails on the old PATCH-only path. The
  acceptance run below used the fixed gateway (`a291610…`).
- **[#105](https://github.com/strf0x1/deck/issues/105), open.** The guest entry
  writes `/workspace/.deck-provisioned` and `/workspace/.deck-checkout` as
  provision markers; both are untracked and not ignored, so the first
  `git status` shows them beside the agent's staged file. A natural
  `git add -A` would publish them. The turn steps here staged named files, so
  the task branch carried only the intended change.
- **[#107](https://github.com/strf0x1/deck/issues/107), open.** After the
  merge, the chip dot turns merged (`#8 · merged`) and the panel offers
  Destroy, but the window's own card still reads `Idle · ready`. The live card
  takes its state from the session, so `View::done`'s "done · merged" branch
  never applies to it. ADR 0008 §8 expects the card to be done.

## What the run showed

The task ran as a VM from the host checkout of the scratch repository. At
create it recorded the inferred policy (repository, `main`, credential
`DECK_GITHUB_TOKEN`, `own-branch`) and the host git identity. The window's
agent pill said `construct acp` and the publish notice was absent, so the
agent advertised the publication capability.

The first turn staged `change.txt`, committed, published and opened the pull
request. The publish result read

> Published `dc1bdfe842763685c5042a0221671f25da39ad1f` to
> `strf0x1/deck-acceptance-scratch#deck/1a1b4b9aaf7-70c0b-0` (created)

and no approval card appeared at any point in the turn. The boundary
`/events` feed the agent read had two `allowed` events with `scope: "default"`,
one for the publish and one for the pull request, and Construct appended the
same admission as a boundary note to each tool result. GitHub's ref was at the
reported commit, and pull request #8 was a draft from the task branch to
`main`. The card and the header both showed `#8 · draft`; opening the chip
showed the panel with the branch, the last commit and the live runs.

A second change fast-forwarded the branch: `(fast-forwarded from dc1bdfe…)`,
and GitHub advanced to `f6254cf…`. `pr_review` read `Head f6254cf…;
mergeable.` and the two queued runs back to the agent.

Marking ready raised exactly one card: `github ·
strf0x1/deck-acceptance-scratch#pr8`, the agent's description "Add the second
scratch change · 1 commit", operation `update pull request 8`. Allowing it once
made GitHub show `draft: false` (after #106). The chip then went
`checks running` → `checks failing` (the queued check the harness completed by
cancelling it), and the panel listed both completed runs.

The rewrite turn expired the reflog and pruned the old tip, then pushed a
divergent commit. The bundle reached the gateway's fast-forward check:

> Deck's GitHub gateway refused: deck_github_refused (403): … is at
> `f6254cf…`, which the bundle does not build on … The branch is at
> `f6254cf…`: rebase onto `f6254cf…` and push again. Do not resend the same
> commits.

That turn made one publish call; GitHub stayed at `f6254cf…`.

Stopping the task kept the same chip, and the panel still opened from
`tasks/<id>/pull_request.json`. `ctrl-r` resumed the task to a ready session
with the chip unchanged. Merging on GitHub turned the chip to `merged`; the
first Destroy click asked for confirmation ("Destroy: confirm", with "Deleting
deletes the task's VM and disk. The pull request stays on GitHub.") and the
second destroyed the task.

For the ask-mode case a fresh daemon and window were started with
`.deck/project.toml` containing `[github] approvals = "ask"`. The task recorded
that mode, and its first push asked with a card (`publish` to the task
branch); allowing it once published.

The ignored `.env` written on the host was absent from `/workspace`, while the
tracked project files (`README.md`, `.gitignore`, `.github/workflows/ci.yml`)
were imported; the guest reported `/workspace`, the task branch and the
rewritten HTTPS origin.

## Limits

- **Checks are a GitHub-side fixture, not a CI result.** GitHub-hosted Actions
  jobs do not start in the private scratch repository for this account: a
  minimal workflow there with a single `echo` also completes as a failure in
  about three seconds with no steps; this repository's public CI runs normally.
  The scratch `ci.yml` therefore targets `self-hosted` (no runner is
  registered), so its check stays `queued`. The driver cancels the queued runs
  to give deckd a completed conclusion; the chip transition and the panel rows
  are read from real GitHub check runs through the task's gateway.
- **The model is scripted.** Each turn's tool steps (and its text reply) are
  written by the driver, so it cannot reason about a result. The refusal case
  is proven by Construct's own tool result and by the single publish call in
  that turn, not by model judgement.
- **The stored GitHub credential is the local `gh` user token** (scope
  `repo`), not a fine-grained token restricted to the scratch repository:
  fine-grained tokens can only be minted in GitHub's web UI. It stays in the
  host file store and is injected only by the gateway; the harness uses the
  same account's `gh` auth to verify GitHub independently. The report holds
  names and fingerprints only.
- **Linux/x86_64 only.** The issue asks for Linux and macOS; macOS was not
  run.
- The scratch repository is reused across reruns, so pull-request numbers and
  branches accumulate; the run's own numbers are in the manifest.
- The publication panel has no controls by design: merge, close and ready are
  GitHub's or the agent's. The harness set nothing ready (the #106 fix made
  the operation work); it only cancelled the queued checks.