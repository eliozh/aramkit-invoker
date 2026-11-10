//! Command-line parsing, validation, and launch resolution.
//!
//! This module holds the pure CLI surface (so it is testable without a
//! server). The scoped `lockfile`/log writers live in [`super::discovery`];
//! every filesystem write there is bounded by ADR-0007.

use std::fmt;
use std::net::TcpListener;
use std::path::PathBuf;

use super::fixtures::{
    MockOptions, MockState, DEFAULT_CHAMPION_ID, DEFAULT_LOCAL_CELL_ID, DEFAULT_PHASE, GAME_ID,
    MAYHEM_GAME_MODE, MAYHEM_QUEUE_ID,
};
use super::schedule::{Scenario, Schedule};

/// The app accepts tokens only from this alphabet (ADR-0005).
const MAX_TOKEN_BYTES: usize = 256;
/// Usage errors exit with this status; runtime failures use `EXIT_RUNTIME`.
pub const EXIT_USAGE: i32 = 2;
pub const EXIT_RUNTIME: i32 = 1;
/// Everything printed about the token goes through this literal.
pub const REDACTED: &str = "<redacted>";
const DEFAULT_STEP_MS: u64 = 1000;
const MIN_STEP_MS: u64 = 1;
const MAX_STEP_MS: u64 = 600_000;

/// The only phases the CLI accepts for `--phase` (ADR-0005).
const ALLOWED_PHASES: [&str; 12] = [
    "None",
    "Lobby",
    "Matchmaking",
    "ReadyCheck",
    "ChampSelect",
    "GameStart",
    "InProgress",
    "EndOfGame",
    "WaitingForStats",
    "PreEndOfGame",
    "FailedToLaunch",
    "TerminatedInError",
];

/// Every flag the parser recognizes. Used to tell a missing value from a
/// value that legitimately begins with `--` (for example a token).
const KNOWN_FLAGS: [&str; 13] = [
    "--app-port",
    "--remoting-auth-token",
    "--mode",
    "--scenario",
    "--phase",
    "--queue-id",
    "--game-mode",
    "--champion-id",
    "--local-cell-id",
    "--step-ms",
    "--install-root",
    "--reject-auth",
    "--gameflow-phase-only",
];

/// Discovery mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Process,
    Lockfile,
    Log,
}

impl Mode {
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "process" => Some(Self::Process),
            "lockfile" => Some(Self::Lockfile),
            "log" => Some(Self::Log),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Process => "process",
            Self::Lockfile => "lockfile",
            Self::Log => "log",
        }
    }
}

/// A usage (exit 2) or runtime (exit 1) failure with a one-line diagnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliError {
    message: String,
    exit_code: i32,
}

impl CliError {
    pub fn usage(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            exit_code: EXIT_USAGE,
        }
    }

    pub fn runtime(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            exit_code: EXIT_RUNTIME,
        }
    }

    pub fn exit_code(&self) -> i32 {
        self.exit_code
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for CliError {}

/// Converts OS arguments into UTF-8 strings.
///
/// `std::env::args()` panics on a non-UTF-8 argument; the CLI is string based,
/// so callers pass `args_os()` here to turn that case into a clear usage error.
/// The offending argument is deliberately not printed.
pub fn collect_utf8_args<I>(args: I) -> Result<Vec<String>, CliError>
where
    I: IntoIterator<Item = std::ffi::OsString>,
{
    args.into_iter()
        .map(|argument| {
            argument
                .into_string()
                .map_err(|_| CliError::usage("命令行参数必须是有效的 UTF-8"))
        })
        .collect()
}

/// Fully validated command line. The token is never rendered unredacted.
#[derive(Clone)]
pub struct CliConfig {
    pub mode: Mode,
    pub app_port: Option<u16>,
    pub remoting_auth_token: Option<String>,
    pub reject_auth: bool,
    pub gameflow_phase_only: bool,
    pub step_ms: u64,
    pub scenario: Option<Scenario>,
    pub phase: Option<String>,
    pub queue_id: i64,
    pub game_mode: String,
    pub champion_id: i64,
    pub local_cell_id: i64,
    pub install_root: Option<PathBuf>,
}

impl fmt::Debug for CliConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CliConfig")
            .field("mode", &self.mode)
            .field("app_port", &self.app_port)
            .field("remoting_auth_token", &REDACTED)
            .field("reject_auth", &self.reject_auth)
            .field("gameflow_phase_only", &self.gameflow_phase_only)
            .field("step_ms", &self.step_ms)
            .field("scenario", &self.scenario)
            .field("phase", &self.phase)
            .field("queue_id", &self.queue_id)
            .field("game_mode", &self.game_mode)
            .field("champion_id", &self.champion_id)
            .field("local_cell_id", &self.local_cell_id)
            .field("install_root", &self.install_root)
            .finish()
    }
}

