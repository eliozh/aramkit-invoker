//! Shared, self-contained mock of the League client (LCU) API.
//!
//! This file is compiled twice: as the core of the `LeagueClientUx` example
//! and as a `#[cfg(test)]` module of the library crate. It therefore uses only
//! the standard library, the four pinned dev-dependencies, and `serde_json`.
//! It must never reference `crate::`, and no release build compiles it.
//!
//! The server terminates TLS with the committed test-only fixture, binds
//! `127.0.0.1` only, requires `Basic riot:<token>` for every request before
//! routing, and serves exactly the four read-only endpoints the app uses.

use std::fmt;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use httparse::Request as HttpRequest;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ServerConfig, ServerConnection, StreamOwned};
use serde_json::{json, Value};

const CERT_DER: &[u8] = include_bytes!("testdata/cert.der");
const KEY_DER: &[u8] = include_bytes!("testdata/key.der");

/// Request-head cap from ADR-0006; larger heads get `400`.
const MAX_HEAD_BYTES: usize = 8 * 1024;
/// Read and write timeout for one connection.
const IO_TIMEOUT: Duration = Duration::from_secs(5);
/// Total wall-clock budget for reading one request head. The per-read timeout
/// is an additional bound; together they stop a dripper from holding the
/// single-threaded accept loop indefinitely (ADR-0007 slowloris rule).
const HEAD_READ_DEADLINE: Duration = Duration::from_secs(5);
/// Poll interval while the non-blocking listener waits for a connection.
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(10);
/// Stop serving after this many consecutive accept failures; a persistent
/// listener error is not recoverable by polling, and retrying forever would
/// spin the CPU and flood stderr.
const MAX_CONSECUTIVE_ACCEPT_ERRORS: u32 = 100;

const SESSION_PATH: &str = "/lol-gameflow/v1/session";
const PHASE_PATH: &str = "/lol-gameflow/v1/gameflow-phase";
const CHAMP_SELECT_PATH: &str = "/lol-champ-select/v1/session";
const CHAMPION_SUMMARY_PATH: &str = "/lol-game-data/assets/v1/champion-summary.json";

const GAME_ID: i64 = 424_242;
const MAYHEM_QUEUE_ID: i64 = 2400;
const MAYHEM_GAME_MODE: &str = "KIWI";
const ARAM_QUEUE_ID: i64 = 450;
const ARAM_GAME_MODE: &str = "ARAM";
const DEFAULT_CHAMPION_ID: i64 = 103;
const DEFAULT_LOCAL_CELL_ID: i64 = 1;
const DEFAULT_PHASE: &str = "Lobby";
const DEFAULT_STEP_MS: u64 = 1000;
const MIN_STEP_MS: u64 = 1;
const MAX_STEP_MS: u64 = 600_000;
/// The app accepts tokens only from this alphabet (ADR-0005).
const MAX_TOKEN_BYTES: usize = 256;
/// Usage errors exit with this status; runtime failures use `EXIT_RUNTIME`.
pub const EXIT_USAGE: i32 = 2;
#[allow(dead_code)]
pub const EXIT_RUNTIME: i32 = 1;
/// Everything printed about the token goes through this literal.
pub const REDACTED: &str = "<redacted>";

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

/// (id, name, alias) entries shipped in every champion summary.
const FIXTURE_CHAMPIONS: [(i64, &str, &str); 3] = [
    (103, "Ahri", "Ahri"),
    (121, "Kha'Zix", "Khazix"),
    (62, "Wukong", "MonkeyKing"),
];

/// Mutable scenario state shared between the test handle and the server.
#[derive(Debug, Clone)]
pub struct MockState {
    pub phase: String,
    pub game_id: i64,
    pub queue_id: i64,
    pub game_mode: String,
    pub champion_id: i64,
    pub local_cell_id: i64,
}

impl Default for MockState {
    fn default() -> Self {
        Self {
            phase: DEFAULT_PHASE.to_owned(),
            game_id: GAME_ID,
            queue_id: MAYHEM_QUEUE_ID,
            game_mode: MAYHEM_GAME_MODE.to_owned(),
            champion_id: DEFAULT_CHAMPION_ID,
            local_cell_id: DEFAULT_LOCAL_CELL_ID,
        }
    }
}

/// Behavior switches that do not belong to the scenario state.
#[derive(Debug, Clone, Default)]
pub struct MockOptions {
    /// Return `401` for every request, including a correct token.
    pub reject_auth: bool,
    /// Return `404` from `/lol-gameflow/v1/session` so the app exercises the
    /// `/lol-gameflow/v1/gameflow-phase` fallback.
    pub gameflow_phase_only: bool,
}

