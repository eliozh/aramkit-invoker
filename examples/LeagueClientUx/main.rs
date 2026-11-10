//! Runnable, dev-only mock of the League client process (ADR-0001/0005).
//!
//! The example target is named exactly `LeagueClientUx` so the unmodified
//! app's process discovery finds it. This file is a thin orchestrator: the
//! pure CLI, schedule, and re-exec resolution logic lives in `mock_core.rs`,
//! which is also compiled by the in-crate `mock_lcu_tests`.

mod mock_core;

use std::env;
use std::process::{self, Command};
use std::thread;

use mock_core::{
    collect_utf8_args, resolve_launch, select_ephemeral_port, CliConfig, CliError, MockServer, Mode,
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

    match config.mode {
        Mode::Process => run_process(argv, config),
        // Slice 03 implements the lockfile/log writers. Until then the flags
        // are accepted and validated but no file is written.
        Mode::Lockfile | Mode::Log => Err(CliError::runtime(
            "--mode lockfile/log 将在切片 03 中实现".to_owned(),
        )),
    }
}

fn run_process(argv: Vec<String>, config: CliConfig) -> Result<(), CliError> {
    let launch = resolve_launch(
        &argv,
        config.app_port,
        config.remoting_auth_token.as_deref(),
        select_ephemeral_port,
        mock_core::generate_token,
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

    // The server thread serves until the process is stopped. Keep it alive and
    // in scope for the lifetime of the run.
    loop {
        thread::park();
    }
}