impl CliConfig {
    /// Parses and validates a full argv (the first element is the program name).
    pub fn parse_from<I, S>(args: I) -> Result<Self, CliError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut arguments = args.into_iter().map(|arg| arg.as_ref().to_owned());
        let _program = arguments.next();
        let arguments: Vec<String> = arguments.collect();

        let mut raw = RawCli::default();
        let mut index = 0;

        while index < arguments.len() {
            let argument = &arguments[index];
            let (name, inline) = split_flag(argument);

            match name {
                "--app-port" => {
                    if raw.app_port.is_some() {
                        return Err(CliError::usage("--app-port 只能指定一次"));
                    }
                    let value = take_value(&arguments, &mut index, name, inline)?;
                    raw.app_port = Some(parse_port(&value)?);
                }
                "--remoting-auth-token" => {
                    if raw.remoting_auth_token.is_some() {
                        return Err(CliError::usage("--remoting-auth-token 只能指定一次"));
                    }
                    let value = take_value(&arguments, &mut index, name, inline)?;
                    if !is_valid_token(&value) {
                        return Err(CliError::usage(
                            "--remoting-auth-token 必须为 1..=256 字节且仅包含 [A-Za-z0-9_=-]",
                        ));
                    }
                    raw.remoting_auth_token = Some(value);
                }
                "--mode" => {
                    let value = take_value(&arguments, &mut index, name, inline)?;
                    raw.mode = Some(Mode::from_name(&value).ok_or_else(|| {
                        CliError::usage(format!("--mode 必须是 process、lockfile 或 log：{value}"))
                    })?);
                }
                "--scenario" => {
                    let value = take_value(&arguments, &mut index, name, inline)?;
                    raw.scenario = Some(Scenario::from_name(&value).ok_or_else(|| {
                        CliError::usage(format!(
                            "--scenario 必须是 mayhem、non-mayhem、dodge 或 failed-to-launch：{value}"
                        ))
                    })?);
                }
                "--phase" => {
                    let value = take_value(&arguments, &mut index, name, inline)?;
                    if !ALLOWED_PHASES.contains(&value.as_str()) {
                        return Err(CliError::usage(format!("--phase 无效：{value}")));
                    }
                    raw.phase = Some(value);
                }
                "--queue-id" => {
                    let value = take_value(&arguments, &mut index, name, inline)?;
                    raw.queue_id = Some(parse_integer(name, &value, 0)?);
                }
                "--game-mode" => {
                    let value = take_value(&arguments, &mut index, name, inline)?;
                    raw.game_mode = Some(validate_game_mode(&value)?);
                }
                "--champion-id" => {
                    let value = take_value(&arguments, &mut index, name, inline)?;
                    raw.champion_id = Some(parse_integer(name, &value, 1)?);
                }
                "--local-cell-id" => {
                    let value = take_value(&arguments, &mut index, name, inline)?;
                    raw.local_cell_id = Some(parse_integer(name, &value, 0)?);
                }
                "--step-ms" => {
                    let value = take_value(&arguments, &mut index, name, inline)?;
                    raw.step_ms = Some(parse_step_ms(&value)?);
                }
                "--install-root" => {
                    let value = take_value(&arguments, &mut index, name, inline)?;
                    if value.is_empty() {
                        return Err(CliError::usage("--install-root 不能为空"));
                    }
                    raw.install_root = Some(PathBuf::from(value));
                }
                "--reject-auth" => {
                    ensure_bare(name, inline)?;
                    raw.reject_auth = true;
                    index += 1;
                }
                "--gameflow-phase-only" => {
                    ensure_bare(name, inline)?;
                    raw.gameflow_phase_only = true;
                    index += 1;
                }
                _ => return Err(CliError::usage(format!("未知参数：{name}"))),
            }
        }

