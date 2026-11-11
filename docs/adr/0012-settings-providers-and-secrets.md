# 0012 — Settings: named providers, models and secrets, kept by `deckd`

Status: accepted, 2026-11-10. Amends ADR 0004 §3 (the model gateway's one
upstream) and §4.2 ("Entering a value in the UI is deferred"). Leaves 0004
§4.6 alone: the local backend still resolves Construct's own config and
secrets.

## Context

A VM task's provider and model come from `deckd`'s environment.
`vm.rs` reads `CONSTRUCT_BASE_URL` and `CONSTRUCT_MODEL` once, when the
backend is built, and falls back to `boundary::DEFAULT_UPSTREAM` /
`DEFAULT_MODEL` (OpenRouter, `openrouter/free`). Every task the daemon
starts gets that one upstream, that one model as its allowlist
(`models: vec![self.model.clone()]`), and the one credential name
`boundary::SECRET` (`OPENAI_API_KEY`). Changing any of it means restarting
`deckd` with different variables. A person who opens Deck from the Dock
cannot do that at all.

Secrets are already in one place (ADR 0004 §4.2): the `DECK_CREDENTIALS`
file or the OS store, read only by `deckd`. Values go in through
`deckd credential set NAME`. The access panel lists the names in a task's
set and offers revoke. 0004 deferred entering a value in the UI.

Construct already has named providers (construct ADR 0027): a
`[providers.<name>]` table with a url, a key name and models, and a
`provider/id` selector. The request body carries the plain id. Inside the
guest, Construct sees only one endpoint, the relay, so the provider has to
be decided on the host.

The options for where settings live:

- **The window's own config file.** Simple, but `deckd` is the authority
  (ADR 0001). A task created by a second window or after a restart must get
  the same provider, and the gateway has to be configured from something
  `deckd` can read when the window is closed.
- **Construct's `config.toml`.** The guest's copy is agent-writable and
  untrusted (0004 §4). The host user's copy describes local runs, not
  what a sealed task may reach.
- **A `deckd`-owned settings file under the data root**, read and written
  through control ops. Chosen.

## Decision

### 1. `deckd` owns `settings.json` under the data root

`<data>/settings.json`, written only by `deckd`, atomically at 0600 (it
holds no values, but it is policy). A window reads it with `settings` and
replaces it with `set_settings {settings, revision}`. `revision` is a
counter in the file (0 while there is none), and the window sends the one
it last read. A stale revision is refused with the current
settings, so two windows cannot silently overwrite each other. The file is
versioned (`"version": 1`); an unknown version is refused at start rather
than rewritten.

```json
{
  "version": 1,
  "revision": 3,
  "default": "openrouter/openrouter/free",
  "providers": [
    {
      "name": "openrouter",
      "url": "https://openrouter.ai/api/v1",
      "secret": "OPENROUTER_API_KEY",
      "models": ["openrouter/free", "anthropic/claude-sonnet-5-5"]
    },
    {
      "name": "local",
      "url": "http://10.0.0.5:8000/v1",
      "secret": null,
      "models": ["qwen3-32b"]
    }
  ]
}
```

- `name` is `[a-z][a-z0-9-]{0,31}`, unique.
- `url` is an `http` or `https` base URL including its version segment, as
  `CONSTRUCT_BASE_URL` is today. `deckd` refuses anything else.
- `secret` is a credential *name* (0004 §4.1), or `null` for a keyless
  server. Never a value.
