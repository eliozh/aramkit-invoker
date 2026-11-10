//! Shared, self-contained mock of the League client (LCU) API.
//!
//! This file is compiled twice: as the core of the `LeagueClientUx` example
//! and as a `#[cfg(test)]` module of the library crate. It therefore uses only
//! the standard library, the five pinned dev-dependencies (including `libc`),
//! and `serde_json`. It must never reference `crate::`, and no release build
//! compiles it.
//!
//! The implementation is split into focused modules and re-exported here so
//! both consumers keep using one `mock_core` facade:
//!
//! - [`cli`]: argument parsing, validation, and process launch resolution.
//! - [`discovery`]: the scoped `lockfile`/client-log writers and path safety.
//! - [`schedule`]: the pure scenario timeline.
//! - [`server`]: the loopback TLS server, request parsing, and routing.
//! - [`fixtures`]: scenario state, options, and JSON fixture bodies.

#[path = "cli.rs"]
mod cli;
#[path = "discovery.rs"]
mod discovery;
#[path = "fixtures.rs"]
mod fixtures;
#[path = "schedule.rs"]
mod schedule;
#[path = "server.rs"]
mod server;

// The facade is compiled in two targets with different consumers (the example
// binary and the in-crate test module), so each build legitimately uses only
// a subset of these re-exports.
#[allow(unused_imports)]
pub use cli::{
    collect_utf8_args, is_valid_token, resolve_launch, select_ephemeral_port, CliConfig, CliError,
    LaunchResolution, Mode, EXIT_RUNTIME, EXIT_USAGE, REDACTED,
};
#[allow(unused_imports)]
pub use discovery::{write_lockfile, write_log, DiscoveryFile};
#[allow(unused_imports)]
pub use fixtures::{MockOptions, MockState};
#[allow(unused_imports)]
pub use schedule::{Scenario, Schedule, Step};
#[allow(unused_imports)]
pub use server::{
    generate_token, install_shutdown_handlers, run_until_stopped, shutdown_flag, MockServer,
};