        if raw.phase.is_some() && raw.scenario.is_some() {
            return Err(CliError::usage("--phase 与 --scenario 不能同时使用"));
        }

        let mode = raw.mode.unwrap_or(Mode::Process);
        if raw.install_root.is_some() && mode == Mode::Process {
            return Err(CliError::usage(
                "--install-root 仅适用于 --mode lockfile 或 --mode log",
            ));
        }

        let scenario = match (&raw.phase, raw.scenario) {
            (Some(_), _) => None,
            (None, Some(scenario)) => Some(scenario),
            (None, None) => Some(Scenario::Mayhem),
        };
        let queue_id = raw
            .queue_id
            .unwrap_or_else(|| scenario.map_or(MAYHEM_QUEUE_ID, Scenario::queue_id));
        let game_mode = raw.game_mode.unwrap_or_else(|| {
            scenario
                .map_or(MAYHEM_GAME_MODE, Scenario::game_mode)
                .to_owned()
        });

        Ok(Self {
            mode,
            app_port: raw.app_port,
            remoting_auth_token: raw.remoting_auth_token,
            reject_auth: raw.reject_auth,
            gameflow_phase_only: raw.gameflow_phase_only,
            step_ms: raw.step_ms.unwrap_or(DEFAULT_STEP_MS),
            scenario,
            phase: raw.phase,
            queue_id,
            game_mode,
            champion_id: raw.champion_id.unwrap_or(DEFAULT_CHAMPION_ID),
            local_cell_id: raw.local_cell_id.unwrap_or(DEFAULT_LOCAL_CELL_ID),
            install_root: raw.install_root,
        })
    }

    /// The state the scenario/static overrides pin, without a phase timeline.
    pub fn state_template(&self) -> MockState {
        MockState {
            phase: DEFAULT_PHASE.to_owned(),
            game_id: GAME_ID,
            queue_id: self.queue_id,
            game_mode: self.game_mode.clone(),
            champion_id: self.champion_id,
            local_cell_id: self.local_cell_id,
        }
    }

    /// `--phase` yields a one-step static schedule; otherwise the scenario runs.
    pub fn schedule(&self) -> Schedule {
        if let Some(phase) = &self.phase {
            let mut state = self.state_template();
            state.phase = phase.clone();
            return Schedule::static_state(state);
        }

        let scenario = self.scenario.unwrap_or(Scenario::Mayhem);
        scenario.schedule(self.step_ms, &self.state_template())
    }

    /// Maps the CLI switches onto the server options.
    pub fn mock_options(&self) -> MockOptions {
        MockOptions {
            reject_auth: self.reject_auth,
            gameflow_phase_only: self.gameflow_phase_only,
        }
    }

    /// The redacted stdout summary. The token never appears here.
    pub fn summary(&self, port: u16) -> String {
        let selection = match (&self.phase, self.scenario) {
            (Some(phase), _) => format!("phase={phase}"),
            (None, Some(scenario)) => format!("scenario={}", scenario.name()),
            (None, None) => format!("scenario={}", Scenario::Mayhem.name()),
        };
        format!(
            "mock-lcu mode={} {selection} port={port} token={REDACTED}",
            self.mode.name()
        )
    }
}