/// One scripted state and how long it is held.
#[derive(Debug, Clone)]
pub struct Step {
    pub state: MockState,
    pub dwell_ms: u64,
}

/// A pure timeline over elapsed time. The final step holds indefinitely.
#[derive(Debug, Clone)]
pub struct Schedule {
    pub steps: Vec<Step>,
}

impl Schedule {
    /// A one-step schedule that holds forever, used by `--phase`.
    pub fn static_state(state: MockState) -> Self {
        Self {
            steps: vec![Step { state, dwell_ms: 0 }],
        }
    }

    /// Returns the state that is active at `elapsed`.
    ///
    /// A step is active for `[start, start + dwell)`; the last step has no
    /// successor and holds for every later instant. This is pure, so tests can
    /// assert boundaries without sleeping.
    pub fn state_at(&self, elapsed: Duration) -> MockState {
        let elapsed_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
        let mut cumulative = 0u64;
        for step in &self.steps {
            cumulative = cumulative.saturating_add(step.dwell_ms);
            if elapsed_ms < cumulative {
                return step.state.clone();
            }
        }
        self.steps
            .last()
            .map(|step| step.state.clone())
            .unwrap_or_default()
    }
}

/// The scripted timelines from ADR-0005.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scenario {
    Mayhem,
    NonMayhem,
    Dodge,
    FailedToLaunch,
}

impl Scenario {
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "mayhem" => Some(Self::Mayhem),
            "non-mayhem" => Some(Self::NonMayhem),
            "dodge" => Some(Self::Dodge),
            "failed-to-launch" => Some(Self::FailedToLaunch),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Mayhem => "mayhem",
            Self::NonMayhem => "non-mayhem",
            Self::Dodge => "dodge",
            Self::FailedToLaunch => "failed-to-launch",
        }
    }

    pub fn queue_id(self) -> i64 {
        match self {
            Self::NonMayhem => ARAM_QUEUE_ID,
            Self::Mayhem | Self::Dodge | Self::FailedToLaunch => MAYHEM_QUEUE_ID,
        }
    }

    pub fn game_mode(self) -> &'static str {
        match self {
            Self::NonMayhem => ARAM_GAME_MODE,
            Self::Mayhem | Self::Dodge | Self::FailedToLaunch => MAYHEM_GAME_MODE,
        }
    }

    /// Builds the scenario timeline; `template` supplies queue, mode, and IDs.
    pub fn schedule(self, step_ms: u64, template: &MockState) -> Schedule {
        let step = |phase: &str, multiplier: u64| Step {
            state: MockState {
                phase: phase.to_owned(),
                ..template.clone()
            },
            dwell_ms: step_ms.saturating_mul(multiplier),
        };

        let steps = match self {
            Self::Mayhem | Self::NonMayhem => vec![
                step(DEFAULT_PHASE, 2),
                step("ChampSelect", 12),
                step("GameStart", 4),
                step("InProgress", 0),
            ],
            Self::Dodge => vec![
                step(DEFAULT_PHASE, 2),
                step("ChampSelect", 12),
                step(DEFAULT_PHASE, 0),
            ],
            Self::FailedToLaunch => vec![
                step(DEFAULT_PHASE, 2),
                step("ChampSelect", 12),
                step("FailedToLaunch", 0),
            ],
        };

        Schedule { steps }
    }
}

/// Discovery mode. Slice 03 implements the file-writing modes.
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

    #[allow(dead_code)]
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
#[allow(dead_code)]
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

