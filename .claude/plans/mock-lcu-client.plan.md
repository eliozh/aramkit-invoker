# Milestone 1 — mock-lcu-client

**Feature**: `mock-lcu-client`
**Milestone**: 1 (`state.milestone = 1`)
**Stage**: F5 plan
**Status**: Approved at G3 (2026-11-10) via Plan Canvas.
**Source spec**: `.claude/features/01-mock-lcu-client/spec.md` (approved at G1)
**Architecture**: `docs/adr/README.md` and ADRs 0001-0007 (proposed, pending G3)
**Baseline**: `.claude/ecc-workflow/handoffs/F0-baseline.md`
**Base commit**: `f50e68e8db044030620a4758f4aa5adc4fbbdad2`

## Goal

Build the dev-only mock LCU server so the unmodified app can complete
`process/argv discovery -> loopback HTTPS Basic auth -> four read-only endpoints -> ChampionTracker -> ARAMKit URL`
without League, a Windows account, a GUI, or elevation, while leaving the release binary and CI artifacts
unchanged. Success is the spec's M1-M4, not a percentage.

## Milestone summary

All slices carry `feature: mock-lcu-client`. One slice = one PR. Slice 01 is deliberately the first
end-to-end path; no slice may be reordered to put models or fixtures ahead of it.

| Slice | Title | Primary ACs | Security | UI | PR size |
|---|---|---|---|---|---|
| `01-mock-core-end-to-end` | Committed TLS fixture, in-process HTTPS core, four endpoints, first full path | AC-1 (parse), AC-4, AC-5, AC-6, AC-7, AC-8 (automated proxy), AC-10, AC-12, AC-13 (generation), AC-14 (state handle), AC-15 | yes | no | Large |
| `02-example-process-timeline` | `LeagueClientUx` example, re-exec discovery, CLI validation, scripted timeline | AC-1 (re-exec), AC-4 (`--reject-auth`), AC-5 (phase-only flag), AC-8 (manual + timeline), AC-9, AC-11, AC-13 (validation/redaction), AC-14 (static flags) | yes | no | Medium |
| `03-lockfile-log-docs` | Lockfile/log discovery modes, Windows E2E notes, developer README | AC-2, AC-3, AC-13 (path validation/no committed token), AC-10 (documented Windows check) | yes | no | Medium |

## Fixed inputs (G1-approved; do not reopen)

- **Committed test certificate.** Use the RSA 2048 self-signed DER certificate and PKCS#8 DER key
  generated once with OpenSSL and committed under `examples/LeagueClientUx/testdata/`. No runtime
  generation, no `rcgen`, no PEM parser (ADR-0003).
- **`LeagueClientUx` process name.** The runnable target is exactly `LeagueClientUx`
  (`LeagueClientUx.exe` on Windows), 14 characters, test-only, never packaged (ADR-0001).
- **No `/ecc:plan-prd`.** The approved spec plus this plan are the complete planning surface (Q10).

## Constraints

- The mock is developer/CI-only. It must never enter the release binary, installer, or GitHub Actions
  artifact set. Examples are not built by `cargo build --release`; server crates are dev-dependencies only.
- Production behavior is unchanged. The only `src/` production-adjacent change is the test-only
  `#[cfg(test)] mod mock_lcu_tests` seam in `src/lcu/mod.rs` (ADR-0002). Slice 03 adds two tests to the
  existing `#[cfg(test)] mod tests` in `src/lcu/creds.rs`; that is test-only, with no API or behavior change.
- HTTPS only; bind `127.0.0.1` only; no plain HTTP, no external network, no proxy, no redirects.
- Implement exactly four read-only endpoints plus the documented 401/404/405/400 responses. No control
  endpoint, no WebSocket, no Candidate C app override.
- Every CLI value is validated at the boundary and fails fast with a clear message. The token is random
  per run, redacted in output, never committed, and never logged unredacted.
- Offline dependency resolution only. The four exact dev-dependency pins and their cached fallbacks are
  in ADR-0004; never enable rustls default features (it would pull uncached `aws-lc-rs`).
