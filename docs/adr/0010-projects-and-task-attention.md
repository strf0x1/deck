# 0010 — One window over all projects, and a "Needs you" strip

Status: proposed, 2026-11-07; §1, §7 and §9 amended 2026-11-09 (one
sidebar in place of the rail, the strip and the sessions column); §1, §5
and §9 amended again 2026-11-09 (a project view, task titles, and a
hierarchy that tells projects from tasks). Answers #124. Builds on 0001 (tasks and
records), 0002 (live attach), 0004 §5 (the approval queue) and 0008 §3–4
(the repository is part of the task).

## Context

Deck opens on the directory it was started in. `main.rs` passes
`current_dir()` to `app::run`, and the window owns one task for that command
and directory. Launched from the Dock or a `.desktop` file, Deck has no
sensible directory, and there is no way to open, list or switch projects.

The sessions column already lists every task that hasn't been destroyed,
whatever its directory. Nothing tells the user which of them is waiting on
them. Every agent client we looked at (the session-organization dossier,
Amp Spaces, Intent) ends up with the same "pile of chats" unless two things
stay separate: *which work needs me now*, and *which work is finished*.

This ADR is the first of three:
- this one: projects, and the two states of a task;
- spaces: durable groupings with a brief, which may cross repositories (a
  later ADR);
- the coordinator agent (#126).

Spaces and the coordinator build on what this ADR settles.

The user's decisions (2026-11-07):
- one window over all projects;
- a cross-project "Needs you" strip;
- projects identified by their root commit;
- spaces before the coordinator;
- a coordinator that creates and prompts tasks from the start.

## Decision

### 1. One window over all projects

The window has two regions (amended 2026-11-09):
- a **sidebar** on the left, holding "Needs you" (§7), the projects and
  their tasks (§9), and the settled fold;
- the **main area**, showing either the selected task (a header with its
  title, then its transcript) or the selected project's **project view**.

The main area shows one of two kinds of place (amended again 2026-11-09):
- **Clicking a project** in the sidebar shows its project view: the
  project's name, checkout, branch and origin; a composer that starts a
  task in it (§5); its unsettled tasks as cards with their status line,
  publication and last activity; and a footer with the agent command and
  the project's rules. Selecting a project never jumps to one of its tasks.
- **Clicking a task**, or a Needs-you item, shows that task.
- The task header's project name is a link back to the project view.

The sidebar highlights whichever place is shown, so a project row and a
task row are never highlighted at once.

The sidebar stays narrow until the pointer reaches it, so the transcript
keeps the screen. Narrow, it shows the Needs-you count, one glyph per
project with a dot when any of its tasks needs you, the selected project's
marker, **Open project…** and the pin. Under the pointer it opens to full
width, floating over the transcript, and closes when the pointer leaves.
Pinned, it stays open and takes its width from the transcript instead of
covering it.

This replaces the first version's four regions: a strip across the top, a
project rail, a sessions column and the transcript. Three side-by-side
columns cost width and read as three navigations for one choice: which
task you are looking at.

The window no longer *is* one task: it has a selected task, and it can
switch between tasks in any project without changing windows.

On launch:
- If the current directory is inside a git checkout, Deck selects that
  project, opening it first if it's new.
- Otherwise, Deck selects the project used last.
- With no projects at all, it shows an empty state with **Open project…**
  (a folder picker) and the recent projects.

`deck [--] [agent argv…]` keeps its meaning. The argv becomes the default
agent command of a project opened by that launch.

### 2. A project is its repository's root commit

A project's id is `git:<sha>`, where `<sha>` is a root commit of the
checkout (`git rev-list --max-parents=0 HEAD`).
- **Several roots** (merged histories): the smallest sha is recorded the
  first time the project is opened. A checkout matches the project when any
  of its roots equals the recorded one, so a later subtree merge doesn't
  split the project.
- **A shallow clone** (`git rev-parse --is-shallow-repository`) has no true
  root. Opening one fails with a message that says to run
  `git fetch --unshallow`.
- **A directory with no commits, or outside git,** opens as `path:<canonical
  path>`, shown as unversioned. It is never merged with a `git:` project
  automatically.

What this means:
- Clones, worktrees and forks of one repository are **one project** with
  several checkouts. That is the body of work the user thinks of. A fork's
  publication destination is still resolved per checkout at create (ADR
  0008 §3).
- Renaming or moving a checkout doesn't change the project.

Each task records its project at create: `Spec` gains `project`, alongside
`cwd`. A task's checkout is still its `cwd`.

### 3. `deckd` owns the project list

`projects/index.json` under the data root holds one record per project.
This is not `approvals/projects.json`, which holds project grants. Each
record has:
- `id`;
- `name`: the first checkout's directory name, renamable;
- `checkouts`: the paths seen for this project;
- `argv`: the default agent command;
- `opened`: when it was last opened.

The window and, later, the coordinator read it through `deckd` with three
requests:
- `projects` lists the records;
- `open_project { path }` resolves the path, adds the checkout and returns
  the record;
- `forget_project { id }` hides the project, and its tasks stay.

The first `deckd` to run this ADR fills the list from existing task records
by resolving each task's `cwd`.

### 4. Project grants follow the project id

Approvals' project grants and rules are keyed by the project string a task
reports. Today that is the task's `cwd` (`vm.rs`). From now on it is the
project id, so a rule approved in one checkout applies in that project's
other checkouts and worktrees.
- **Migration:** a path-keyed grant moves to the project its path resolves
  to. One that doesn't resolve stays as it is.
- **What this widens:** a fork checkout shares its upstream's web rules.
  GitHub rules stay scoped by repository (ADR 0007 §3), so a fork never
  inherits the right to publish upstream.

### 5. Opening a project starts no task

A project's view always has the composer (amended again 2026-11-09: it was
the empty state of a project with no tasks). The first prompt creates the
task with the project's `argv` and the selected checkout, then sends the
prompt. No VM boots until there is work for it.