/// Token rule shared by the CLI and the server (ADR-0005).
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
fn strip_credential_flags(argv: &[String]) -> Vec<String> {
    let mut stripped = Vec::with_capacity(argv.len());
    let mut index = 0;

    while index < argv.len() {
        let argument = &argv[index];
        if argument == "--app-port" || argument == "--remoting-auth-token" {
            index += 1;
            // Drop the separated value unless the next token is another flag.
            if index < argv.len() && !argv[index].starts_with("--") {
                index += 1;
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

/// A schedule evaluated from the server's start instant, read per request.
struct ScheduledState {
    schedule: Schedule,
    started: Instant,
}

/// A running loopback HTTPS mock server. Dropping it stops the thread.
pub struct MockServer {
    address: SocketAddr,
    /// Read by `argv`; the example advertises the pair through its real argv.
    #[allow(dead_code)]
    token: String,
    /// Mutated by in-process tests; the CLI uses a schedule instead.
    #[allow(dead_code)]
    state: Arc<RwLock<MockState>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl MockServer {
    /// Binds `127.0.0.1:0`, generates a per-run token, and starts serving.
    #[allow(dead_code)]
    pub fn start(options: MockOptions) -> std::io::Result<Self> {
        Self::start_on(0, None, options, None)
    }

    /// Binds `127.0.0.1:<port>` (`0` = ephemeral) with an optional token and
    /// schedule. A `None` token is generated; a `Some` schedule is evaluated
    /// per request from the start instant, otherwise the state handle is read.
    pub fn start_on(
        port: u16,
        token: Option<String>,
        options: MockOptions,
        schedule: Option<Schedule>,
    ) -> std::io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", port))?;
        let address = listener.local_addr()?;
        listener.set_nonblocking(true)?;

        let token = match token {
            Some(token) => token,
            None => generate_token()?,
        };
        let expected_credential = STANDARD.encode(format!("riot:{token}"));

        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|error| std::io::Error::other(error.to_string()))?
            .with_no_client_auth()
            .with_single_cert(
                vec![CertificateDer::from(CERT_DER)],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(KEY_DER)),
            )
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        let config = Arc::new(config);

        let state = Arc::new(RwLock::new(MockState::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let scheduled = schedule.map(|schedule| ScheduledState {
            schedule,
            started: Instant::now(),
        });

        let thread_state = Arc::clone(&state);
        let thread_stop = Arc::clone(&stop);
        let thread = thread::Builder::new()
            .name("mock-lcu-core".to_owned())
            .spawn(move || {
                serve(
                    &listener,
                    &config,
                    &thread_state,
                    &options,
                    &expected_credential,
                    &thread_stop,
                    scheduled.as_ref(),
                );
            })?;

        Ok(Self {
            address,
            token,
            state,
            stop,
            thread: Some(thread),
        })
    }

    /// The address the listener actually bound (always `127.0.0.1`).
    pub fn local_addr(&self) -> SocketAddr {
        self.address
    }

    /// The shared static scenario state.
    ///
    /// A schedule takes precedence over this handle: when the server was
    /// started with `Some(schedule)`, every request is answered from the
    /// schedule and mutations to this handle have no effect. Without a
    /// schedule, this is the live state the server reads.
    #[allow(dead_code)]
    pub fn state(&self) -> Arc<RwLock<MockState>> {
        Arc::clone(&self.state)
    }

    /// The command line a real client would advertise, for `parse_lcu_args`.
    #[allow(dead_code)]
    pub fn argv(&self) -> Vec<String> {
        vec![
            "LeagueClientUx.exe".to_owned(),
            format!("--app-port={}", self.address.port()),
            format!("--remoting-auth-token={}", self.token),
        ]
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            if thread.join().is_err() {
                // The panic payload is discarded so nothing it carried (for
                // example a credential) can reach stderr.
                eprintln!("mock-lcu: server thread panicked during shutdown");
            }
        }
    }
}

/// Generates the per-run remoting token: 32 OS-random bytes, base64 URL-safe
/// without padding (43 characters, a subset of the app's accepted charset).
pub fn generate_token() -> std::io::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|error| std::io::Error::other(error.to_string()))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// One connection at a time, matching the sequential app watcher.
#[allow(clippy::too_many_arguments)]
fn serve(
    listener: &TcpListener,
    config: &Arc<ServerConfig>,
    state: &Arc<RwLock<MockState>>,
    options: &MockOptions,
    expected_credential: &str,
    stop: &AtomicBool,
    scheduled: Option<&ScheduledState>,
) {
    let mut consecutive_accept_errors = 0u32;
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _peer)) => {
                consecutive_accept_errors = 0;
                // A socket we cannot configure (blocking mode and both
                // timeouts) has no slowloris bound; close it instead of
                // serving it without those guards.
                if configure_stream(&stream).is_err() {
                    continue;
                }

                let Ok(connection) = ServerConnection::new(Arc::clone(config)) else {
                    continue;
                };
                let mut tls = StreamOwned::new(connection, stream);
                handle_connection(&mut tls, state, options, expected_credential, scheduled);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                // No pending connection: not an error, and it ends any burst.
                consecutive_accept_errors = 0;
                thread::sleep(ACCEPT_POLL_INTERVAL);
            }
            Err(error) => {
                // Log once per burst, then give up after the bounded count.
                if consecutive_accept_errors == 0 {
                    eprintln!("mock-lcu: accept 失败：{error}");
                }
                consecutive_accept_errors = consecutive_accept_errors.saturating_add(1);
                if consecutive_accept_errors >= MAX_CONSECUTIVE_ACCEPT_ERRORS {
                    eprintln!("mock-lcu: accept 连续失败 {consecutive_accept_errors} 次，停止监听");
                    break;
                }
                thread::sleep(ACCEPT_POLL_INTERVAL);
            }
        }
    }
}

