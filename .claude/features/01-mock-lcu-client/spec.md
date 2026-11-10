# Feature Spec: Mock Chinese-Server League Client (LCU) for Testing

Status: approved at G1 (2026-11-10). Gate recorded in `.claude/ecc-workflow/state.json`.

Resolved at G1:

- Q1: accept the dev-only `LeagueClientUx` process name; document it as test-only and never package it.
- Q10: skip `/ecc:plan-prd`; this is a developer/test surface, so the spec plus the plan are enough.
- D2: use a committed test-only self-signed certificate, because the environment is offline and `rcgen` is unavailable.

## Problem

The app can only exercise a real LCU through a running Chinese-server League client. That requires a
Windows install and account, the client may run elevated, and it cannot run in CI. Existing tests
cover argument/lockfile/log parsing, the JSON models, the tracker state machine, and URL building,
but nothing exercises the real path end to end: process discovery -> HTTPS Basic auth ->
gameflow/champ-select/champion-summary -> trigger -> ARAMKit URL. Discovery and endpoint-shape
changes therefore cannot be verified on a machine without League or in CI.

Goal: a dev-only mock LCU server that the unmodified app discovers and talks to, so a developer can
run the real app and watch it open the ARAMKit page without League installed, and so CI can exercise
the client path headlessly.

The mock serves only the four read-only endpoints the app uses. It is not a League client
replacement.

### Assumptions not supported by the existing docs

- [Assumption A1] A dev-only process named `LeagueClientUx` will not be blocked by antivirus, Riot
  Vanguard, or repository policy in the developer environment. No doc supports this; see Q1.
- [Assumption A2] A small champion-summary subset is sufficient. The app only needs the picked
  champion's entry, but the README does not say the asset may be truncated.
- [Assumption A3] Candidate A discovery can run on Linux for automated tests because `sysinfo`
  exposes process name/argv there. The README only promises Linux for checks/unit tests.
- [Assumption A4] Candidate B (lockfile/log) is end-to-end discoverable only on Windows, because
  `process_image_path` in `src/lcu/creds.rs` is Windows-only. The README does not state this.
- [Assumption A5] The app's `danger_accept_invalid_certs(true)` accepts a runtime self-signed
  certificate on Windows SChannel and Linux OpenSSL. Only the client side is documented in code;
  the server side is untested.
- [Assumption A6] Timeline timings can satisfy the app's 3 s discovery loop, 700 ms poll, and 2 s
  `GameStart` grace. The constants exist in code; no doc states them as a mock requirement.
- [Assumption A7] `cargo build --release` does not build `examples/` targets, so the release output
  stays unchanged. This is Cargo behavior, not a repository doc.
- [Assumption A8] The mock and a real League client will not run at the same time. The app prefers
  the newest `LeagueClientUx` PID, so coexistence is not supported.
- [Assumption A9] The mock will re-exec itself (or be launched by a documented command) to put its
  port and token into its own command line. This is an implementation assumption.
- [Assumption A10] A gated/example test target can share server code with in-process tests without
  adding the mock to the release binary. The exact sharing mechanism is a planning decision.

### Design decisions (resolved by this spec)

**D1 - Discoverability: Candidate A primary, Candidate B included, Candidate C deferred.**
The mock executable/process is named exactly `LeagueClientUx` (Windows: `LeagueClientUx.exe`; Linux:
`LeagueClientUx`, 14 characters so Linux `comm` truncation cannot break matching). It advertises
`--app-port=<p> --remoting-auth-token=<t>` in its own argv. If launched without those flags, it
binds an ephemeral port, generates a token, re-execs itself with the flags, and the child serves
(the child detects the flags and does not re-exec again). Candidate B is also implemented as an
explicit mode that writes a `lockfile` or a `*_LeagueClientUx.log` under the mock's own executable
directory, which the app derives as an install root on Windows; it is Windows-only end to end.
Candidate C (an env-var/CLI override inside the app) is out of the MVP: it is a production change,
it bypasses the discovery security model, and it is not needed to meet the goal.
Justification: Candidate A is the only zero-app-change path that exercises the exact production
discovery code, and it works on Windows for manual runs and on Linux for headless tests.

