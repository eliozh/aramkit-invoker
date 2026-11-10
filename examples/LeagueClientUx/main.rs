//! Runnable, dev-only mock of the League client process (ADR-0001/0005).
//!
//! The example target is named exactly `LeagueClientUx` so the unmodified
//! app's process discovery finds it. This file is a thin orchestrator: the
//! pure CLI, schedule, server, and discovery-file logic lives in `mock_core`,
//! which is also compiled by the in-crate `mock_lcu_tests`.

mod mock_core;

use std::env;
use std::path::{Path, PathBuf};
use std::process::{self, Command};

use mock_core::{
    collect_utf8_args, generate_token, install_shutdown_handlers, resolve_launch,
    run_until_stopped, select_ephemeral_port, shutdown_flag, write_lockfile, write_log, CliConfig,
    CliError, MockServer, Mode,
};

fn main() {
    // `env::args()` panics on non-UTF-8 argv; `collect_utf8_args` turns that
    // into a clear usage error instead.
    let error = match collect_utf8_args(env::args_os()).and_then(run) {
        Ok(()) => return,
        Err(error) => error,
    };
    eprintln!("{error}");
    process::exit(error.exit_code());
}

fn run(argv: Vec<String>) -> Result<(), CliError> {
    let config = CliConfig::parse_from(&argv)?;

    // SIGINT (and SIGTERM/SIGHUP/SIGQUIT on Unix) set the process-wide flag;
    // both serve paths poll it and run their cleanup instead of dying without
    // unwinding. The re-exec parent also ignores it: it keeps waiting for the
    // child, which cleans up.
    install_shutdown_handlers()?;

    match config.mode {
        Mode::Process => run_process(argv, config),
        Mode::Lockfile => run_discovery(config, DiscoveryMode::Lockfile),
        Mode::Log => run_discovery(config, DiscoveryMode::Log),
    }
}

/// The two discovery-file shapes `run_discovery` can serve. Passing this
/// instead of the full [`Mode`] makes the process mode unrepresentable.
#[derive(Clone, Copy)]
enum DiscoveryMode {
    Lockfile,
    Log,
}

fn run_process(argv: Vec<String>, config: CliConfig) -> Result<(), CliError> {
    let launch = resolve_launch(
        &argv,
        config.app_port,
        config.remoting_auth_token.as_deref(),
        select_ephemeral_port,
        generate_token,
    )
    .map_err(|error| CliError::runtime(format!("无法解析启动参数：{error}")))?;

    if launch.reexec {
        return reexec(&launch.args);
    }

    serve(config, launch.port, launch.token)
}

/// Re-executes `current_exe()` with the definitive credential pair and blocks
/// until the child exits, propagating its status (ADR-0001).
fn reexec(args: &[String]) -> Result<(), CliError> {
    let executable = env::current_exe()
        .map_err(|error| CliError::runtime(format!("无法定位当前可执行文件：{error}")))?;
    let status = Command::new(executable)
        .args(args.get(1..).unwrap_or(&[]))
        .status()
        .map_err(|error| CliError::runtime(format!("无法重新启动 mock：{error}")))?;
    process::exit(status.code().unwrap_or(1));
}

/// Serves in `--mode lockfile` or `--mode log`.
///
/// The discovery file is written before the server starts. If the bind fails,
/// the error return drops the `DiscoveryFile`, which removes the file again.
/// These modes never re-exec and never advertise credential flags in argv; the
/// app is expected to fall through to Candidate B discovery (ADR-0001).
fn run_discovery(config: CliConfig, mode: DiscoveryMode) -> Result<(), CliError> {
    let port = match config.app_port.filter(|port| *port > 0) {
        Some(port) => port,
        None => select_ephemeral_port()
            .map_err(|error| CliError::runtime(format!("无法选择回环端口：{error}")))?,
    };
    let token = match config.remoting_auth_token.clone() {
        Some(token) => token,
        None => {
            generate_token().map_err(|error| CliError::runtime(format!("无法生成令牌：{error}")))?
        }
    };
    let root = match &config.install_root {
        Some(root) => root.clone(),
        None => default_install_root()?,
    };

    // Validation happens inside the writer, before any filesystem mutation.
    let discovery = match mode {
        DiscoveryMode::Lockfile => write_lockfile(&root, port, &token)?,
        DiscoveryMode::Log => write_log(&root, port, &token)?,
    };

    let server = MockServer::start_on(
        port,
        Some(token),
        config.mock_options(),
        Some(config.schedule()),
    )
    .map_err(|error| CliError::runtime(format!("无法在 127.0.0.1:{port} 上启动 mock：{error}")))?;

    // The summary, the discovery path, and the token-free status line. The
    // token itself is never printed.
    println!("{}", config.summary(server.local_addr().port()));
    println!("发现文件：{}", discovery.path().display());

    // On SIGINT/SIGTERM stop the server thread first, then drop the
    // `DiscoveryFile`, which removes it. Returning exits 0.
    run_until_stopped(shutdown_flag(), || {
        drop(server);
        drop(discovery);
    });
    Ok(())
}

/// The default discovery root is the mock executable's directory, which the
/// app derives from the process image path (ADR-0001).
fn default_install_root() -> Result<PathBuf, CliError> {
    let executable = env::current_exe()
        .map_err(|error| CliError::runtime(format!("无法定位 mock 可执行文件：{error}")))?;
    executable
        .parent()
        .map(Path::to_path_buf)
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| CliError::runtime("无法确定 mock 可执行文件所在目录"))
}

fn serve(config: CliConfig, port: u16, token: String) -> Result<(), CliError> {
    let server = MockServer::start_on(
        port,
        Some(token),
        config.mock_options(),
        Some(config.schedule()),
    )
    .map_err(|error| CliError::runtime(format!("无法在 127.0.0.1:{port} 上启动 mock：{error}")))?;

    // The child owns the advertised pair in its argv; this summary never
    // prints the token.
    println!("{}", config.summary(server.local_addr().port()));

    // Keep the server alive until SIGINT/SIGTERM, then stop it. Returning
    // exits 0.
    run_until_stopped(shutdown_flag(), || drop(server));
    Ok(())
}
