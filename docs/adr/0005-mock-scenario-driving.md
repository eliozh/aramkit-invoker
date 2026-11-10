# ADR-0005: Drive the mock with static flags and a deterministic timeline

**Date**: 2026-11-10
**Status**: accepted
**Deciders**: architect (F5); pending G3

## Context

The app discovers every 3 s, polls every 700 ms, and waits 2 s in `GameStart` before firing. A mock
must hold `ChampSelect` long enough for the app to record the champion, then move to `GameStart` or
`InProgress`. Automated tests must be deterministic and must not depend on wall-clock timing.

The spec resolves the control question (D3): static CLI state plus a scripted timeline; a loopback
control endpoint is deferred. Tests drive the core in-process.

## Decision

Represent the mock state as data and the timeline as a pure schedule over elapsed time. The process
name, port, and token are discovery concerns (ADR-0001); this ADR covers phases and endpoint state.

```rust
pub struct MockState {
    pub phase: String,       // Lobby, ChampSelect, GameStart, InProgress, FailedToLaunch, ...
    pub game_id: i64,        // constant 424242 for every scenario
    pub queue_id: i64,       // 2400 (Mayhem) by default
    pub game_mode: String,   // "KIWI" by default
    pub champion_id: i64,    // 103 (Ahri) by default
    pub local_cell_id: i64,  // 1 by default
}

pub struct Step {
    pub state: MockState,
    pub dwell_ms: u64,
}

pub struct Schedule {
    pub steps: Vec<Step>,
}
```

`Schedule::state_at(elapsed)` walks the steps by cumulative dwell and returns the matching state. The
final step has no successor and holds indefinitely. The server evaluates the schedule on each
request from the server's start instant; there is no timer thread and no background mutation.

### Static mode

`--phase <Phase>` builds a one-step schedule that holds forever. `--queue-id`, `--game-mode`,
`--champion-id`, and `--local-cell-id` pin the rest of the state. `--phase` and `--scenario` are
mutually exclusive; supplying both is a validation error. Static mode is for inspection (AC-14), not
for the trigger.

Allowed phase values:

```text
None, Lobby, Matchmaking, ReadyCheck, ChampSelect, GameStart, InProgress,
EndOfGame, WaitingForStats, PreEndOfGame, FailedToLaunch, TerminatedInError
```

### Scenario timelines

Dwell times are multipliers of `--step-ms` (default 1000 ms). `--step-ms` accepts `1..=600000`.
Tests bypass timing entirely (see below).

| Scenario | Timeline (dwell multiplier) | Queue / mode |
|----------|-----------------------------|--------------|
| `mayhem` (default) | Lobby (2) -> ChampSelect (12) -> GameStart (4) -> InProgress (hold) | 2400 / KIWI |
| `non-mayhem` | same timeline | 450 / ARAM |
| `dodge` | Lobby (2) -> ChampSelect (12) -> Lobby (hold) | 2400 / KIWI |
| `failed-to-launch` | Lobby (2) -> ChampSelect (12) -> FailedToLaunch (hold) | 2400 / KIWI |

The default is chosen so the app has time to finish discovery (3 s loop) and observe the champion
before `GameStart`: `ChampSelect` lasts 12 s from server start. `GameStart` lasts 4 s, which exceeds
the app's 2 s grace plus a 700 ms poll. The timeline starts in `Lobby`, never in `InProgress`, so the
app can observe the pending pick (AC-8).

`--queue-id` and `--game-mode` override the scenario defaults, so `--scenario mayhem --queue-id 450
--game-mode ARAM` exercises the no-trigger path with the full timeline (AC-9). `--champion-id` and
`--local-cell-id` override those fields. A champion id that is not in the fixture is appended to the
champion-summary response (ADR-0006) so the end-to-end URL can still be computed.

### In-process test control

`MockServer` owns an `Arc<RwLock<MockState>>`. When the core is built without a schedule, the state
is static and the embedding test sets it explicitly between requests:

```text
Lobby -> ChampSelect -> GameStart -> InProgress
```

The test feeds the real `ChampionTracker` and client functions, asserts the ARAMKit URL once, and
replays the same sequence for `non-mayhem`, `dodge`, and `failed-to-launch` to assert no trigger.
No sleeps and no wall-clock dependence. The CLI uses a schedule and exposes the same state handle
internally, so the HTTP code path is identical.

## CLI surface and validation