**D2 - Transport: HTTPS with a committed test-only self-signed certificate.**
No plain HTTP. `LcuCredentials::base_url()` in `src/lcu/creds.rs` hardcodes `https://127.0.0.1:<p>`,
and `build_client()` in `src/lcu/client.rs` disables certificate verification for loopback. Plain
HTTP would require an app change and would not exercise the real TLS path. The certificate and key
are a throwaway localhost fixture generated once with `openssl` and committed under the mock's test
directory; they are used only by the mock, never by the app or a production path, and the server
binds `127.0.0.1` only. This replaces the original "runtime-generated" wording because the target
environment is offline and `rcgen` is not available.

**D3 - Scenario driving: static flags + scripted timeline; control endpoint deferred.**
The MVP supports (a) static state via CLI flags for inspection/debugging and (b) a deterministic
scripted timeline, default Mayhem: Lobby -> ChampSelect (champion assigned) -> GameStart ->
InProgress, with configurable dwell times and failure branches (non-Mayhem, dodge,
FailedToLaunch). Tests drive the mock core in-process directly, so no network control plane is
needed for deterministic automated tests. A loopback control endpoint is deferred (Q3).

**D4 - Location: `examples/LeagueClientUx/` example target; release build unchanged.**
The runnable entry point is an example target, invoked with
`cargo run --example LeagueClientUx -- ...`. The server core lives in a shared source file under
that directory and is included by the example and by an in-process `#[cfg(test)]` test module.
Server dependencies are dev-dependencies. `cargo build --release` must not build or package the
mock, so `target/release/aramkit-invoker.exe` and the GitHub Actions artifact set stay unchanged.
Fallbacks, in order, if the example layout proves awkward: a `[[bin]]` gated by
`required-features = ["mock-lcu"]`, then a workspace member. Do not ship the mock.

## Users

- Maintainers and contributors who must verify LCU discovery, parsing, and triggering without a
  League install.
- CI and release verifiers who need a headless, deterministic check of the client path.
- Developers on non-Windows machines who need to run the client logic locally.
- Not end users. No user-facing behavior is added.

## Hypothesis and measurable outcome

Hypothesis: if the mock advertises credentials exactly as the real client does and serves the four
endpoints in the shapes `src/lcu/model.rs` and `src/champion.rs` expect, then the unmodified app
completes discovery -> ChampSelect -> GameStart/InProgress -> ARAMKit page open without League, and
a headless test can assert the same path minus the browser.

Measurable outcomes (binary pass/fail; no invented percentages or dates):

- M1: On Windows with no League client/process, the documented mock command plus the real app opens
  the expected `https://aramkit.cn/zh-CN/champions/<slug>` exactly once for the configured champion,
  and the app shows connected status.
- M2: `cargo test --all-targets --locked` includes at least one test that starts the mock core
  in-process, exercises credential-input parsing, the four endpoint shapes, and the computed URL,
  and passes with no GUI/display and no League.
- M3: `cargo build --release --locked` still produces the unchanged `aramkit-invoker.exe` and does
  not build the mock; the release artifact set is unchanged.
- M4: The same automated test passes on Linux and on the existing `windows-latest` CI runner.

## Acceptance criteria

Each criterion states how it is verified. "Automated" means part of the default test target set;
"Manual" means a documented Windows developer check.