- `models` is the provider's allowlist, at least one id. Ids are opaque and
  may contain `/` (OpenRouter's `vendor/model`).
- `default` is `<provider>/<id>`. It splits at the first `/`, as construct's
  selector does, so `openrouter/openrouter/free` is the `openrouter`
  provider and the id `openrouter/free`. It must name a listed model.

**No file means the implicit provider.** Without `settings.json`, `deckd`
behaves as today: one provider named `default` from `CONSTRUCT_BASE_URL`
(or `DEFAULT_UPSTREAM`), secret `OPENAI_API_KEY`, and the one model
`CONSTRUCT_MODEL` (or `DEFAULT_MODEL`). The settings panel shows that
provider and saving writes it out as a real file. **With a file, the two
variables still override** the default provider's url and the default
model, and `deckd` logs that at start, as it does for `DECK_GITHUB_*`.
Scripts and tests that set them keep working, and the log says why the
panel disagrees with the file.

### 2. A task is pinned to one provider at create

`Spec` gains `model: Option<ModelChoice>` with the provider's name, url,
secret name, allowlist and selected id, resolved from the settings at
create (the window may pass a `<provider>/<id>` to override the default,
which must be in the settings). It is recorded like `Spec.github`
(ADR 0008 §3), so:

- a restart, resume or a second window uses what the task was created with,
  not whatever the settings say now;
- editing or deleting a provider affects only tasks created afterwards;
- a record without `model` resolves from the settings at the next start,
  the way `policy_for` does for GitHub, so an upgrade changes nothing.

The VMM's gateway config gets that provider's `upstream`, `models` and
`secret`. The guest still gets `CONSTRUCT_MODEL=<id>`, the relay URL and
the placeholder `OPENAI_API_KEY` (0004 §4). The guest name stays fixed
whatever the host secret is called, so nothing in the guest learns the host
name. The task's credential set (0004 §4.3) is the provider's `secret` (when
it has one) plus the GitHub token, replacing the hard-coded `SECRET`.

A runtime model switch stays inside the task's provider: a listed id is
allowed, an unlisted one asks through the approval queue exactly as now
(0004 §5). Switching provider means a new task. Routing one task to several
providers would need the guest's Construct configured with one provider per
host provider, each on its own relay path. That is deferred: nothing needs
it yet, and per-path routing adds a second way to address the upstream.

The local backend ignores `model` (0004 §4.6). The panel says so beside a
local task, and the composer status row shows `local · construct config`
in place of a provider.

### 3. Secret values can be entered in the window

This replaces 0004 §4.2's "Entering a value in the UI is deferred."

- New control ops: `set_credential {name, value}` and
  `delete_credential {name}`. `deckd` validates the name, writes through its
  one store (the same `Store::set` the CLI uses, so Keychain's access list
  names `deckd`), and replies with the new listing entry (name, fingerprint,
  store), never the value. `credential list` is unchanged.
- The control socket is already user-only (0700 runtime directory). The
  value crosses it once, in one request line. `deckd` does not log request
  lines that carry a value, and the value's type (`credentials::Secret`)
  serializes as a plain string but redacts itself in `Debug` and has no
  `Display`.
- The window holds the typed value only in the field's editor while the
  field is open. Submitting sends it on a background task and clears the
  editor before the reply arrives. A failed write shows the error and
  leaves the field empty; the user types the value again. The value is never
  in `Deck` state, the control socket's `state` reply, a log or a test
  snapshot.
- Setting a value that was revoked records a new fingerprint. The next
  request uses it (0004 §4.4 already reads per operation). Running requests
  on the old value are not closed by a replace. Revoke still does that.
- `delete_credential` removes the item from the store. If any live task's
  set names it, the panel asks for confirmation and says those tasks' next
  model request will fail with `deck_credential_missing`.

### 4. The settings panel

A gear button in a new footer row at the bottom of the sidebar opens it
(and its narrow glyph column). It takes the conversation
pane's body while open, like the access panel, and escape closes it. It has
three sections:

- **Providers.** One row each: name, url, key status (`set` with
  fingerprint, `not set`, `none` for keyless), model count, and the
  `default` mark. Add and edit open an inline form: name, url, key name
  (prefilled `<NAME>_API_KEY`), models one per line. The form validates as
  `deckd` will and shows `deckd`'s refusal when it doesn't. Delete asks once.
  It does not offer to delete the provider's secret, since another provider
  may name it.
- **Default model.** A picker over every `<provider>/<id>`. New sessions use
  it unless the user picks another for that session.
- **Secrets.** Every stored name with its fingerprint and store
  (`ListCredentials`, already behind the broker's one read per name).
  Set / replace opens a masked field. Revoke and retry are the access
  panel's, reused. Names no provider references are marked `unused`. The
  access panel stays task-scoped; it shows only that task's set.

Colours follow `theme.rs`: green for a set key, amber for a provider whose
key is not set (waiting on the human), red only on delete and revoke.

New session uses the default model. A small selector beside New session
picks another `<provider>/<id>` for that one task. The composer status row
already shows the agent and backend; it gains the task's `provider/id`.

### 5. Steps

1. `deck-runtime`: `settings.rs` (the pure type, validation, the implicit
   provider, env override), the file under the data root, `settings` /
   `set_settings` ops with revision checks. Unit tests for validation and
   resolution; a daemon test for the stale-revision refusal.
2. `Spec.model`, resolved at create; `vm.rs` builds the gateway config and
   credential set from it; `policy_for`-style fallback for old records.
   Lifecycle unit test, VM config unit test (`vmm.json` names the
   provider's secret and models, never a value).
3. `set_credential` / `delete_credential` ops and `daemon_client` calls;
   daemon tests that the value is stored, the reply has no value, and a
   request line with a value is not logged.
4. Window: `settings.rs` pure state (form, validation, the masked field's
   lifecycle), `settings_panel.rs` view, the gear, the New session model
   selector, the status row. `crates/deck/tests/view.rs` cases: add a
   provider, set a key (and that no state reply contains the value), pick
   the default, create a session on another model.
5. Docs: AGENTS.md's table, `docs/deckd.md`'s ops, 0004's status line.

## Consequences

- A person using the Dock app can add a provider, store its key and choose
  a model without a shell. A dev script that sets `CONSTRUCT_BASE_URL`
  still works and is logged as an override.
- Tasks are reproducible: a record says which provider and models it was
  allowed, so a later settings change cannot widen a running or resumed
  task.
- A secret value now passes through the window process for a moment. That
  is a new place it exists. It is the same user and the same machine as the
  CLI's stdin, and it is never written down; 0004's threat model (§8) does
  not treat the window as a boundary. A compromised window could already
  approve requests.
- Provider switching inside a task is ruled out until §2's per-path routing
  is decided. Revisit when Construct's picker needs it in VM tasks.
- No model discovery yet: models are typed. `deckd` fetching `/models` with
  the key for a "Fetch models" button is the natural next step. It is an
  outbound host request made outside any task, and it needs its own rule
  for which providers may be probed.
- The local backend stays outside all of this, so settings say nothing
  about what a local task can reach. The panel says that rather than
  implying otherwise.

## Evidence

- Read at `0fb531b`: `crates/deck-runtime/src/vm.rs` (env-read upstream and
  model, one-model allowlist, `credentials()` with `boundary::SECRET`),
  `crates/deck-runtime/src/boundary.rs` (defaults, placeholder),
  `crates/deck-gateway/src/model.rs` (`Settings { upstream, models, secret }`),
  `crates/deck-runtime/src/wire.rs` (credential ops),
  `crates/deck/src/ui/access_panel.rs`, `credentials.rs`.
- ADR 0004 §3–4, ADR 0008 §3 (policy recorded at create), construct
  ADR 0027 (named providers, `provider/id` split at the first `/`).