- No UI. The mock is headless; the existing app window is only an observation surface for manual runs.
- Manual checks are Windows-only where the app is platform-bound (process image path, browser window).
  Everything else must run headless on Linux and `windows-latest`.

## Patterns to mirror

| Category | Source | Pattern |
|---|---|---|
| Naming | `src/lcu/creds.rs`, `src/lcu/client.rs` | Private modules; `snake_case` functions; `parse_*` / `fetch_*` verbs; test names state the behavior |
| Errors | `src/lcu/client.rs` | `anyhow`/`Context` with user-facing Chinese messages in the app; the mock uses explicit `Result` errors with one-line diagnostics and non-zero exits for usage errors |
| Auth | `src/lcu/client.rs:106-114` | Exactly `basic_auth("riot", Some(&token))`; the mock accepts only that header and returns 401 otherwise |
| Models | `src/lcu/model.rs`, `src/champion.rs` | camelCase JSON; numeric IDs; `ChampionCatalog::from_entries`; `champion_url_for_alias` |
| Tests | `src/lcu/creds.rs`, `src/lcu/model.rs` | In-crate `#[cfg(test)]` modules; `tempfile` for filesystem cases; descriptive behavior names; no sleeps |
| Release gating | `.github/workflows/build-windows.yml` | The workflow runs only `cargo test --all-targets --locked` and `cargo build --release --locked`; the mock must pass or be absent, never add a packaging step |

---

## Slice 01 — `01-mock-core-end-to-end`

- **feature**: `mock-lcu-client`
- **title**: Committed TLS fixture, in-process HTTPS core, four endpoints, and the first full path
- **ui**: no
- **security**: yes (loopback TLS, Basic auth, token generation/redaction, bounded responses)
- **ACs**: AC-1 (parse + TCP-connect portion), AC-4 (401/200 + `reqwest` 401 chain), AC-5, AC-6,
  AC-7, AC-8 (automated in-process proxy), AC-10, AC-12, AC-13 (token generation portion),
  AC-14 (state-handle portion), AC-15

### Files

| File | Action | Why |
|---|---|---|
| `Cargo.toml` | UPDATE | Add `[dev-dependencies]` exactly as ADR-0002/0004: `rustls = "=0.23.46"` with `default-features = false, features = ["ring", "std", "tls12"]`, `httparse = "=1.10.1"`, `base64 = "=0.22.1"`, `getrandom = "=0.3.4"` |
| `Cargo.lock` | UPDATE | Resolve the new dev-dependencies once with `cargo test --all-targets` (no `--locked`), review the added entries, commit the lockfile, then use `--locked` everywhere |
| `examples/LeagueClientUx/testdata/cert.der` | CREATE | Test-only self-signed RSA 2048 certificate; SAN `IP:127.0.0.1,DNS:localhost`; 10-year validity; generated with the ADR-0003 OpenSSL commands |
| `examples/LeagueClientUx/testdata/key.der` | CREATE | Test-only PKCS#8 DER private key; public by design; never installed into a trust store or referenced from production |
| `examples/LeagueClientUx/mock_core.rs` | CREATE | Self-contained core: `MockState`, `MockServer` on `127.0.0.1:0`, advertised argv, token generation, `Arc<RwLock<MockState>>` handle, rustls `StreamOwned` server, `httparse` request head, auth-before-routing, the four handlers, exact status/header rules, no 3xx, `Connection: close`, 8 KiB head cap, 5 s timeouts, constant-time Basic credential compare (local helper; no extra crate) |
| `src/lcu/mod.rs` | UPDATE | Append the test-only seam `#[cfg(test)] mod mock_lcu_tests { #[path = "../../examples/LeagueClientUx/mock_core.rs"] mod mock_core; ... }`. No public API; no release compilation |

### Tests

All tests live in `src/lcu/mod.rs::mock_lcu_tests` and use the real `parse_lcu_args`, `build_client`,
`fetch_*`, `ChampionTracker`, and `champion_url_for_alias`.