**AC-1 - Credential discovery, Candidate A (command line).**
Given the mock runs as process `LeagueClientUx` (Linux) or `LeagueClientUx.exe` (Windows) with
`--app-port=<p> --remoting-auth-token=<t>` in its argv and listening on `127.0.0.1:<p>`, the app's
`discover()` returns port `p` and token `t`, and `port_accepts_connections` succeeds.
Verification: automated unit test that parses the mock's advertised argv with `parse_lcu_args` and
asserts the served values plus a successful loopback TCP connect; Manual Windows run showing the
app leaves "请启动英雄联盟客户端" and reports connected. Launching without flags must re-exec with
them, and the child must not re-exec again. [Assumption A3, A9]

**AC-2 - Credential discovery, Candidate B lockfile.**
Given the mock runs in `--mode lockfile` from a Windows directory (its own executable directory)
with no credential flags, it writes `<exe-dir>\lockfile` as
`LeagueClientUx:<pid>:<port>:<token>:https` before or while listening. The app on Windows discovers
`p`/`t` through the lockfile strategy.
Verification: Windows manual/E2E, plus an automated parser test (`parse_lockfile`) using the mock's
exact line. Linux end-to-end discovery is not expected because `process_image_path` is not
implemented; only the parser test applies. [Assumption A4]

**AC-3 - Credential discovery, Candidate B log.**
Given `--mode log`, the mock writes
`<exe-dir>\Game\Logs\LeagueClient Logs\<timestamp>_LeagueClientUx.log` whose first 64 KiB contain
`--app-port=<p>` and `--remoting-auth-token=<t>`, with a token in the app's accepted log charset
(`[A-Za-z0-9_=-]`). The app on Windows discovers `p`/`t` through the log strategy.
Verification: Windows manual/E2E, plus an automated parser test using the mock's exact log content.
Linux parser test only. [Assumption A4]

**AC-4 - Authentication and 401.**
Missing `Authorization`, a wrong password, or a wrong username returns HTTP 401 with
`WWW-Authenticate: Basic`; correct `Basic base64("riot:<token>")` returns 200. With `--reject-auth`,
all endpoints return 401 even for the advertised token. The app's watcher treats a 401 as
`credentials_rejected` and does not open a page.
Verification: automated mock HTTP tests for 401/200; an automated unit test asserting
`fetch_gameflow` returns an error chain containing a `reqwest::Error` with status 401; Manual
`--reject-auth` run showing "客户端凭证已失效，正在重新读取" and no page.

**AC-5 - Gameflow shapes.**
`GET /lol-gameflow/v1/session` returns camelCase
`{"phase":"...","gameData":{"gameId":<n>,"queue":{"id":<n>,"gameMode":"..."}}}` and deserializes
into `GameflowSession` with the expected `phase`, `game_id`, `queue().id`, and `queue().game_mode`.
`GET /lol-gameflow/v1/gameflow-phase` returns a bare JSON string (for example `"ChampSelect"`),
used when `/session` returns 404 (a `--gameflow-phase-only` mode).
Verification: automated deserialization test against the mock's response; optional variant with
IDs serialized as strings to exercise the flexible deserializer.

**AC-6 - Champ-select shape.**
`GET /lol-champ-select/v1/session` returns
`{"localPlayerCellId":<n>,"myTeam":[{"cellId":<n>,"championId":<n>}],"actions":[[{"actorCellId":<n>,"championId":<n>,"completed":true,"type":"pick"}]]}`.
The local member has the scenario champion, and `ChampSelectSession::local_champion_id()` returns
it. Outside ChampSelect the endpoint returns 404, which the app maps to `None`.
Verification: automated test against the mock's response, covering both the `myTeam` path and the
completed-pick fallback.

**AC-7 - Champion-summary shape.**
`GET /lol-game-data/assets/v1/champion-summary.json` returns a JSON array of
`{"id":<n>,"name":"<localized>","alias":"<alias>"}` including the scenario champion(s).
`ChampionCatalog::from_entries` maps them, and `champion_url_for_alias` produces the expected
lowercase, punctuation-stripped slug. Minimum fixture: the default scenario champion plus one
punctuation alias, for example `{"id":103,"name":"Ahri","alias":"Ahri"}`, `{"id":121,"name":"Kha'Zix","alias":"Khazix"}`,
`{"id":62,"name":"Wukong","alias":"MonkeyKing"}`.
Verification: automated test asserting the catalog and URL for each fixture entry. [Assumption A2]

