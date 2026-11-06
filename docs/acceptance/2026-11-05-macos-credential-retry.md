# Live macOS credential retry verification, 2026-11-05

PR [#72](https://github.com/strf0x1/deck/pull/72), commit
`13e84f93f5c2b8a9cb8c305cb8687483293c49e1`, was exercised on macOS 27.0,
build 26A428, arm64. The core pause and retry behavior passed live development
checks. This run does not close release-identity acceptance or the findings
below.

## Environment and isolation

The host initially had no signing identities. A dedicated self-signed
certificate, `Deck PR72 Local Test Signing`, was created in a disposable
Keychain. Both tested binaries used identifier `com.sprawlcommons.deck.pr72`
and the same certificate. Their designated requirement was:

```text
identifier "com.sprawlcommons.deck.pr72" and certificate leaf = H"7d9e99f88e08466dc1e607c539559d2ab0b2bf2b"
```

No system-wide certificate trust was added. Keychain accepted item-specific
trust for this identity. Apple's [code signing guide](https://developer.apple.com/library/archive/documentation/Security/Conceptual/CodeSigningGuide/Procedures/Procedures.html)
describes self-signed identities for designated-requirement validation; this
is not Apple-issued distribution signing.

The only credential was `DECK_PR72_TEST`, with a synthetic value. Test tasks
and the daemon used an isolated data root. Local fake-agent holders stood in
for VMM holders, using the same `vmm.json` credential-set fixture as the daemon
integration tests. The real gateway spoke to the real daemon and a local HTTP
fixture. No VM or external provider was involved. The native window ran the
real fake agent, and input used `DECK_CONTROL`.

The user answered Keychain dialogs. Computer Use refused access to
SecurityAgent, so no automated inspection or interaction with those dialogs
was attempted after that refusal. Dialog observations are user reports;
daemon, gateway, and window results are recorded independently.

## Observed results

| Check | Result |
|---|---|
| Caller times out before a late denial | The signed CLI listing returned unavailable at 5.05 seconds. The eventual refusal remained paused. |
| Repeated CLI listings after denial | Three listings returned the paused error in 35, 48, and 43 ms. |
| Repeated real gateway requests after denial | Three requests returned `503 deck_credential_unavailable` in 19, 22, and 2 ms. |
| Repeated Access-panel opens | Three close/open cycles retained the refusal. The panel showed the daemon's paused error. |
| Explicit host retry | The command reported cancellation cleared in 50 ms. A subsequent read produced a new dialog. |
| Successful read after Always Allow | The listing returned the active fingerprint in 58 ms. The panel showed active state and Revoke. |
| Real gateway after successful read | `/v1/models` returned HTTP 200. The local fixture confirmed the expected synthetic Authorization header, recording only a boolean. |
| Changed binary, same identity | A separate `cargo rustc` build had a different SHA-256 but the same designated requirement. After daemon replacement, a listing succeeded in 56 ms without another reported dialog. |
| Locked Keychain | A listing returned unavailable at 5.05 seconds. Retry returned in 35 ms and reported that the pending read was unchanged. |
| Automatic recovery after unlocking | After correct authentication for the first lock test, a listing recovered automatically in 46 ms. No completed refusal had been cleared. |
| Wrong password | The user reported that one wrong password was rejected and the dialog stayed open. Retry left the pending read unchanged. |
| Other callers during wrong-password challenge | The gateway returned unavailable in 5.01 seconds; the task-specific credential listing returned unavailable in 5.02 seconds. |
| Closing the wrong-password dialog | The next listing returned paused in 54 ms. Programmatically unlocking the disposable Keychain left the refusal paused; explicit retry then restored an active listing in 47 ms. |

The wrong-password challenge did not itself complete the OS read. This run
therefore does not independently establish a live `errSecAuthFailed` return.
The macOS unit test covers its status mapping, together with cancellation,
missing items, and interaction-not-allowed.

## Findings

1. **The Access panel clips the retry instruction.** At the default
   1180-point window width, the credential row displays the start of the
   denial message but hides the end containing the command. The single-row
   detail in `crates/deck/src/ui/access_panel.rs:43` needs to make the full
   recovery instruction readable, for example by wrapping below the name.
   A dedicated Retry button is outside this PR's stated scope; readable
   instructions are still needed for the CLI-only recovery path.
2. **A repeated CLI listing exceeded the test's 20-second timeout while the
   wrong-password dialog remained open.** Retry stayed responsive and
   task-specific listings and gateway fetches respected the five-second
   bound. The CLI enumerates names through synchronous Keychain metadata
   search before the bounded value reads. That is a possible cause, not a
   sampled-stack diagnosis. The enumeration code is identical at the PR base
   `c566fa7`; this observation was not rerun on that baseline and is not
   claimed as a regression introduced by #72.

## Layout follow-up for #73

The follow-up for [#73](https://github.com/strf0x1/deck/issues/73) moves the
credential detail below its name, status, and action. The detail wraps within
the row instead of extending beyond the panel. The original observation above
describes the tested `13e84f9` layout; the follow-up fixes that clipping.

The real-layout regression
`credential_retry_instructions_fit_the_access_panel_at_supported_widths`
failed before the layout change because an 829-point detail extended beyond
its row at the 1180-point window width. It passes after the change at widths
1180 and 760, with both ordinary and 64-character credential names. It also
checks that status and Revoke remain in bounds and that clicking Revoke opens
the correct confirmation.

Native macOS window inspection confirmed the complete retry instruction at
both widths. A long name and its command wrap at 760 points; panel scrolling
makes the complete message and the next credential's Revoke button reachable.
This layout check used synthetic replies from a local daemon fixture and did
not access Keychain. Screenshots, the fixture, and before/after test logs are
under `~/Library/Application Support/deck-acceptance/macos/access-retry-layout-73/`.
Formatting, clippy, and the full local suite passed with 392 tests and two
ignored. The separate CLI timing observation remains unresolved.

## Checks and evidence

`cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` passed.
`cargo test` reported 391 passed, zero failed, and two ignored. Both PR CI
jobs were also green at the tested head.

The [evidence manifest](evidence/2026-11-05-macos-credential-retry.json)
contains selected replies, timings, binary hashes, and screenshot hashes.
Raw logs, screenshots, and the local driver are retained under
`~/Library/Application Support/deck-acceptance/macos/credential-retry-pr72/`.
The driver is a record of this interactive run, not an unattended acceptance
suite. Its setup used a restricted test item created by `security
add-generic-password`, and the test sequence used `credential list`,
`credential retry`, real gateway HTTP requests, and native-window controls.

All test tasks, the daemon, the window, the gateway, and the HTTP fixture were
stopped. The synthetic Keychain item was deleted, and the original Keychain
search list was restored. The test identity remains in the locked
`signing.keychain-db` in the evidence directory, which has mode 0700. Loose
private-key and PKCS#12 export files were removed. No user credential or
provider account was changed.