| Test | Asserts |
|---|---|
| `generated_token_uses_app_charset_and_is_not_empty` | 32 random bytes, base64 URL-safe without padding; 43 chars; charset subset of `[A-Za-z0-9_=-]` (AC-13) |
| `end_to_end_argv_auth_endpoints_and_aramkit_url` | Start the core; read its advertised `--app-port=<p> --remoting-auth-token=<t>` argv; `parse_lcu_args` returns `p`/`t`; a raw loopback TCP connect succeeds; `build_client` + Basic auth fetch `/lol-gameflow/v1/session`, `/lol-gameflow/v1/gameflow-phase`, `/lol-champ-select/v1/session`, and the champion summary; drive `ChampionTracker` through `Lobby -> ChampSelect -> GameStart -> InProgress`; assert exactly one trigger for champion 103 and the URL `https://aramkit.cn/zh-CN/champions/ahri` (AC-1, AC-8 automated proxy, AC-12) |
| `missing_wrong_and_rejected_credentials_are_401` | No `Authorization`, wrong password, wrong username, wrong scheme, and `reject_auth = true` all return 401 with `WWW-Authenticate` starting `Basic` and an empty body; correct `Basic base64("riot:<token>")` returns 200 (AC-4) |
| `fetch_gameflow_surfaces_reqwest_401` | `fetch_gameflow` with a bad token returns an error whose chain contains a `reqwest::Error` with status 401 (AC-4) |
| `gameflow_session_and_phase_only_shapes` | `/lol-gameflow/v1/session` deserializes into `GameflowSession` with the scenario `phase`, `game_id`, `queue().id`, `queue().game_mode`; phase-only mode returns 404 then the bare JSON string from `/gameflow-phase`, and `fetch_gameflow` yields that phase with default game data (AC-5) |
| `champ_select_shape_and_404_outside_champ_select` | `ChampSelectSession::local_champion_id()` returns the scenario champion via `myTeam`; the completed-pick fallback shape also resolves; outside `ChampSelect` the endpoint returns 404 and `fetch_champ_select` returns `None` (AC-6) |
| `champion_summary_shapes_and_url_slugs` | The summary maps Ahri (103), Kha'Zix (121), Wukong (62); URLs are `.../ahri`, `.../khazix`, `.../monkeyking`; an unknown configured champion is appended and resolves (AC-7) |
| `server_binds_127_0_0_1_only` | The listener local address is exactly `127.0.0.1`; no wildcard bind; a non-loopback destination does not accept (best-effort; skip if the host has no non-loopback address) (AC-10) |
| `no_redirects_and_champion_summary_is_bounded` | Every route/case returns 200/400/401/404/405 only; no 3xx and no `Location`; the champion summary is under 4 KiB, far below the app's 2 MB cap; every response has an exact `Content-Length` (AC-15) |
| `static_state_handle_pins_all_fields` | Setting `phase`, `queue_id`, `game_mode`, `champion_id`, and `local_cell_id` on the handle changes the endpoint bodies and the parsed champion/cell (AC-14, in-process portion) |

### DONE-WHEN

- `env -u DISPLAY cargo test --all-targets --locked` passes, including the end-to-end test and the
  explicit 401 case (M2, AC-12).
- The four endpoints and all error statuses match ADR-0006; the server binds `127.0.0.1` only.
- The test module is `#[cfg(test)]` only; `cargo build --release --locked` still succeeds and
  `cargo tree -e normal` contains none of `rustls`, `httparse`, `base64`, or `getrandom`.
- `cargo fmt --check`, `cargo clippy --all-targets --locked -- -D warnings`, and
  `cargo check --target x86_64-pc-windows-msvc --locked` pass.

---

## Slice 02 — `02-example-process-timeline`