**AC-8 - Full end-to-end trigger (Manual, Windows).**
With the real app already running and the mock launched with the default
`--mode process --scenario mayhem --champion-id 103 --queue-id 2400 --game-mode KIWI`, the timeline
holds ChampSelect long enough for the app to record the champion, then transitions to `GameStart`
(held at least 2 s) or `InProgress`. The app opens exactly one
`https://aramkit.cn/zh-CN/champions/ahri` page and shows the champion name/status.
The timeline must not start in `InProgress`; it starts in `Lobby` or `ChampSelect` so the app can
observe the pending pick.
Verification: Manual Windows run with the documented command and expected observation. Automated
proxy: the in-process test drives the same state sequence through the client/parse layer and
asserts the computed URL, without opening a browser.

**AC-9 - No false trigger.**
With a non-Mayhem queue (for example `--queue-id 450 --game-mode ARAM`, or
`--queue-id 0 --game-mode ARAM`), the same timeline completes and the app does not open a page.
With `--scenario failed-to-launch` (ChampSelect -> FailedToLaunch), the app does not open a page.
Verification: automated mock-state/queue assertions plus the existing tracker tests; Manual check
optional.

**AC-10 - Loopback only, no external network.**
The mock binds only `127.0.0.1`, does not accept connections on LAN interfaces, and never contacts
external hosts.
Verification: automated bind/connection test on Linux (`127.0.0.1` succeeds; a non-loopback address
is not bound) and/or a documented Windows `Get-NetTCPConnection` check.

**AC-11 - Release build unchanged.**
`cargo build --release --locked` still produces the unchanged `aramkit-invoker.exe`, does not build
`LeagueClientUx`, and leaves the release artifact set and GitHub Actions packaging unchanged.
Verification: inspect `target/release` after the build and the CI artifact list. [Assumption A7]

**AC-12 - Headless automated test.**
At least one test starts the mock core in-process, uses the mock's advertised credentials, calls the
app's discovery/request/parse path, and asserts the computed ARAMKit URL and at least one 401 case.
It runs with no display server, no browser, no League, and no privilege elevation.
Verification: run `cargo test --all-targets --locked` with `DISPLAY` unset on Linux and on
`windows-latest`; the test must not spawn a browser or Edge. [Assumption A5, A10]

**AC-13 - Token hygiene and input validation.**
The mock generates a random token per run from the app's accepted charset, never writes it into the
repository, and does not log it unredacted. CLI values are validated: port in `1..=65535` (0 is
allowed only to request an ephemeral port); token non-empty and charset-compatible when written to
argv/log; phase/queue/champion/cell IDs numeric; `--mode` and `--scenario` in the allowed set.
Invalid input exits non-zero with a clear message.
Verification: automated unit tests for validation and token generation; grep the test output and
fixtures for the token; confirm no token in committed files.

**AC-14 - Static state flags.**
`--phase <Phase> --queue-id <n> --game-mode <mode> --champion-id <n> --local-cell-id <n>` pins the
mock's responses; the app's status reflects the pinned phase/queue/champion. This mode is for
inspection, not for the trigger (there is no transition).
Verification: automated endpoint assertions per flag combination; Manual app check optional.

**AC-15 - No redirects and bounded responses.**
The mock returns 200/401/404 directly and never a 3xx. The champion-summary response is well under
the app's 2 MB limit.
Verification: automated status assertions and a response-size assertion.

## Out of scope

- Emulating the real client UI, Riot Client, WeGame/TCLS, login, anti-cheat, WebSocket events, or
  any endpoint beyond the four listed.
- Changing production discovery, the HTTPS scheme, the trigger state machine, queue constants, or
  the ARAMKit URL.
