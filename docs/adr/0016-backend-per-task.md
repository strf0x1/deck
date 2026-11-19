# 0016 — A task picks its backend

Status: accepted, 2026-11-18. Amends ADR 0003's "one runtime serves one
backend".

## Context

ADR 0003 gave each `deckd` one backend, chosen by whether `DECK_VM_IMAGE` was
in its environment when it started. Nothing in the window could choose: a
user who wanted one task sealed in a VM and another running locally had to
restart the daemon with a different environment, and the window could only
guess which backend the next task would get. Benchmark runs used local
workers because the daemon had been started without the VM variables, not
because anyone picked local.

## Decision

1. `Runtime::open_with` takes several backends, each under a distinct name.
   The first is the default for a task that names none. `deckd` always
   offers `local`, and puts `vm` first when `DECK_VM_IMAGE` is set, so a
   configured VM is still the default.
2. `Spec.backend` is what a caller asks for. `create` runs the task on that
   backend, refuses one this daemon lacks (and never falls back to another),
   and clears the field: the record's own `backend` says where the task runs.
   A window's `run` finds an idle task on the backend it asked for.
3. A record is run, reconciled, adopted and destroyed only by the backend
   whose name it carries. A record naming a backend this daemon lacks is left
   alone, as ADR 0003 said for a switched data root.
4. A coordinator's worker runs where its coordinator runs. The one-local-
   worker-per-checkout rule (deck#200) is decided by the coordinator's
   backend, not the daemon's default.
5. One coordinator per scope (ADR 0011 §2) holds across backends; a
   coordinator on a backend this daemon lacks does not count.
6. `deckd` reports its backends, the default first, in the settings snapshot
   (`backends`). The window's New session menu shows a "runs in" choice
   while more than one is offered. Only a pick other than the default is
   sent, as with the model (ADR 0012 §4). Control protocol 6.

## Consequences

- One daemon serves VM and local tasks side by side. A window can start
  either without restarting `deckd`.
- The VM still requires `DECK_VM_*` and, on macOS, a signed `deckd`. Without
  them the menu offers no choice and everything is local, as before.
- There is no persisted default beyond "VM when configured". A user who wants
  local by default with VM configured picks it per session.
- Revisit if a third backend appears (its name goes in the window's
  `session_backend::describe`), or if a default needs to be saved in
  `settings.json`.

## Evidence

- Benchmark runs on the default setup used local workers: `deckd` was
  started without `DECK_VM_IMAGE`.
- `crates/deck-runtime/src/runtime/backends.rs` tests: the default when none
  is named, refusal of an unknown name, a record from a dropped backend left
  alone. `crates/deck/tests/view.rs`
  `the_menu_picks_where_new_session_runs_while_deckd_offers_a_choice`.