/// Raw, not-yet-defaulted CLI values.
#[derive(Default)]
struct RawCli {
    mode: Option<Mode>,
    app_port: Option<u16>,
    remoting_auth_token: Option<String>,
    reject_auth: bool,
    gameflow_phase_only: bool,
    step_ms: Option<u64>,
    scenario: Option<Scenario>,
    phase: Option<String>,
    queue_id: Option<i64>,
    game_mode: Option<String>,
    champion_id: Option<i64>,
    local_cell_id: Option<i64>,
    install_root: Option<PathBuf>,
}

fn split_flag(argument: &str) -> (&str, Option<&str>) {
    match argument.split_once('=') {
        Some((name, value)) if name.starts_with("--") => (name, Some(value)),
        _ => (argument, None),
    }
}

fn take_value(
    arguments: &[String],
    index: &mut usize,
    name: &str,
    inline: Option<&str>,
) -> Result<String, CliError> {
    if let Some(value) = inline {
        *index += 1;
        return Ok(value.to_owned());
    }

    // Only a known flag name marks the value as missing. A value such as
    // `--abc` that merely starts with `--` is a legal token.
    let value = arguments
        .get(*index + 1)
        .ok_or_else(|| CliError::usage(format!("{name} 缺少取值")))?;
    let (next_name, _) = split_flag(value);
    if KNOWN_FLAGS.contains(&next_name) {
        return Err(CliError::usage(format!("{name} 缺少取值")));
    }
    *index += 2;
    Ok(value.clone())
}

fn ensure_bare(name: &str, inline: Option<&str>) -> Result<(), CliError> {
    if inline.is_some() {
        return Err(CliError::usage(format!("{name} 不接受取值")));
    }
    Ok(())
}

fn parse_port(value: &str) -> Result<u16, CliError> {
    value
        .parse::<u16>()
        .map_err(|_| CliError::usage(format!("--app-port 必须是 0..=65535 的整数：{value}")))
}

fn parse_integer(name: &str, value: &str, minimum: i64) -> Result<i64, CliError> {
    let parsed = value
        .parse::<i64>()
        .map_err(|_| CliError::usage(format!("{name} 必须是整数：{value}")))?;
    if parsed < minimum {
        return Err(CliError::usage(format!(
            "{name} 必须 >= {minimum}：{value}"
        )));
    }
    Ok(parsed)
}

fn parse_step_ms(value: &str) -> Result<u64, CliError> {
    let parsed = value
        .parse::<u64>()
        .map_err(|_| CliError::usage(format!("--step-ms 必须是整数：{value}")))?;
    if !(MIN_STEP_MS..=MAX_STEP_MS).contains(&parsed) {
        return Err(CliError::usage(format!(
            "--step-ms 必须在 {MIN_STEP_MS}..={MAX_STEP_MS} 之间：{value}"
        )));
    }
    Ok(parsed)
}

fn validate_game_mode(value: &str) -> Result<String, CliError> {
    let valid = !value.is_empty()
        && value.chars().count() <= 32
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'));
    if !valid {
        return Err(CliError::usage(format!(
            "--game-mode 必须为 1..=32 个 [A-Za-z0-9_-] 字符：{value}"
        )));
    }
    Ok(value.to_owned())
}

/// Token rule shared by the CLI, the discovery writers, and the server
/// (ADR-0005).
pub fn is_valid_token(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= MAX_TOKEN_BYTES
        && token.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '=')
        })
}

/// The outcome of process-mode launch resolution.
#[derive(Clone)]
pub struct LaunchResolution {
    pub port: u16,
    pub token: String,
    /// True when the caller must spawn a child with `args`.
    pub reexec: bool,
    /// Child argv (including the program name) when `reexec` is true.
    pub args: Vec<String>,
}