- Candidate C (an env-var/CLI override inside the app) in the MVP.
- Full champion-summary fidelity or live game data.
- Cross-platform browser-window E2E (Windows desktop only), certificate trust installation, or
  certificate pinning.
- Distributing or bundling the mock to end users; any release artifact.
- Multi-instance orchestration, coexistence with a real client, or load/performance testing.
- Any UI for controlling the mock; the control HTTP endpoint is deferred.

## Affected modules and screens

New (dev/test-only):

- `examples/LeagueClientUx/`: CLI, timeline/state machine, TLS loopback server, the four endpoint
  handlers, lockfile/log writers, and champion fixtures.
- One shared mock-core source file under that directory, included by the example and by an
  in-process `#[cfg(test)]` test module; the core must not be duplicated.
- Tests: in-crate unit tests in `src/lcu/` (or a `#[cfg(test)]` module) using the mock core;
  optional `tests/` cases only if a public seam is added.
- `Cargo.toml`: `[[example]]` entry and dev-dependencies, if the chosen layout needs them;
  `Cargo.lock` updates.
- README: a short developer section with the mock commands and the "close the real client" note
  (documentation only; no user-facing behavior change).

Unchanged:

- `src/lcu/creds.rs`, `src/lcu/client.rs`, `src/lcu/model.rs`, `src/lcu/mod.rs`, `src/champion.rs`,
  `src/tracker.rs`, `src/config.rs`, `src/app.rs`, `src/platform.rs`, `src/main.rs`.
- The config schema (`%APPDATA%\aramkit-invoker\config.json`), the release binary, and the GitHub
  Actions release packaging.
- All existing UI screens.

Screens: none. The existing main window's status/log area is used only as an observation surface
for manual acceptance; no UI is added or changed.

## UI yes/no

No. The mock is a headless CLI/server with no UI. The existing app UI is unchanged.

## Security triggers

- **Authentication: Yes.** HTTP Basic `riot:<token>`; 401 for missing/wrong credentials;
  `--reject-auth` mode. No real credentials are used; tokens are random per run.
- **User input: Yes.** CLI flags (`--mode`, `--scenario`, `--phase`, `--queue-id`, `--game-mode`,
  `--champion-id`, `--local-cell-id`, `--step-ms`, `--install-root`, `--reject-auth`) must be
  validated at the boundary and fail fast with clear errors.
- **Queries: No.** No database or SQL.
- **File paths: Yes (optional).** `--mode lockfile|log` writes only under the mock's own executable
  directory (the root the app derives) or an explicitly provided directory. Reject relative and UNC
  paths, symlink escapes, and writes outside the chosen root; prefer a temp directory. Candidate B
  must never write into a real League install.
- **External APIs: No.** The mock never contacts external hosts and binds loopback only. The app's
  existing outbound behavior (opening the ARAMKit page in a browser) is unchanged. No proxy, no
  redirects.
- **Crypto: Yes.** A throwaway test-only self-signed certificate/key for the loopback TLS server,
  generated once with `openssl` and committed under the mock's test directory. It is used only by
  the mock, never by the app, and no production secret is reused. The app already disables
  verification for loopback only.
- **Payments: No.**
- **Secrets: Yes.** The remoting token must be random per run, valid for the app's parser charset
  when advertised via argv/log, never committed, and never logged unredacted. The token is
  inherently visible in the process list/argv by design (the same is true of the real client);
  document this. Mirror the app's redaction discipline.
- **Additional.** Process-name impersonation (`LeagueClientUx`) is a deliberate test-only behavior.
  The mock must not be shipped in release artifacts, must not inject into or modify the real
  client/game, and must serve only the four read-only endpoints. Antivirus/Vanguard false positives
  are a known risk (Q1).

## Data or migration changes