- **feature**: `mock-lcu-client`
- **title**: Runnable `LeagueClientUx` example, re-exec discovery, CLI validation, and scripted timeline
- **ui**: no
- **security**: yes (input validation, token redaction, process impersonation documented, file-mode guard)
- **ACs**: AC-1 (re-exec and full discovery), AC-4 (`--reject-auth` wiring + manual app behavior),
  AC-5 (`--gameflow-phase-only` flag), AC-8 (timeline and manual Windows run), AC-9, AC-11,
  AC-13 (validation/redaction), AC-14 (static flags)

### Files

| File | Action | Why |
|---|---|---|
| `Cargo.toml` | UPDATE | Add the explicit target so the example name cannot drift: `[[example]] name = "LeagueClientUx" path = "examples/LeagueClientUx/main.rs"` |
| `examples/LeagueClientUx/main.rs` | CREATE | CLI orchestration: parse/validate flags; default `--mode process`; select/generate port and token; advertise exactly one `--app-port=<p> --remoting-auth-token=<t>` pair; strip stale flags in both `--flag=value` and `--flag value` forms; re-exec `current_exe()` with the definitive pair; the child detects the pair and serves; the parent waits and propagates the exit status; bind failure exits non-zero; redacted stdout summary; wire `--reject-auth`, `--gameflow-phase-only`, `--step-ms`, `--phase`, `--scenario`, `--queue-id`, `--game-mode`, `--champion-id`, `--local-cell-id`; reject `--install-root` in process mode |
| `examples/LeagueClientUx/mock_core.rs` | UPDATE | Add the pure `Schedule`/`Step` model, `Scenario` constructors, static one-step schedules, `CliConfig::parse_from`, validation helpers, token generation/redaction helpers, and server options for `reject_auth` / `gameflow_phase_only`; keep the core self-contained (no `crate::`) |
| `src/lcu/mod.rs` | UPDATE | Extend `mock_lcu_tests` with CLI, re-exec-resolution, timeline, static-state, and no-false-trigger tests (names below) |

### Tests

All tests remain in `src/lcu/mod.rs::mock_lcu_tests`; the example source is a thin orchestrator so the
pure parsing/schedule logic is reachable from the in-crate test module.

| Test | Asserts |
|---|---|
| `cli_rejects_unknown_flags_and_invalid_values` | Unknown flags exit 2; port `> 65535`/non-numeric fails (0 means ephemeral); empty or over-256-byte token fails; token outside `[A-Za-z0-9_=-]` fails; bad `--mode`, `--scenario`, `--phase`, `--game-mode`, or numeric IDs fail with a clear message (AC-13) |
| `phase_and_explicit_scenario_are_mutually_exclusive` | `--phase` plus an explicitly supplied `--scenario` is an error; `--phase` alone builds a one-step static schedule and does not apply the default `mayhem` scenario (AC-14) |
| `install_root_is_rejected_in_process_mode` | `--install-root` with `--mode process` exits 2 instead of being ignored (AC-13) |
| `token_redaction_hides_the_generated_token` | The stdout summary contains mode/scenario/port and `<redacted>`, never the token; the `Debug` representation is also redacted (AC-13) |
| `resolve_launch_advertises_exactly_one_flag_pair` | Missing flags produce re-exec arguments with exactly one definitive `--app-port=<p> --remoting-auth-token=<t>` pair; the pair is parseable by `parse_lcu_args` (AC-1) |
| `resolve_launch_strips_previous_flag_forms` | Existing `--flag=value` and `--flag value` forms for both flags are removed before re-exec so the child sees one authoritative pair (AC-1) |
| `resolve_launch_child_does_not_reexec` | Both flags present with a non-zero port means serve, no re-exec (AC-1) |
| `mayhem_timeline_state_at_boundaries` | With default `--step-ms 1000`: start is `Lobby` (never `InProgress`); 2 s is `ChampSelect`; 14 s is `GameStart`; 18 s and later are `InProgress`; `GameStart` dwell exceeds the app's 2 s grace plus one 700 ms poll (AC-8) |
| `failed_scenarios_end_in_no_trigger_phases` | `non-mayhem` ends `InProgress` with queue 450/ARAM; `dodge` ends `Lobby`; `failed-to-launch` ends `FailedToLaunch`; all are terminal for the tracker's trigger logic (AC-9) |
| `static_flags_pin_phase_queue_champion_and_cell` | `--phase ChampSelect --queue-id 2400 --game-mode KIWI --champion-id 103 --local-cell-id 2` produces endpoint responses with exactly those values (AC-14) |
| `mayhem_path_triggers_exactly_once_with_expected_url` | Driving the served state through the Mayhem sequence with the real client/tracker produces exactly one trigger and one ARAMKit URL (AC-8 automated proxy) |
| `non_mayhem_dodge_and_failed_to_launch_do_not_trigger` | The same sequence for `--queue-id 450 --game-mode ARAM`, `dodge`, and `failed-to-launch` produces no trigger and no URL (AC-9) |
| `phase_only_mode_returns_404_then_phase_without_trigger` | `--gameflow-phase-only` exercises the `/session` -> `/gameflow-phase` fallback and phase deserialization, but does not produce the full trigger because there is no queue data (AC-5; deliberate limitation) |