The complete flag set is fixed; unknown flags exit 2.

| Flag | Type / values | Default | Validation |
|------|---------------|---------|------------|
| `--app-port` | `u16`, `0..=65535` | select an ephemeral port | `0` means "select an ephemeral port"; resolved before serving/re-exec |
| `--remoting-auth-token` | string | generate per run | non-empty, at most 256 bytes, charset `[A-Za-z0-9_=-]` |
| `--mode` | `process`, `lockfile`, `log` | `process` | exactly one of the three |
| `--scenario` | `mayhem`, `non-mayhem`, `dodge`, `failed-to-launch` | `mayhem` | mutually exclusive with `--phase` |
| `--phase` | phase name | none (timeline mode) | in the allowed phase set; mutually exclusive with `--scenario` |
| `--queue-id` | `i64` | scenario default (2400 or 450) | `>= 0` |
| `--game-mode` | string | scenario default (`KIWI` or `ARAM`) | non-empty, at most 32 chars, ASCII alphanumeric/`_`/`-` |
| `--champion-id` | `i64` | 103 | `> 0` |
| `--local-cell-id` | `i64` | 1 | `>= 0` |
| `--step-ms` | `u64` | 1000 | `1..=600000` |
| `--install-root` | absolute path | executable directory | only valid with `--mode lockfile` or `--mode log`; validated per ADR-0007 |
| `--reject-auth` | flag | off | always returns 401 |
| `--gameflow-phase-only` | flag | off | `/lol-gameflow/v1/session` returns 404 |

Behavioral rules:

- `--mode lockfile` / `--mode log` are allowed on Linux for writer tests, but end-to-end discovery
  is Windows-only because `process_image_path` is Windows-only.
- `--install-root` in `process` mode exits 2 rather than being silently ignored.
- `--phase` with an explicitly supplied `--scenario` exits 2. The default `--scenario mayhem` is not
  applied when `--phase` is present; phase mode is a static one-step schedule.
- Runtime errors (bind failure, file write failure) exit 1 with a clear message. Usage errors exit 2.
- Generated tokens are 32 random bytes encoded with base64 URL-safe without padding (43 characters,
  a subset of the app's charset). A token is generated per run and never logged unredacted.
- The CLI prints the effective mode, scenario/phase, port, and a redacted token summary. It never
  prints the token; the app reads it from argv or the discovery file.

## Alternatives Considered

### Alternative 1: Time-based transitions only
- **Pros**: One mechanism for CLI and tests.
- **Cons**: Tests must sleep, are slower, and can flake on loaded CI runners.
- **Why not**: A pure schedule plus an explicit state handle is deterministic.

### Alternative 2: A loopback control endpoint (`POST /mock/state`)
- **Pros**: An external harness could drive the mock.
- **Cons**: Extra attack surface, extra auth path, and no need because tests are in-process; the spec
  defers it (Q3).
- **Why not**: Out of scope for the MVP.

### Alternative 3: Start the timeline in `InProgress`
- **Pros**: The trigger would fire immediately.
- **Cons**: The app would never observe `ChampSelect`, so no champion would be recorded and no page
  would open (AC-8).
- **Why not**: The sequence must be realistic.

### Alternative 4: Fixed hard-coded timings
- **Pros**: Simplest.
- **Cons**: The developer cannot shorten a manual run, and CI cannot tune the proxy path.
- **Why not**: `--step-ms` and flags are cheap and explicitly requested (Q8).

## Consequences

### Positive
- Deterministic automated tests with no sleeps.
- Manual runs have a realistic sequence with enough dwell time for the app.
- Failure branches (`non-mayhem`, `dodge`, `failed-to-launch`) share one schedule model.

### Negative
- The schedule is evaluated per request, so phase changes are only observable when a request
  arrives. The app polls every 700 ms, so this is sufficient; a request-free interval simply skips
  intermediate observations.
- The state is intentionally mutable behind a lock. The lock is small and never held across I/O.

### Risks
- A `--step-ms` value that is too small can make a manual run miss `ChampSelect`; the default is
  safe, and the README documents the minimum useful values.
- `GameStart` must remain at least 2 s plus one poll for the grace path; `--step-ms` values below
  about 250 with the default multiplier undermine the manual E2E. This is documented, not enforced,
  because tests bypass the schedule.
- Scenario and override flags can express contradictory states. Validation accepts overrides by
  design (for AC-9) but rejects `--phase` with `--scenario`.
