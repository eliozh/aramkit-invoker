# ADR-0001: Discover the mock through a `LeagueClientUx`-named process

**Date**: 2026-11-10
**Status**: accepted
**Deciders**: architect (F5); pending G3

## Context

The app has no discovery override. `src/lcu/creds.rs` calls `sysinfo`, selects processes whose name
is `LeagueClientUx` or `LeagueClientUx.exe`, and reads `--app-port` and
`--remoting-auth-token` from their command line. Only if that fails does it fall back to a
`lockfile` or a `*_LeagueClientUx.log`. A mock that the unmodified app can find therefore has to
look like the real client on the process list and on the loopback socket.

Candidate C (an env-var or CLI override inside the app) was considered and deferred by the approved
spec: it changes production discovery and bypasses the security model the app uses for credentials.
Candidate B (lockfile/log) is Windows-only end to end because `process_image_path` is Windows-only.

## Decision

The runnable mock is an example target named exactly `LeagueClientUx`
(`examples/LeagueClientUx/main.rs`, explicit `[[example]] name = "LeagueClientUx"`). It is 14
characters, so Linux `comm` truncation at 15 characters cannot break matching, and Cargo produces
`LeagueClientUx.exe` on Windows.

In `--mode process` (the default) the process advertises `--app-port=<p> --remoting-auth-token=<t>`
in its own argv:

- If both flags with a non-zero port are present, the process serves and never re-execs.
- Otherwise it generates a token when missing, selects a loopback port when missing or zero, strips
  any existing `--app-port` / `--remoting-auth-token` arguments in both `--flag=value` and
  `--flag value` forms, re-execs `current_exe()` with the original arguments plus the definitive
  `--flag=value` pair, and waits for the child. The child sees both flags and serves. Exactly one
  runtime role (parent or child) listens.
- Port selection binds `127.0.0.1:0`, reads the assigned port, then drops the listener before
  re-exec; the child binds that port. A bind failure exits non-zero with a clear message. The small
  loopback TOCTOU race is accepted; there is no cross-platform way to pass a listening socket to a
  child without extra unsafe code.
- The `--flag=value` form is advertised because the app accepts both that and the space-separated
  form.
- The parent and child both match the process name. The app prefers the newest PID, so it selects
  the child; the parent has no credential flags and no listener, so it is never selected even if it
  is inspected.
- The parent propagates the child's exit status, so `cargo run --example LeagueClientUx` blocks for
  the server's lifetime.

In `--mode lockfile` and `--mode log` the process does **not** re-exec with credential flags.
It binds/generates the port and token internally, writes the discovery file first, and then serves,
so the app's Candidate A parse fails and discovery falls through to Candidate B. The file layout and
write-root rules are in ADR-0007. The default root is the mock's executable directory because the app
derives install roots from the process image path; the README therefore recommends copying the
example binary into a temporary directory for Candidate B, which makes the derived root temporary as
well.

## Alternatives Considered

### Alternative 1: Candidate C, an environment/CLI override inside the app
- **Pros**: No process impersonation; works while a real client runs; easy CI.
- **Cons**: Changes production code and the discovery security model; the approved spec defers it.
- **Why not**: The goal is to exercise the real discovery path without app changes.

### Alternative 2: A custom process name plus a lockfile only
- **Pros**: No argv advertising or re-exec.
- **Cons**: The app would not discover it on Linux; Candidate A would remain untested.
- **Why not**: Candidate A is the only zero-app-change path that exercises the production code.

### Alternative 3: Inherit the listening socket across re-exec
- **Pros**: No port race.
- **Cons**: Unix FD inheritance and Windows handle inheritance require platform-specific unsafe code
  and careful handle flags; disproportionate for a dev-only server.
- **Why not**: The race is bounded to loopback and a single developer machine.

## Consequences

### Positive
- The mock is found by the unmodified app on Linux (process name plus argv) and Windows (manual E2E).
- The real `parse_lcu_args` path, including the 300 ms port-connect check, is exercised.
- Candidate B modes cover the CN/WeGame lockfile and log fallbacks.

### Negative
- A process named `LeagueClientUx` is deliberate impersonation. Antivirus, EDR, or Riot Vanguard
  policy may flag it; the spec accepts this as a known test-only risk (Q1).
- The token is visible in the process command line by design. The real client behaves the same; the
  threat model in ADR-0007 documents the accepted residual risk.
- During startup there are briefly two matching processes. The newest-PID preference makes the
  choice deterministic.

### Risks
- If the example target is renamed, discovery breaks. The explicit `[[example]] name` and an
  automated test that parses the advertised argv guard this.
- Coexistence with a real client is unsupported because the app picks the newest PID. The README
  documents "close the real client"; no runtime detection is added.