### DONE-WHEN

- `cargo run --example LeagueClientUx -- --phase ChampSelect --champion-id 103 --queue-id 2400 --game-mode KIWI`
  serves on loopback, and a separate process can parse its advertised argv with `parse_lcu_args`.
- Launching without flags re-execs once; the child serves and does not re-exec (manual process-level
  check on Linux and Windows, per Q7). The parent blocks for the server's lifetime.
- The scripted-timeline and no-false-trigger tests pass with no sleeps, and the default timeline never
  starts in `InProgress`.
- `cargo build --release --locked` still does not build the example:
  `test ! -e target/release/examples/LeagueClientUx*`, and `cargo tree -e normal` has no mock dependency.
- The existing workflow needs no new step: `cargo test --all-targets --locked` compiles the example on
  Linux and `windows-latest` (AC-11). The workflow file is unchanged.
- A manual Windows smoke run with the real app is recorded; the fully documented E2E steps land in slice 03.

---

## Slice 03 — `03-lockfile-log-docs`

- **feature**: `mock-lcu-client`
- **title**: Lockfile/log discovery modes, Windows E2E notes, and developer documentation
- **ui**: no
- **security**: yes (scoped file writes, path validation, token-in-file hygiene, cleanup)
- **ACs**: AC-2, AC-3, AC-13 (file/path validation and no committed token), AC-10 (documented Windows check)

### Files

