# 0015 — Give each local Git worker its own worktree

Status: proposed, 2026-11-18.

## Context

Local coordinator workers start in the project's first registered checkout.
An earlier run ([#200]) let seven workers edit that directory at once. The
guard from [#208] serializes worker creation in a shared checkout. [#207]
proposes a linked Git worktree per worker so independent work can run in
parallel. VM publication stays under [ADR 0008].

## Decision

### Starting work

Deck shows the source branch, short commit, and checkout before delegation.
For this first version, the source is the project's first registered
checkout. Deck uses its committed HEAD and pins that commit at each
dispatch. If the checkout moved or disappeared, Deck shows its path
and asks the user to select a replacement source. If the source has
local changes, Deck says, "Workers start from committed files. Local
edits and untracked files stay here." The user can commit anything
workers need before dispatching. Ignored files and local setup such as
dependencies also stay in the source checkout.

Deck creates `deck/<task-id>` in `<data>/tasks/<id>/worktree` and starts the
worker in that directory. The user sees one project with several worker
cards. The worktree is a task workspace. Opening it routes to that task,
and project registration and index rebuilding exclude it. Project
identity alone is insufficient to find the source repository. Separate
clones and forks can have the same root commit.

Deck keeps coordinator limits and the one-worker guard for in-place
workers, including legacy tasks and resumes. Git workers with managed
worktrees can run up to the normal coordinator limit. If Git setup fails,
Deck shows the failure and keeps the brief out of the shared checkout. A
Git repository without a first commit asks the user to make one. The
coordinator and ordinary local sessions keep their existing checkout
behavior.

### Reviewing and integrating work

The worker card shows the branch, starting commit, and integration target.
It offers **Review changes** against the starting commit, **Open worktree**,
and **Copy branch**. A completed turn can leave staged, unstaged, or
untracked changes. Show **Uncommitted changes** in that case and let the
user ask the worker to commit or open the worktree. Show **Ready for
review** when an idle or stopped worker has committed changes to review
and has no uncommitted project files. Its branch is already available in
the source repository.

The coordinator can report the branch, starting commit, target, and result
tip from the task summary. Add local checkout information to the shared
control contract and teach Construct to render it. Publication fields
keep their existing meaning. Host paths belong in Deck's task view.

The user decides how and when to integrate. They can merge locally or use
their normal Git or GitHub workflow. A coordinator can split independent
work and wait to dispatch a dependent worker until the source HEAD
includes its prerequisite. The coordinator's normal role is to
report results and recommend an integration order. Deck performs no
merge, rebase, push, or pull request creation as part of worktree
management. Local agents continue to use the host's Git workflow and
permissions.

Version one detects a local merge only if the worker has new commits, has
no uncommitted project files, and `git merge-base --is-ancestor`
confirms its branch tip is reachable from the recorded integration ref
in the source repository. Deck records the target as a full local ref
at dispatch, so switching the source checkout to a different branch
cannot change the target. Detached HEAD can seed a worker with no
automatic integration target. A missing target or a Git
error gives an unknown integration state. After a squash or rebase merge,
the user can use **Mark done**. Mark done records the user's decision
without changing Git state.

### Keeping and removing worktrees

Stopping, restarting, closing Deck, and marking a task done preserve its
worktree. **Remove worktree** is a separate action available from the task,
including after it settles. It names the branch Deck will keep. Deck
confirms the worker has exited and rechecks the recorded worktree and
branch before removing anything. If it finds staged, unstaged, untracked,
or ignored files, or cannot verify that the branch retains the work,
Deck keeps the directory and explains what to save or clear. Ignored
files can include local data, even if some are build output.

Use Git's ordinary worktree removal for clean, verified checkouts. Keep
the task branch and its name in the task record. Report blocked or failed
removal and allow retry. The same rules apply during recovery. Neither a
merge signal nor a daemon restart authorizes worktree removal.

### Runtime ownership and default agent support

Reserve the task ID and record the workspace intent before provisioning.
The intent carries the canonical source checkout, its Git common
directory, the starting commit, and an optional integration ref. The
task ID determines the branch and path. Provision before the first
worker launch under the task's liveness lock. Retrying incomplete setup
may reuse resources only when they match the recorded intent. Once the
workspace has been set up, resume uses it as-is and surfaces a missing
or changed workspace for recovery. Deck preserves existing worktree
contents and starts the worker only in its verified worktree.

A linked worktree keeps the worker's files and index separate. Git refs,
objects, and configuration are still shared with the source repo.
Construct's default shell sandbox currently cannot follow the linked
`.git` pointer to inspect or stage changes. Make bounded, task-scoped Git
status and staging available alongside Construct's approved commit
workflow before enabling parallel local Git workers. Prove the default
worker can edit, inspect, stage, and commit its own files without a grant
to the source checkout or unrestricted shared Git metadata.

### Follow-up: one integration task across backends

Use a regular worker with an integration role. The coordinator gives it
a target, the target's starting commit, and the ordered task IDs and exact
commits to combine. Deck resolves those inputs from task records and keeps
one active integration task per destination. The worker merges in its
own checkout, resolves conflicts, runs the combined tests, and reports
what it changed. The coordinator can ask the user about a resolution
when the task briefs disagree. The user still decides what lands.

| Destination | Where the integration worker gets the commits | Where the result goes |
|---|---|---|
| Local branch | Task branches in the source Git repository | The integration task's local branch |
| New PR | Published worker branches from the configured GitHub repository | One draft PR from the integration task's branch, using the existing gateway |
| Existing PR | The current PR head and published contributor commits | A proposed update from the integration task's branch, with an explicitly authorized fast-forward to the PR head if supported |

Agents contributing to one PR still work on their own branches.
For an existing PR, start from its current head so the result preserves
the commits already under review. In the first release, hand back the
result branch for the user to apply. A draft PR from the integration
task's own branch already fits the VM gateway policy. To support an
existing PR update in Deck, give just the integration task a scoped
publication grant under [ADR 0008]. Check the expected head at the write
and refresh the integration if the PR advanced.

A local integration task needs bounded Git operations to start and
finish a merge under Construct's sandbox. A VM integration task can
fetch worker commits already published under project policy. If a commit
exists only on a worker disk, the integration needs an approved way to
receive it. In either backend, missing inputs keep the task waiting with
a reason.

The first worktree slice exposes the branch and commit information this
role needs. The integration workflow follows separately.

For VM integration, Deck pins the target and the published contributor
commits before it starts the agent. Deck fetches from the configured
repository with the host's GitHub credential and hands the guest a clean
checkout through the existing import share. The guest's HTTPS proxy passes
encrypted Git traffic through, so it cannot attach the host credential.

The GitHub gateway accepts only the integration worker's own branch. Its
published commit must contain the target and every contributor pin. New PR
integrations can open a draft from that branch. Existing PR integrations
return the result branch for the user to apply.

Local coordinator workers record the source Git repository, its starting
commit, and the local target ref before launch. Deck registers and
materializes the worktree through private Git configuration. Resume checks
the original registration and preserves files in the worktree.

A local integration uses the same worktree setup at the selected branch's
pinned tip. Deck resolves contributor task branches in the same repository.
A branch with no contributor commit leaves the integration waiting. A
request retry can fill missing inputs and keeps the recorded pins.
Coordinator summaries carry the local branch and starting commit under
contract 1.7.0. Deck reports a review commit after checking that its
ancestry includes the starting commit and every contributor pin.

## Consequences

Workers can edit and test in parallel without overwriting each other's
working files or staging each other's edits. Edits to the same lines,
incompatible interface changes, and shared external services can still
conflict. Keep dependent changes sequential and review each branch
against its target before integrating. A worktree is directory isolation
for a local agent. A VM remains the stronger boundary.

Each checkout starts with its own build directories. Keep the usual
toolchain download caches and accept cold builds in version one. Shared
mutable build output can reintroduce contention.

A standalone local clone is the alternative if the default Construct Git
workflow cannot be supported on linked worktrees. It keeps Git metadata
inside the workspace, at the cost of disk and setup time. Deck would need
a user-facing action to import a committed tip into a reserved task branch
in the source repository before deleting the clone. Linked worktrees
make the result branch available without that extra step.

## Evidence and acceptance

- Deck `bc70e80` and the merged guard `a6909d4`. Coordinator creation is in
  `daemon/control_create.rs`. The runtime already holds a liveness lock
  through backend start and destroy. The project index can rebuild from
  task records, so excluding worktree paths only at creation is
  insufficient.
- A temporary Git repository showed that two worktrees can commit
  independent edits to the same path. The edits still conflicted when
  merged together. A squash merge failed the ancestry check, and a
  fast-forward passed. Ordinary removal refused dirty tracked and
  untracked work and removed a worktree containing only ignored files.
  Force removal erased uncommitted files while leaving the branch.
- A scripted run with the local Construct 0.4.0 debug binary on macOS
  edited a linked worktree but failed at `git status` with exit 128. The
  same scripted edit, status, stage, and commit passed in a standalone
  clone under the default sandbox. An explicit local branch import from
  that clone left the source branch and files intact.
- Cover the default Construct stage-to-commit run, two concurrent workers
  and an unchanged dirty source checkout, retry and resume, project
  registration and index rebuild, dirty cleanup refusal, and branch
  retention after clean removal. Drive the coordinator and task views
  through dispatch, review, Mark done, and removal. Check the coordinator
  receives real branch data and the UI distinguishes a committed result
  from an idle worker with unsaved changes.

[#200]: https://github.com/strf0x1/deck/issues/200
[#207]: https://github.com/strf0x1/deck/issues/207
[#208]: https://github.com/strf0x1/deck/pull/208
[ADR 0008]: 0008-github-workflow-end-to-end.md