None. There is no app config schema change, no database, and no migration. The mock uses static
fixtures and temporary lockfile/log files that are deleted on exit or live under a temp/derived
directory. `Cargo.lock` changes because of dev-dependencies; that is a lockfile update, not a data
migration. No user data is read or written.

## New dependencies

Dev-only capabilities are required:

- A loopback TLS HTTP server. The existing `reqwest` dependency is client-only and cannot serve
  TLS.
- A test-only self-signed certificate/key fixture (generated once with `openssl`, committed). No
  runtime certificate-generation crate is available offline.
- Optionally, OS-RNG-backed token generation (may be available without a new crate, depending on
  the approach).
- Optionally, JSON fixture serialization; `serde_json` is already a normal dependency and can be
  reused.

Constraints:

- Dependencies must be dev-dependencies, or otherwise excluded from the release
  `aramkit-invoker.exe`.
- They must build on Linux and `windows-latest` without extra system packages where possible;
  native OpenSSL on Windows is a portability risk.
- Exact crates and versions are a planning decision, not fixed here.
- `Cargo.lock` will change; the CI `--locked` commands must use the updated lockfile.

## Open questions

- **Q1 (process impersonation).** Is a dev-only executable/process named `LeagueClientUx`
  acceptable given antivirus, Riot Vanguard, or repository policy? Recommendation: yes for a
  non-distributed example; document it as test-only and never package it. If rejected, fall back to
  Candidate C with a security review.
- **Q2 (Candidate B scope).** Include lockfile/log modes in the MVP or defer them? Recommendation:
  include them, because they cover the CN/WeGame discovery fallbacks and are small file writes;
  mark them Windows-only end to end. Defer to a follow-up slice if scope tightens.
- **Q3 (control endpoint).** Implement `POST /mock/state` for external test control? Recommendation:
  no for the MVP; the in-process core gives deterministic tests. Add it only if an external harness
  needs it, and then bind loopback and require the token.
- **Q4 (champion data).** Subset fixture or full asset? Recommendation: a subset with the scenario
  champions and punctuation aliases; the app only needs the picked champion. Document the mock as
  data-incomplete.
- **Q5 (real-client coexistence).** Must the mock work while the real client runs? Recommendation:
  no; the app prefers the newest `LeagueClientUx` PID, so document "close the real client".
  Candidate C would be needed for coexistence.
- **Q6 (mock location plumbing).** Is the shared-source/example approach acceptable, or should this
  be a workspace member? Recommendation: example plus shared core for the MVP; move to a workspace
  member only if the mock grows or needs independent versioning.
- **Q7 (test-level discovery for Candidate A).** Do we need a process-level automated test for
  Candidate A, or is parser-level automation plus a Manual Windows E2E enough? Recommendation:
  parser-level automated plus Manual Windows E2E; add process-level automation only if a stable
  example-binary locator is available.
- **Q8 (timeline defaults).** Are the default dwell times (ChampSelect at least 5 s, GameStart at
  least 3 s, InProgress at least 10 s) acceptable? Recommendation: yes; make them configurable with
  `--step-ms` and document them.
- **Q9 (token visibility).** Should the mock print the token? Recommendation: no; print a redacted
  summary and the launch command without the token, since the app reads it from argv. Add an
  explicit `--show-token` only if debugging requires it.
- **Q10 (new-surface process).** This feature creates a new developer/test surface, not a
  user-facing product surface. Recommendation: run `/ecc:plan-prd` before implementation planning to
  decide whether the repo should formalize that surface and its ownership. Do not run it in F1.
- **Q11 (CI).** Does the existing `windows-latest` workflow need a new step for the mock tests?
  Recommendation: no, if the tests are in the default `cargo test --all-targets` set and the example
  compiles there; otherwise add one documented step.
- **Q12 (plain-HTTP fallback).** If the TLS server dependency proves problematic, is plain HTTP
  acceptable? Recommendation: no; that requires a production change and loses the real-path
  coverage. Keep HTTPS.