| File | Action | Why |
|---|---|---|
| `examples/LeagueClientUx/mock_core.rs` | UPDATE | Add `--mode lockfile` and `--mode log` writers. Lockfile: `<root>/lockfile` as exactly `LeagueClientUx:<pid>:<port>:<token>:https`. Log: `<root>/Game/Logs/LeagueClient Logs/<timestamp>_LeagueClientUx.log`, with `--app-port=<p> --remoting-auth-token=<t>` in the first 64 KiB and a token matching `[A-Za-z0-9_=-]`. Validate the root: absolute only; reject relative, UNC, `..`, and symlink escapes; allowlist the two output shapes; refuse real-install markers (`LeagueClient.exe`, a `LeagueClientUx.exe` that is not the mock's own executable, and known Riot/Tencent/WeGame prefixes); Unix mode 0600; Windows default ACL; delete on clean exit |
| `examples/LeagueClientUx/main.rs` | UPDATE | Wire `--mode lockfile`/`--mode log`, `--install-root`, write-before-serve ordering, no credential flags in argv, and cleanup on exit |
| `src/lcu/creds.rs` | UPDATE (test-only) | Add two tests to the existing `#[cfg(test)] mod tests`: `parses_the_mock_lockfile_line` and `parses_the_mock_log_file_layout`. This is the only way to exercise the private log-directory parser against the mock's exact output without widening visibility; no production code, signature, or behavior changes |
| `src/lcu/mod.rs` | UPDATE | Extend `mock_lcu_tests` with writer, path-validation, and cleanup tests (names below) |
| `README.md` | UPDATE | Developer section: build/run commands; process, lockfile, and log modes; default scenario and `--step-ms`; `--reject-auth`; `--gameflow-phase-only`; static flags; Windows E2E steps and expected observation; "close the real client"; copy the example binary to a temporary directory for Candidate B; the certificate is a non-secret test fixture; the mock is never shipped |

### Tests

| Test | Location | Asserts |
|---|---|---|
| `lockfile_writer_emits_parseable_line` | `src/lcu/mod.rs::mock_lcu_tests` | The writer creates `<root>/lockfile` with exactly `LeagueClientUx:<pid>:<port>:<token>:https`; `parse_lockfile` returns the same port/token; Unix mode is 0600 (AC-2) |
| `log_writer_emits_expected_path_and_head` | `src/lcu/mod.rs::mock_lcu_tests` | The writer creates `<root>/Game/Logs/LeagueClient Logs/<timestamp>_LeagueClientUx.log`; the first 64 KiB contain both flags; the advertised token matches the app's log charset (AC-3) |
| `rejects_relative_unc_and_escape_roots` | `src/lcu/mod.rs::mock_lcu_tests` | Relative paths, UNC paths, `..`, symlink escapes, known install prefixes, and a real-install marker are rejected with a clear error; no file is written outside the chosen root (AC-13) |
| `clean_exit_removes_discovery_files` | `src/lcu/mod.rs::mock_lcu_tests` | A clean stop deletes the lockfile/log it created and leaves the temporary root usable (AC-13) |
| `parses_the_mock_lockfile_line` | `src/lcu/creds.rs::tests` | The app's `parse_lockfile` accepts the mock's exact line and token charset (AC-2) |
| `parses_the_mock_log_file_layout` | `src/lcu/creds.rs::tests` | The private `credentials_from_log_directory` accepts the mock's exact path/content, including NUL-tolerant text handling (AC-3) |

### DONE-WHEN

- The lockfile and log writer/parser round-trips pass on Linux; end-to-end discovery is documented as
  Windows-only because `process_image_path` is Windows-only (AC-2/AC-3).
- `--mode lockfile` and `--mode log` write only under the validated root, never advertise credential
  flags, and clean up on exit.
- `README.md` has copy-pasteable commands, the expected Windows observation, the "close the real client"
  note, the temporary-directory recommendation for Candidate B, and the test-certificate explanation.
- No generated token appears in committed files or test output; the fixture files are the only committed
  key material and are labelled non-secret.
- `cargo build --release --locked` and the GitHub Actions artifact set are still unchanged.

---

## Acceptance-criteria coverage

Every AC maps to at least one slice. "Automated" means part of the default `cargo test --all-targets`
set; "manual" means a documented Windows developer check.

| AC | Slice(s) | Automated evidence | Manual evidence |
|---|---|---|---|
| AC-1 | 01, 02 | Advertised argv parsed by `parse_lcu_args`; loopback TCP connect; re-exec argument resolution and idempotence | Windows run: app leaves "请启动英雄联盟客户端" and reports connected |
| AC-2 | 03 | Lockfile writer + `parse_lockfile` round-trip | Windows lockfile discovery through the app |
| AC-3 | 03 | Log writer + private log-directory parser test | Windows log discovery through the app |
| AC-4 | 01, 02 | 200/401 HTTP cases; `fetch_gameflow` error chain with `reqwest` 401; `--reject-auth` wiring | `--reject-auth` run shows "客户端凭证已失效，正在重新读取" and opens no page |
| AC-5 | 01, 02 | `/session` shape, `/gameflow-phase` bare string, phase-only fallback | None |
| AC-6 | 01 | `myTeam` and completed-pick fallback; 404 -> `None` mapping | None |
| AC-7 | 01 | Catalog mapping and `ahri`/`khazix`/`monkeyking` URLs; appended unknown champion | None |
| AC-8 | 01, 02 | In-process state sequence to the URL; timeline boundaries; exactly one trigger | Windows real-app run opens exactly one `.../ahri` page and shows name/status |
| AC-9 | 02 | Non-Mayhem, dodge, and failed-to-launch produce no trigger | Optional manual confirmation |
| AC-10 | 01, 03 | Bind address is exactly `127.0.0.1`; no outbound client | Optional Windows `Get-NetTCPConnection` check |
| AC-11 | 01, 02 | Release build has no example; `cargo tree -e normal` has no mock deps; workflow unchanged | CI artifact list inspection |
| AC-12 | 01 | `env -u DISPLAY cargo test --all-targets --locked` on Linux and `windows-latest`; no browser spawned | None |
| AC-13 | 01, 02, 03 | Token charset/size; CLI validation; redaction; file-path validation; no committed token | None |
| AC-14 | 01, 02 | State handle and static CLI flags pin phase/queue/champion/cell | Optional app status check |
| AC-15 | 01 | Status set has no 3xx/Location; champion summary under 4 KiB and under the 2 MB cap | None |

### Manual-only acceptance

No AC is entirely manual-only. The following portions are manual by design, and each has an automated
proxy elsewhere:

- **AC-1** full app discovery on Windows is manual because process-level discovery needs a real OS
  process list; the parser, TCP-connect, and re-exec-resolution behavior is automated.
- **AC-2 / AC-3** end-to-end lockfile/log discovery is Windows-only because
  `process_image_path` is `None` on non-Windows; the writer and parser are automated on Linux.
- **AC-4** observing `--reject-auth` in the real app is manual; the HTTP 401/200 and error-chain
  behavior is automated.
- **AC-8** the browser page and app status observation is manual; the state sequence, trigger count, and
  computed URL are automated in-process.
- **AC-10** the Windows `Get-NetTCPConnection` check is optional; the bind assertion is automated.

## Verification

Run from the repository root. The one-time dependency resolution step (no `--locked`) is required only
when the new dev-dependencies first enter `Cargo.lock`; review the lockfile diff and commit it before the
gate.

```bash
# One-time, first slice only: resolve and review the new dev-dependency entries.
cargo test --all-targets
git diff -- Cargo.lock

# Gate commands (all use the committed lockfile).
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --all-targets --locked
cargo build --release --locked
cargo check --target x86_64-pc-windows-msvc --locked

# Headless proof (AC-12): no display, no browser, no League, no elevation.
env -u DISPLAY cargo test --all-targets --locked

# Release isolation (AC-11).
test ! -e target/release/examples/LeagueClientUx
# Only rustls is slice-introduced; base64/httparse/getrandom are pre-existing
# normal transitive deps of reqwest/hyper/tempfile, so they are not a leak signal.
if [ -n "$(cargo tree -e normal -i rustls 2>/dev/null)" ]; then
  echo "mock dependency leaked into the normal dependency tree"; exit 1
fi
git diff --exit-code -- .github/workflows/build-windows.yml

# Token hygiene (AC-13): expect no generated token in the repository or test output.
git grep -nE 'remoting-auth-token=[A-Za-z0-9_=-]{20,}' -- ':!examples/LeagueClientUx/testdata' || true
cargo test --all-targets --locked 2>&1 | grep -E 'remoting-auth-token=[A-Za-z0-9_=-]{20,}' && exit 1 || true
```

Expected: all five gate commands pass; the headless run passes; the release isolation checks produce no
output; the workflow diff is empty; no token is found.

## Risks

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| Offline dependency resolution fails for one of the four dev-dependencies | Medium | Blocks slice 01 | Use the exact pins and the cached fallback decision tree below; never fetch, vendor, or switch to plain HTTP |
| rustls default features accidentally enabled, pulling uncached `aws-lc-rs` and failing on Windows | Medium | Blocks build | Keep `default-features = false, features = ["ring", "std", "tls12"]`; review the `Cargo.toml` diff and `Cargo.lock`; `ring` builds through the existing C toolchain |
| `mock_core.rs` compiles twice (example and test) and trips `clippy -D warnings` or dead-code checks | Medium | CI failure | Keep the core warning-clean; use a narrow `#[allow(dead_code)]` only where one consumer does not use a helper; run all-targets clippy on both targets |
| The `#[path]` test seam drifts or accidentally compiles in release | Low | Release contamination | One seam in `src/lcu/mod.rs` under `#[cfg(test)]`; release-isolation checks; review |
| Process-name impersonation is flagged by antivirus/EDR/Vanguard | Accepted | Developer inconvenience | G1 accepted this as test-only; document it; never package the mock |
| Re-exec port selection has a loopback TOCTOU race | Low | Bind failure on startup | ADR-0001 accepts the bounded race; fail with a clear non-zero exit; tests use a direct port |
| Candidate B discovery cannot be automated on Linux | Medium | Coverage gap | Parser/writer tests on Linux; documented Windows E2E; do not change `process_image_path` |
| Timing values too small for the manual Windows E2E | Low | Missed `ChampSelect` | Default `--step-ms 1000`; documented minimum; tests bypass time with the state handle |
| Committed test key triggers secret scanners | Low | Review noise | Label it a non-secret test fixture in the README and fixture note; never install it into a trust store; release isolation |
| Token leaks into stdout, test output, or a committed file | High impact, low likelihood | Credential hygiene failure | Redacted summary; no request-head logging; tests/greps above; token written only in explicit discovery modes |

### Offline dependency resolution decision tree

Follow ADR-0004 exactly. Apply only the first failing branch, re-run the resolution, and record the
chosen versions in the PR.

1. **Try the exact pins first.** `rustls =0.23.46` (`default-features = false`, `ring,std,tls12`),
   `httparse =1.10.1`, `base64 =0.22.1`, `getrandom =0.3.4`. Run `cargo test --all-targets` once
   without `--locked`, review the `Cargo.lock` diff, commit it, then use `--locked`.
2. **If rustls 0.23.46 cannot resolve**, fall back to the cached `rustls =0.23.37` with the identical
   feature set.
3. **If `getrandom` 0.3.4 cannot resolve**, fall back to cached `getrandom` 0.2.17
   (`getrandom::getrandom`) or 0.4.3 and adjust the single call site.
4. **If `base64` 0.22.1 cannot resolve**, fall back to cached `base64` 0.23.1; only the standard
   encode call is used.
5. **If `httparse` 1.10.1 is unavailable**, use the cached version only if it is the same API; otherwise
   use a minimal hand-written 8 KiB request-head parser with tests (ADR-0004 alternative 4). This is a
   last resort and must be called out in review.
6. **Never** enable rustls default features: 0.23.46 requires `aws-lc-rs >= 1.18`, only 1.15.3 is
   cached, and `aws-lc-sys` needs cmake/nasm on Windows.
7. **Never** add `rcgen`, `tiny_http`, `rustls-pemfile`, or `openssl-src`; they are not cached.
8. **If a required crate is absent from the local registry cache and no cached fallback works**, stop
   and write `Status: needs-human` with the exact failing resolution. Do not use the network, do not
   vendor a crate, and do not switch to plain HTTP (Q12).

## Acceptance checklist

- [ ] Slice 01 lands first and contains the full in-process path through the real client/tracker.
- [ ] Each slice is one PR with the `mock-lcu-client` feature tag and all listed tests.
- [ ] AC-1 through AC-15 are covered by at least one slice; manual-only portions are documented.
- [ ] `cargo fmt --check`, `cargo clippy --all-targets --locked -- -D warnings`,
      `cargo test --all-targets --locked`, `cargo build --release --locked`, and
      `cargo check --target x86_64-pc-windows-msvc --locked` pass.
- [ ] The headless test passes with `DISPLAY` unset; no browser, League, or elevation is required.
- [ ] The release binary, workflow, and artifact set are unchanged.
- [ ] No token or mock dependency appears in committed files or the release tree.