/// Applies the blocking mode and both I/O timeouts. Returns the first error so
/// the caller can decline to serve a socket without its slowloris bounds.
fn configure_stream(stream: &TcpStream) -> std::io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    Ok(())
}

fn handle_connection(
    stream: &mut StreamOwned<ServerConnection, TcpStream>,
    state: &Arc<RwLock<MockState>>,
    options: &MockOptions,
    expected_credential: &str,
    scheduled: Option<&ScheduledState>,
) {
    let response = match read_request_head(stream) {
        Ok(Some(head)) => respond(
            &head,
            &resolved_state(state, scheduled),
            options,
            expected_credential,
        ),
        Ok(None) => MockResponse::empty(400),
        // TLS handshake and socket errors close without an HTTP response.
        Err(_) => return,
    };
    // Best-effort write: the connection closes right after, so a write error
    // only truncates the response and the socket drop ends the connection.
    let _ = write_response(stream, &response);
}

/// A scheduled server evaluates the timeline; a static server reads its handle.
fn resolved_state(state: &Arc<RwLock<MockState>>, scheduled: Option<&ScheduledState>) -> MockState {
    if let Some(scheduled) = scheduled {
        return scheduled.schedule.state_at(scheduled.started.elapsed());
    }
    state
        .read()
        .unwrap_or_else(|error| error.into_inner())
        .clone()
}

/// Reads one request head with two bounds: `MAX_HEAD_BYTES` and a total
/// `HEAD_READ_DEADLINE`. Returns `Ok(None)` when either bound is exceeded so
/// the caller answers `400` and closes.
fn read_request_head(stream: &mut impl Read) -> std::io::Result<Option<Vec<u8>>> {
    let deadline = Instant::now() + HEAD_READ_DEADLINE;
    let mut head = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];

    loop {
        if head.windows(4).any(|window| window == b"\r\n\r\n") {
            return Ok(Some(head));
        }
        if head.len() >= MAX_HEAD_BYTES {
            return Ok(None);
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }

        let read = stream.read(&mut chunk)?;
        if read == 0 {
            return Ok(None);
        }
        // Reject before appending: never let the head grow past the cap.
        if head.len() + read > MAX_HEAD_BYTES {
            return Ok(None);
        }
        head.extend_from_slice(&chunk[..read]);
    }
}

fn respond(
    head: &[u8],
    state: &MockState,
    options: &MockOptions,
    expected_credential: &str,
) -> MockResponse {
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut request = HttpRequest::new(&mut headers);

    match request.parse(head) {
        Ok(httparse::Status::Complete(_)) => {}
        _ => return MockResponse::empty(400),
    }

    if request.version != Some(1) {
        return MockResponse::empty(400);
    }

    let (Some(method), Some(path)) = (request.method, request.path) else {
        return MockResponse::empty(400);
    };

    if has_body_or_transfer_encoding(&request) {
        return MockResponse::empty(400);
    }

    if !is_authorized(&request, options.reject_auth, expected_credential) {
        return MockResponse::unauthorized();
    }

    if method != "GET" {
        return MockResponse::method_not_allowed();
    }

    route(path, state, options)
}

fn has_body_or_transfer_encoding(request: &HttpRequest<'_, '_>) -> bool {
    request.headers.iter().any(|header| {
        if header.name.eq_ignore_ascii_case("transfer-encoding") {
            return true;
        }
        if header.name.eq_ignore_ascii_case("content-length") {
            let value = String::from_utf8_lossy(header.value);
            return value
                .trim()
                .parse::<u64>()
                .map_or(true, |length| length > 0);
        }
        false
    })
}

fn is_authorized(request: &HttpRequest<'_, '_>, reject_auth: bool, expected: &str) -> bool {
    if reject_auth {
        return false;
    }

    let mut values = request
        .headers
        .iter()
        .filter(|header| header.name.eq_ignore_ascii_case("authorization"));
    let Some(header) = values.next() else {
        return false;
    };
    if values.next().is_some() {
        return false;
    }

    let Ok(value) = std::str::from_utf8(header.value) else {
        return false;
    };
    let Some((scheme, credential)) = value.split_once(' ') else {
        return false;
    };
    scheme.eq_ignore_ascii_case("Basic")
        && constant_time_eq(credential.as_bytes(), expected.as_bytes())
}