The empty state says what a task will see: tracked and untracked files,
without ignored ones (ADR 0008 §4).

### 6. Every task has two separate states

**Attention: does it need you now?** A task needs you when any of these
holds:
1. a pending approval for it in `deckd`'s queue (ADR 0004 §5);
2. its turn is `waiting` for a permission answer (ADR 0002 §2);
3. a turn ended (`idle` with a stop reason) after the last time you looked
   at that task;
4. its lifecycle shows a failure: an unclean exit, or interrupted work after
   recovery.

"Looked at" is recorded by `deckd` as `seen` on the task, the turn number
last shown while the task was selected. So it survives restarts and agrees
across windows.

**Settled: is it finished?** A task is settled when it is destroyed, when
its publication is merged (ADR 0008 §8), or when you mark it done. In the
sessions column, settled tasks fold below the unsettled ones and drop out of
the column after 14 days. Their records stay.

The two states are independent. A settled task can still need you (a failed
destroy), and an unsettled one usually doesn't.

### 7. The "Needs you" section

The section opens the sidebar (amended 2026-11-09; it was a strip across
the window's top).
- It lists every attention item from every project, oldest first.
- Each item is one row: its task's title, and the reason in a word or two
  ("asks", "wants", "finished", "stopped"). The full reason ("asks to run
  `rm -rf build`") shows on hover and on the item it opens.
- Selecting an item selects that project and task and opens the item: the
  permission card, the boundary card, or the turn's footer.
- With nothing pending, the section is one quiet line, "Nothing needs you".
  It never disappears, so its absence never reads as broken. Narrow, the
  sidebar shows the count only when it is above zero.

### 8. The window attaches to every live task

The holder never parses ACP (ADR 0002 §3), so `deckd` can't see turn state.
The window keeps one live attachment (`deck_acp::Client`) per running task
that advertises live attach, in every project. It derives items 2 and 3 of
§6 from each attachment's turn notifications, and gets items 1 and 4 from
`deckd`.
- Each attachment keeps its own `Session`, so switching back is instant.
  Only the selected task's transcript is rendered.
- A task without live attach contributes only items 1 and 4.

Each attachment replays its transcript once (ADR 0002 §2). With many live
tasks that is the cost to watch: a status-only attachment would be a
contract change (ADR 0006), made only if this one proves too slow.

### 9. Projects and their tasks, before spaces

Below "Needs you", the sidebar lists every visible project (amended
2026-11-09; it was a separate sessions column for the selected project).
- Each project is a group header. The selected project's group is open and
  lists its unsettled tasks, one row each, in the order the user dragged
  them, most recent first otherwise. Other groups are closed and show their
  task count and an attention dot. Clicking a header selects that project.
- A task row has its state dot, its title, and on the right either its
  attention word or its publication (`PR 141`). It keeps its actions
  (Mark done, the publication panel, destroy after a merge) behind hover.
  What a card's second line said (the agent's status) moves to the header.
- The settled fold closes the list.

**Telling projects from tasks** (amended again 2026-11-09). The first
version drew both as a dot, a name and a number at one size, and named
tasks after their directory, so a task called `agent` sat beside a project
called `agent_dotfiles`. One rule each:
- **A project** is a group header: a chevron, its letter tile (the same
  tile the narrow sidebar shows), its name in semibold, and on the right
  its task count, with an amber dot before it when a task needs you. A
  project row never has a state dot.
- **A task** is indented under its project to the tile's edge, in the
  smaller, dimmer text, and always starts with its state dot. Mark done is
  a check icon shown on hover.
- **Needs-you rows** say where they belong: `deck › fix rail drag`.
- Small "Needs you" and "Projects" labels separate the sections. The
  wordmark tile at the top goes, since it read as a project.
- Only the selected project's group opens; its chevron collapses it. A
  click anywhere else on a project's header selects that project.

**A task's title** is recorded by `deckd` on the task (`Task.title`), set
once from the first line of its first prompt (at most 60 characters). A
task that has none yet takes it from its replayed transcript when the
window attaches. Without either, it is "Untitled" and the first six
characters of its id. Never the directory name, and never the agent's name.
An agent-provided session title can replace it later; that is not part of
this amendment.

The spaces ADR adds spaces inside a project's group. A task outside any
space then sits in the project's **Inbox**, which is this same list.

## Consequences

- **One sidebar** (amendment, 2026-11-09): `ui/rail.rs` becomes the
  sidebar's open/close state, the sessions column and the top strip go,
  and the header moves over the transcript. Slices: #154, #155, #156.
- **The project view, titles and hierarchy** (amendment, 2026-11-09):
  `Task.title` and a `title` daemon request; the sidebar's rows restyled;
  the main area's project view. Slices: #162 (titles, now), #163 (hierarchy, after #160),
  #164 (the project view, after #163).
- **The window's single task goes away.** `task.rs` and `ui/attachment.rs`
  manage a set of attachments keyed by task id, and `ui/root.rs` a selection.
  Switching tasks no longer detaches the old one. `New session` creates in
  the selected project's selected checkout.
- **Records grow, additively.** `Spec.project`, `Task.seen`, `projects/index.json`,
  and the new daemon requests. Grants migrate once.
- **Dock and `.desktop` launches have a sensible start** (§1). The
  packaging work in #125 needs nothing from this ADR beyond that.
- **The coordinator's raw material** is here: projects, task attention and
  settledness are the facts it reads from `deckd`.

  The coordinator also needs turn state when no window is open. That is the
  first thing #126 must add, because today only an attached window sees it.
- **Rejected options:**
  - one window per project: the cross-project strip is the point;
  - identifying a project by `origin`: it changes with renames and forks,
    and doesn't exist for a local repository;
  - identifying a project by directory: one project would become several
    with worktrees.
- **Revisit** if attaching to every live task is too slow to open (§8), or if
  forks sharing web rules (§4) surprises users.

## Evidence

- Design inputs:
  - the session-organization dossier;
  - the structural mockup reviewed on 2026-11-07;
  - Amp Spaces (<https://ampcode.com/news/spaces>);
  - Intent (<https://intentapp.dev/>).
- Ground truth read for this ADR:
  - `main.rs`;
  - `ui/tasks.rs`, whose column already lists all non-destroyed tasks;
  - `relay.rs`, a JSON-agnostic relay that delivers lines only to an
    attached peer;
  - ADR 0002 §2 (turn notifications, questions asked again on attach);
  - `approvals.rs` and `vm.rs` (project grants keyed by `cwd`);
  - ADR 0008 §3–4.

No implementation yet. Slices:

- Runtime, no GPUI:
  - #137: root-commit identity, `projects/index.json`, daemon requests,
    `Spec.project`.
  - #140: project grants keyed by project id, with migration (after #137).
  - #138: `seen` and Mark done in task records.
- Window:
  - #139: one attachment per task instead of one window task.
  - #141: project rail, launch selection, first-prompt create (after #137
    and #139).
  - #142: the "Needs you" strip, seen turns, the settled fold (after #138,
    #139 and #141).
