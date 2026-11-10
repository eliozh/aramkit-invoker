# ADR-0002: Place the mock in an example target with a shared core and dev-dependencies

**Date**: 2026-11-10
**Status**: accepted
**Deciders**: architect (F5); pending G3

## Context

The mock must be runnable by a developer, exercised in-process by headless tests, and absent from
the release binary and GitHub Actions artifact. The existing `src/lcu/client.rs`, `creds.rs`, and
`model.rs` are private modules (`mod`, not `pub mod`) inside `src/lcu/`; an integration test under
`tests/` cannot reach them without adding a public seam.

Cargo does not build examples for `cargo build --release`, and dev-dependencies are not compiled for
normal builds. `cfg(test)` code is excluded from release builds. These properties give a layout that
satisfies the release constraint without feature flags or a workspace.

## Decision

Layout:

```text
examples/LeagueClientUx/
├── main.rs          CLI parsing, re-exec, lockfile/log setup, server start
├── mock_core.rs     TLS server, HTTP parser, routing, MockState/Schedule, auth, fixtures
└── testdata/
    ├── cert.der     test-only self-signed certificate (ADR-0003)
    └── key.der      test-only private key (ADR-0003)
```

`Cargo.toml` gets an explicit example target so the name cannot drift, plus dev-dependencies only:

```toml
[[example]]
name = "LeagueClientUx"
path = "examples/LeagueClientUx/main.rs"

[dev-dependencies]
rustls = { version = "=0.23.46", default-features = false, features = ["ring", "std", "tls12"] }
httparse = "=1.10.1"
base64 = "=0.22.1"
getrandom = "=0.3.4"
```

The exact pins are deliberate: the machine and CI must resolve the same cached versions offline.
They may be relaxed to caret requirements after `Cargo.lock` is committed and verified. The
implementer runs `cargo test --all-targets` once without `--locked` to resolve the new
dev-dependencies into `Cargo.lock`, reviews the added entries, commits the lockfile, and then
confirms both the locked test and release builds.

`mock_core.rs` is self-contained: it imports only the standard library, the four dev-dependencies,
and `serde_json` (an existing normal dependency). It must not use `crate::` because it is compiled
both as part of the example crate and as a module of the library crate.

In-process test seam: append one `#[cfg(test)]` module to `src/lcu/mod.rs` (or `client.rs`):

```rust
#[cfg(test)]
mod mock_lcu_tests {
    #[path = "../../examples/LeagueClientUx/mock_core.rs"]
    mod mock_core;

    // tests use crate::lcu::creds::parse_lcu_args, crate::lcu::client::*, and
    // crate::champion::champion_url_for_alias, then drive mock_core in-process.
}
```

This is the only change under `src/`. It is not compiled in release builds, adds no public API, and
keeps the private `lcu` modules encapsulated. `parse_lcu_args` and the `fetch_*` functions are
reachable from this module because it is inside the crate.

Tests use a manually controlled state handle rather than sleeps: start the server on
`127.0.0.1:0`, read the advertised argv from the core, parse it with `parse_lcu_args`, build the
app's client with `build_client()`, fetch each endpoint, drive `ChampionTracker` through
`Lobby -> ChampSelect -> GameStart -> InProgress`, and assert the ARAMKit URL. A second test sends a
wrong token and asserts the `reqwest::Error` chain contains status 401. No display, browser, League,
or elevation is needed.

`cargo test --all-targets --locked` compiles the example on both Linux and Windows, so example-only
compilation errors are caught in the existing CI job.

## Release isolation

- `cargo build --release --locked` builds the library and `src/main.rs` only. Examples are not built.
- Dev-dependencies are not resolved for a normal build and cannot enter the release binary.
- `#[cfg(test)]` is false in release, so the shared core is not compiled into the app.
- The GitHub Actions workflow uploads only `target/release/aramkit-invoker.exe`; it has no step that
  packages `examples/`.
- Verification: after a release build, assert `target/release/examples/` does not exist (or contains
  no `LeagueClientUx`), assert `cargo tree -e normal` contains none of rustls/httparse/base64/
  getrandom for the app, and confirm the artifact list is unchanged.

## Alternatives Considered

### Alternative 1: `[[bin]]` gated by `required-features = ["mock-lcu"]`
- **Pros**: A normal binary entry point.
- **Cons**: Requires a feature flag and `--features mock-lcu`; the default release build must stay
  free of the feature. More moving parts than an example.
- **Why not**: Kept only as the first fallback from spec D4 if the example layout proves awkward.

### Alternative 2: A workspace member
- **Pros**: Independent versioning and dependency graph.
- **Cons**: Restructures the repository, changes CI paths and lockfile ownership, and is
  disproportionate for a small dev-only server.
- **Why not**: The spec's last fallback; not needed for the MVP.

### Alternative 3: An integration test under `tests/`
- **Pros**: Standard integration-test location; naturally dev-only.
- **Cons**: Cannot access private `lcu::client`/`creds`/`model`; would require widening the public
  API, which the spec does not want.
- **Why not**: The in-crate `#[cfg(test)]` module preserves encapsulation and still exercises the
  real client path.

### Alternative 4: Put the core in `src/lcu/mock.rs`
- **Pros**: Simple module visibility.
- **Cons**: Mixes a dev-only server into the production source tree; the spec requires the shared
  core under `examples/`; still needs `cfg(test)` gating against release.
- **Why not**: The example layout keeps the surface clearly out of the product.

## Consequences

### Positive
- One core is exercised by both the runnable example and headless tests; no duplicated server logic.
- The release binary, installer, and artifact set are unchanged.
- No feature flags or workspace restructuring.

### Negative
- `mock_core.rs` is compiled twice (example and test), so it must stay warning-clean in both
  contexts. A narrow `#[allow(dead_code)]` on the module is acceptable if one consumer does not use
  a helper.
- The `#[path]` include is slightly unusual and must be documented for maintainers.
- The example is built by `cargo test --all-targets`, so its compilation time is added to CI.

### Risks
- A future refactor could reference the mock from non-test code. A review check and the `cfg(test)`
  boundary guard this; the release verification above detects accidental release inclusion.
- The four dev-dependencies are pinned offline. If the registry cache changes, the pins may need
  adjustment before `--locked` builds.