/// Compares credentials without an early exit on the first differing byte.
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }

    let difference = left
        .iter()
        .zip(right.iter())
        .fold(0u8, |difference, (left, right)| difference | (left ^ right));
    // `black_box` keeps the compiler from folding or removing the OR
    // reduction, which is what makes this comparison constant time.
    std::hint::black_box(difference) == 0
}

fn route(path: &str, state: &MockState, options: &MockOptions) -> MockResponse {
    match path {
        SESSION_PATH if !options.gameflow_phase_only => {
            MockResponse::json(200, gameflow_body(state))
        }
        SESSION_PATH => MockResponse::empty(404),
        PHASE_PATH => MockResponse::json(200, Value::String(state.phase.clone())),
        CHAMP_SELECT_PATH if state.phase == "ChampSelect" => {
            MockResponse::json(200, champ_select_body(state))
        }
        CHAMP_SELECT_PATH => MockResponse::empty(404),
        CHAMPION_SUMMARY_PATH => MockResponse::json(200, champion_summary_body(state)),
        _ => MockResponse::empty(404),
    }
}

fn gameflow_body(state: &MockState) -> Value {
    json!({
        "phase": &state.phase,
        "gameData": {
            "gameId": state.game_id,
            "queue": {
                "id": state.queue_id,
                "gameMode": &state.game_mode,
            },
        },
    })
}

fn champ_select_body(state: &MockState) -> Value {
    json!({
        "localPlayerCellId": state.local_cell_id,
        "myTeam": [{
            "cellId": state.local_cell_id,
            "championId": state.champion_id,
        }],
        "actions": [[{
            "actorCellId": state.local_cell_id,
            "championId": state.champion_id,
            "completed": true,
            "type": "pick",
        }]],
    })
}

fn champion_summary_body(state: &MockState) -> Value {
    let mut entries: Vec<Value> = FIXTURE_CHAMPIONS
        .iter()
        .map(|&(id, name, alias)| json!({ "id": id, "name": name, "alias": alias }))
        .collect();

    if !FIXTURE_CHAMPIONS
        .iter()
        .any(|(id, _, _)| *id == state.champion_id)
    {
        entries.push(json!({
            "id": state.champion_id,
            "name": format!("Mock Champion {}", state.champion_id),
            "alias": format!("MockChampion{}", state.champion_id),
        }));
    }

    Value::Array(entries)
}

struct MockResponse {
    status: u16,
    reason: &'static str,
    extra_headers: Vec<(&'static str, String)>,
    body: Vec<u8>,
}

impl MockResponse {
    fn empty(status: u16) -> Self {
        Self {
            status,
            reason: reason_phrase(status),
            extra_headers: Vec::new(),
            body: Vec::new(),
        }
    }

    fn json(status: u16, value: Value) -> Self {
        match serde_json::to_vec(&value) {
            Ok(body) => Self {
                status,
                reason: reason_phrase(status),
                extra_headers: vec![
                    ("Content-Type", "application/json".to_owned()),
                    ("Cache-Control", "no-store".to_owned()),
                ],
                body,
            },
            Err(_) => Self::empty(500),
        }
    }

    fn unauthorized() -> Self {
        Self {
            status: 401,
            reason: reason_phrase(401),
            extra_headers: vec![("WWW-Authenticate", "Basic realm=\"mock-lcu\"".to_owned())],
            body: Vec::new(),
        }
    }

    fn method_not_allowed() -> Self {
        Self {
            status: 405,
            reason: reason_phrase(405),
            extra_headers: vec![("Allow", "GET".to_owned())],
            body: Vec::new(),
        }
    }
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Internal Server Error",
    }
}

fn write_response(
    stream: &mut StreamOwned<ServerConnection, TcpStream>,
    response: &MockResponse,
) -> std::io::Result<()> {
    let mut output = Vec::with_capacity(response.body.len() + 160);
    output.extend_from_slice(
        format!("HTTP/1.1 {} {}\r\n", response.status, response.reason).as_bytes(),
    );
    output.extend_from_slice(format!("Content-Length: {}\r\n", response.body.len()).as_bytes());
    output.extend_from_slice(b"Connection: close\r\n");
    for (name, value) in &response.extra_headers {
        output.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
    }
    output.extend_from_slice(b"\r\n");
    output.extend_from_slice(&response.body);

    // Best-effort on close: the caller discards any error from this write.
    stream.write_all(&output)?;
    stream.flush()
}
