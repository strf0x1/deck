# Live testing bug fixes: #77, #78 and #71

2026-11-06, Linux/x86_64, on PR #79's task-ID fix (parent `0e4e91a`).
This is a targeted follow-up, not a rerun of the full 161-check matrix.

## Credential-name isolation (#77)

Every decision now binds its grant to the approved credential name. Timed,
task and project grants persist that binding; Once retains its write
fingerprint as well. Values and fingerprints are not part of this identity,
so rotation under the same name preserves authority. A different name or
switching between a credential and no credential asks again. Gateway cached
model admission and admission after a queue reply both check the name;
web admission requires no credential. GitHub profile creation binds the
host-configured token name and includes it in idempotence matching.

Migration is deliberately conservative: a persisted rule with no credential
field matches only requests without a credential. Old model and GitHub rules
need a new approval or profile decision; Deck does not guess their old name.
Old credential-free web rules continue to work. Tests cover all reusable
scopes, reopen, daemon control, legacy rules and real gateway sockets, including
a queue that incorrectly answers allowed for a mismatched durable rule.

## VM working directory (#78)

The window uses `/workspace` for VM session creation, load and attach. The
host task spec remains the project identity and import source. Local tasks
continue using their recorded host directory.

The updated `timed_ui.py` runs Construct directly, with no wrapper or alias.
It checks `pwd`, an imported project file and absence of the host path inside
the guest, then stops and restarts the task through the real window and
checks the same session and cwd again. All **14/14** checks pass, including
public TLS and Construct's original timed deadline. The guest is the current
Construct image from the earlier 2026-11-06 acceptance manifest, with libkrun
1.19.7/libkrunfw 5.6.2. An initial run selected the older general-purpose image:
cwd and public TLS worked, but three deadline metadata assertions failed.
Selecting the image used by the original timed acceptance restored those
checks; no product assertion was weakened. The final screenshot was inspected.

[Window results](evidence/2026-11-06-bugfix-vm-ui.json) ·
[Window screenshot](evidence/2026-11-06-bugfix-vm-ui.png)

## Locked Secret Service classification (#71)

The isolated GNOME keyring probe reproduced the old failure on lock cycle 0:
Deck reported missing while D-Bus SearchItems returned zero unlocked and one
locked match. The follow-up `secret-tool search --all` exited successfully
with 77 stdout bytes and 189 stderr bytes, but no parsed name attribute.
The classifier had mistaken absence of that attribute for absence of the item.
No raw search output or credential value was recorded.

A failed lookup now reports missing only after a successful search with both
stdout and stderr completely empty. Partial metadata or diagnostics are
unavailable. The unit and real-process tests include successful search output
without a name, diagnostic-only output, genuine absence and failed search.
The live driver now repeats 20 immediate lock/read/unlock/read cycles; all
classify locked access as unavailable and restore the original synthetic value.
The full private-store probe passes **11/11**, including single-flight,
five-second bounds, rotation and service restart recovery. It uses a private
D-Bus session, temporary HOME and synthetic value; the user keyring is untouched.

[Private-store results](evidence/2026-11-06-bugfix-private-secret.json)

## Checks and limits

`cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`
(**402 passed, 2 ignored**), `scripts/acp-contract.sh verify` and Python
compilation pass on Linux. No ACP contract or Construct changes. VM-window
coverage is opt-in and was run live, outside the default Rust suite. macOS
validation belongs to CI; no live macOS run was performed here. All disposable
tasks, VMs, windows, daemons and synthetic credential files were cleaned up.
