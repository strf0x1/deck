# Linux acceptance evidence

The latest run is [the live timed-grant pass](2026-11-06-timed-linux.md):
**159/161 final assertions pass**, plus a separate **10/11** private-store
repeat. It finds #76–#78 and reproduces #71. Timed/public/UI results need the
documented local #76 fix; UI also uses a fixture path workaround for #78.
This does not accept unmodified merged main. Workspace checks pass with
**397 tests**. The report identifies failures and fixture boundaries.

The preceding fully passing run is [the merged #66/#67 rerun](2026-11-05-final-linux-rerun.md)
at `337db12`: **107/107 live assertions pass**, with **389 workspace tests
passing**. Its manifest records the host binary and harness hashes. This is
an opt-in Linux/KVM acceptance suite, separate from the default Cargo tests.

The subsequent [credential-retry slice](2026-11-05-credential-retry.md) records
its separate validation, live development macOS checks and an intermittent private
Secret Service observation (#71). It is not a replacement full-suite acceptance
run.

Start with the [driver instructions](../../scripts/linux-acceptance/README.md)
for prerequisites, image creation and commands. Keep images, library copies
and run evidence outside Cargo's managed build cache. The suite uses
synthetic credentials, disposable tasks and an isolated Secret Service;
some cases deliberately terminate their own daemon or gateway.

The suite exercises:

- Concurrent task isolation, native vsock policy and direct-network denials.
- Model credential stripping, injection, rotation and stream revocation.
- Real Construct boundary feedback and gateway-crash containment.
- Mirror bodies, redirects, resolver-cache changes and scoped profile rules.
- Grant expiry during daemon outage and malformed GitHub requests.
- Private Secret Service lock classification, single-flight, timeout and recovery.
- GitHub's total header deadline under trickled input.

Fixture boundaries and untested cases are part of each report. A passing
suite does not accept the entire ADR 0004 product scope, the scoped-reuse
approval amendment, or macOS behavior.

Historical runs explain the findings and fixes:

1. [Initial Linux acceptance and PR #58 rerun](2026-11-05-linux-adr-0004.md).
2. [Broader coverage and mirror findings](2026-11-05-broader-linux.md).
3. [Resolver, expiry, malformed GitHub and private keyring follow-up](2026-11-05-follow-up-linux.md).
4. [Final merged-fix rerun and remaining limits](2026-11-05-final-linux-rerun.md).

Portable assertion snapshots are in [evidence/](evidence/); detailed local
build logs and VM images are not checked in. Earlier failed assertions are
retained as historical evidence, rather than rewritten as passing results.