impl fmt::Debug for LaunchResolution {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LaunchResolution")
            .field("port", &self.port)
            .field("token", &REDACTED)
            .field("reexec", &self.reexec)
            .field("args", &RedactedArgs(&self.args))
            .finish()
    }
}

/// Renders an argv with credential values replaced by `<redacted>` so an
/// accidental `{:?}` cannot leak a token.
struct RedactedArgs<'a>(&'a [String]);

impl fmt::Debug for RedactedArgs<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut list = formatter.debug_list();
        let mut index = 0;
        while index < self.0.len() {
            let argument = &self.0[index];
            let is_separated_secret =
                argument == "--app-port" || argument == "--remoting-auth-token";
            if is_separated_secret {
                list.entry(argument);
                if self.0.get(index + 1).is_some() {
                    list.entry(&REDACTED);
                    index += 1;
                }
            } else {
                let (name, inline) = split_flag(argument);
                let is_inline_secret =
                    inline.is_some() && (name == "--app-port" || name == "--remoting-auth-token");
                if is_inline_secret {
                    list.entry(&format!("{name}={REDACTED}"));
                } else {
                    list.entry(argument);
                }
            }
            index += 1;
        }
        list.finish()
    }
}

/// Binds `127.0.0.1:0` just long enough to learn a free port (ADR-0001).
#[allow(dead_code)]
pub fn select_ephemeral_port() -> std::io::Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    Ok(listener.local_addr()?.port())
}

/// Decides whether this process serves or re-execs with a definitive pair.
///
/// `current_port`/`current_token` come from the validated CLI. A non-zero port
/// and a token mean the child role: serve without re-exec. Otherwise the
/// missing values are selected/generated, stale flag forms are stripped, and
/// exactly one `--flag=value` pair is advertised.
pub fn resolve_launch<P, G>(
    argv: &[String],
    current_port: Option<u16>,
    current_token: Option<&str>,
    select_port: P,
    generate: G,
) -> std::io::Result<LaunchResolution>
where
    P: FnOnce() -> std::io::Result<u16>,
    G: FnOnce() -> std::io::Result<String>,
{
    let present_port = current_port.filter(|port| *port > 0);
    let present_token = current_token.filter(|token| !token.is_empty());

    if let (Some(port), Some(token)) = (present_port, present_token) {
        return Ok(LaunchResolution {
            port,
            token: token.to_owned(),
            reexec: false,
            args: argv.to_vec(),
        });
    }

    let port = match present_port {
        Some(port) => port,
        None => select_port()?,
    };
    let token = match present_token {
        Some(token) => token.to_owned(),
        None => generate()?,
    };

    let mut args = strip_credential_flags(argv);
    args.push(format!("--app-port={port}"));
    args.push(format!("--remoting-auth-token={token}"));

    Ok(LaunchResolution {
        port,
        token,
        reexec: true,
        args,
    })
}

/// Removes every `--flag=value` and `--flag value` form of the credential flags.
///
/// A separated value is dropped with the same rule `take_value` uses: unless
/// the next token is a known flag name, it is a value even when it begins with
/// `--`. This keeps re-exec from leaving a stale dash-prefixed token behind.
fn strip_credential_flags(argv: &[String]) -> Vec<String> {
    let mut stripped = Vec::with_capacity(argv.len());
    let mut index = 0;

    while index < argv.len() {
        let argument = &argv[index];
        if argument == "--app-port" || argument == "--remoting-auth-token" {
            index += 1;
            if index < argv.len() {
                let (next_name, _) = split_flag(&argv[index]);
                if !KNOWN_FLAGS.contains(&next_name) {
                    index += 1;
                }
            }
            continue;
        }
        if argument.starts_with("--app-port=") || argument.starts_with("--remoting-auth-token=") {
            index += 1;
            continue;
        }
        stripped.push(argument.clone());
        index += 1;
    }

    stripped
}
