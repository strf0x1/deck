# The GitHub workflow on macOS, 2026-11-07

The ADR 0008 §9 checklist from [the Linux run](2026-11-06-github-workflow.md),
repeated on macOS 27.0 (26A428), arm64 (Apple M4 Pro): real Construct
`255de54` (0.4.0) in a libkrun VM with its scripted model, the real Deck window
driven through `DECK_CONTROL`, and a **public** scratch GitHub repository.
**54/54 assertions pass**, including a real GitHub-hosted Actions check that
runs, is observed as `checks running`, and completes as `success` in the chip
and the publication panel.

The Linux run left macOS unrun. This one repeats its cases on the other
supported host; it does not replace the Linux evidence, and it is one run.

## Environment and isolation

| | |
|---|---|
| Host | macOS 27.0, build 26A428, arm64; Hypervisor.framework |
| Deck | `2e564b2` (merged main) plus this run's working-tree changes: the driver's macOS port and the guest-entry `sync` in `scripts/vm-image.sh` |
| Construct | `255de54` (0.4.0, `origin/main`), built for the guest at `rust:1.91-bookworm`; the same commit serves the host's scripted model |
| libkrun | 1.19.7 with libkrunfw 5.6.2, built from source per `docs/vm-spike.md` |
| `deckd` | `target/debug/deckd`, signed with `scripts/vm-sign.sh` (Hypervisor entitlement) and copied out of `target` so cargo cannot relink it |
| Guest | arm64 Debian bookworm built by `scripts/linux-acceptance/build_image.py` from `scripts/vm-image.sh`'s recipe, with curl, Python 3, OpenSSH client, iproute2 and iptables |
| Repository | `strf0x1/deck-acceptance-scratch-public`, public, created for this run |
| Credentials | the 0600 file store; `DECK_GITHUB_TOKEN` is the local `gh` user token (names and fingerprints only in the manifest), `OPENAI_API_KEY` is synthetic |
| Model | `construct-dev serve` on host loopback, one scripted script per turn |

A public repository asks for a card on every write by default (ADR 0008 §3),
so the project file declared the explicit opt-in
`approvals = "own-branch-public"`. Its workflow is a real GitHub-hosted job
(`ubuntu-latest`) that runs `sleep 60`, so the check stays in progress long
enough for the window to observe it. The driver waits for the runs to complete
and records their conclusions; nothing is cancelled.

Screen recording permission was granted to the shell that launched Deck;
`screencapture -x -o -R<x,y,w,h>` captures the window's reported frame. The
screenshots are downscaled to 1180 points wide for the repository.

## Observed results

The chip transitions were observed live: `draft: #2` → `checks running: #2`
(7.1 s) → `ready: #2` (5.6 s) after the checks completed, then `merged: #2`.
The publication panel listed both real runs as `success`.

| Case | Result |
|---|---|
| edit → commit → push → draft PR, no card; scope `default` on `/events` | PASS |
| PR chip on card and header; panel opens | PASS |
| checks read; chip running; panel lists the runs | PASS, real GitHub-hosted job, `success` |
| second push fast-forwards, previous set | PASS |
| forced history refused `deck_github_refused` with `current`, no retry | PASS |
| marking ready: card with summary, allow, PR ready on GitHub | PASS |
| `approvals = "ask"`: first push shows a card | PASS |
| git-ignored `.env` absent from `/workspace` | PASS |
| stop/resume keeps chip and panel | PASS, after the marker fix below |
| merge: chip merged, Destroy asks twice | PASS |

The scratch pull request is
[#2](https://github.com/strf0x1/deck-acceptance-scratch-public/pull/2), merged
by the driver's GitHub call (`merge_method=merge`). Its two check runs, one for
the push and one for the pull request event, completed `success`.

## Findings

1. **The guest checkout marker was not durable (fixed here).** On this host the
   lifecycle contract `vm_task_runs_attaches_reattaches_stops_and_destroys`
   failed at `resume` with `VMM startup failed: VMM did not report startup`,
   and case 9 stalled with the session `down`. The resumed guest console said
   `deck-guest: import share missing`: a stop kills the VMM without a guest
   poweroff, so `/workspace/.deck-provisioned` (and the checkout) could still
   be in the guest's page cache when the VM died. A resume cannot import again
   — the first boot consumed the share — and the entry exits before opening
   the ACP stream. `scripts/vm-image.sh` now runs `sync` after provisioning
   the workspace. Before the change the VM test failed at `resume` in every
   run and the acceptance aborted at case 9; after it the lifecycle suite is
   3/3 and case 9 passes. The fix applies to every host, not only macOS.
2. **The driver is now cross-platform.** `Window.launch` spawns the window
   directly on macOS (no `hyprctl` dispatch), screenshots use `screencapture`
   with the reported bounds, and quit/restart are `cmd-q`/`cmd-r` rather than
   `ctrl-q`/`ctrl-r`. The runtime directory for sockets defaults to a short
   `/tmp` path when `XDG_RUNTIME_DIR` is unset. A private repository still
   runs the original path unchanged.
3. **The Construct 0.4.0 tag predates the capability this case needs.** A
   first attempt with the `v0.4.0` tag (`62f7412`) produced the correct
   `this agent cannot publish from a sandbox` notice and a vendor permission
   card for `commit`, because that tag pins contract 1.0.0 and does not
   advertise `github_operations`. The run uses `origin/main` (`255de54`, the
   same revision as the Linux run), which advertises contract 1.2.0.

## Limits

- The model is scripted, so each turn's tools and reply are fixed by the
  driver; the refusal is proven by Construct's tool result and the single
  publish call, not by model reasoning.
- The token is the local `gh` user token (`repo` scope), not a fine-grained
  token restricted to the scratch repository; it never leaves the host store.
- The scratch CI is a real hosted job but a deliberate no-op (`sleep 60`); it
  proves check-run registration and state, not a build.
- The public repository exercises the explicit `own-branch-public` opt-in,
  not the private-repository inference where the default admits writes.
- One host, one run. The macOS Keychain/Secret Service store, the grep/mirror
  drivers and the timed-grant driver were not run here.

## Checks and evidence

The run used `cargo build` at the revision above; the workspace's
`fmt`/`clippy`/`test` checks were not part of this acceptance. The manifest
records the revision, the dirty harness/image files, binary hashes, both image
hashes, the guest Construct hash and the credential names/fingerprints.

- [Results (54 assertions)](evidence/2026-11-07-github-macos.json) and
  [manifest](evidence/2026-11-07-github-macos-manifest.json).
- Chip transitions: [running](evidence/2026-11-07-github-macos-chip-running.json),
  [result](evidence/2026-11-07-github-macos-chip-result.json),
  [merged](evidence/2026-11-07-github-macos-chip-merged.json).
- Screenshots: [panel](evidence/2026-11-07-github-macos-panel.png),
  [checks](evidence/2026-11-07-github-macos-checks.png),
  [running](evidence/2026-11-07-github-macos-running.png),
  [ready card](evidence/2026-11-07-github-macos-ready-card.png),
  [ask card](evidence/2026-11-07-github-macos-ask-card.png),
  [stopped](evidence/2026-11-07-github-macos-stopped.png),
  [merged](evidence/2026-11-07-github-macos-merged.png),
  [Destroy armed](evidence/2026-11-07-github-macos-destroy-armed.png).
- Raw logs, VM images and the scratch checkout remain under
  `~/Library/Application Support/deck-acceptance/macos/github-workflow-2026-11-06/`.

All test tasks, the daemons, the window, the scripted endpoint and the VMMs
were stopped; the tasks were destroyed. The file credential store holds only
the two names above.
