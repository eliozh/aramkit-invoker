use std::ffi::OsStr;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
use zeroize::Zeroize;

/// Local credentials exposed by the League client on the loopback interface.
pub struct LcuCredentials {
    pub port: u16,
    pub token: String,
}

impl std::fmt::Debug for LcuCredentials {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LcuCredentials")
            .field("port", &self.port)
            .field("token", &"<redacted>")
            .finish()
    }
}

impl Drop for LcuCredentials {
    fn drop(&mut self) {
        self.token.zeroize();
    }
}

impl LcuCredentials {
    pub fn base_url(&self) -> String {
        format!("https://127.0.0.1:{}", self.port)
    }
}

/// Outcome of one discovery round.
#[derive(Debug, Default)]
pub struct Discovery {
    pub credentials: Option<LcuCredentials>,
    /// True when a `LeagueClientUx` process exists even if its credentials
    /// could not be read (for example when the client runs elevated).
    pub client_running: bool,
}

/// File suffix of the session log written by the client's UX process.
const UX_LOG_SUFFIX: &str = "_LeagueClientUx.log";
/// The launch arguments are logged at the very beginning of the file.
const LOG_HEAD_BYTES: u64 = 64 * 1024;
/// How many of the newest session logs to inspect.
const MAX_LOG_CANDIDATES: usize = 4;

/// Likely install roots, used when the client process cannot be inspected.
///
/// The Chinese (Tencent/WeGame) client usually lives outside the Riot Games
/// directory, so both layouts are covered.
const COMMON_INSTALL_ROOTS: &[&str] = &[
    r"C:\Riot Games\League of Legends",
    r"D:\Riot Games\League of Legends",
    r"E:\Riot Games\League of Legends",
    r"F:\Riot Games\League of Legends",
    r"C:\Program Files\Riot Games\League of Legends",
    r"D:\Program Files\Riot Games\League of Legends",
    r"C:\Riot Games\英雄联盟",
    r"D:\Riot Games\英雄联盟",
    r"C:\Program Files\Tencent\League of Legends\英雄联盟LCU",
    r"D:\Program Files\Tencent\League of Legends\英雄联盟LCU",
    r"C:\Program Files\WeGame\英雄联盟",
    r"D:\WeGameApps\英雄联盟",
    r"D:\WeGame\英雄联盟",
    r"E:\WeGame\英雄联盟",
    r"D:\腾讯游戏\英雄联盟",
    r"E:\腾讯游戏\英雄联盟",
];

/// A running League client process.
struct LeagueProcess {
    pid: u32,
    exe: Option<PathBuf>,
    args: Vec<String>,
}

impl Drop for LeagueProcess {
    fn drop(&mut self) {
        // Never leave a copy of the remoting token in memory.
        for arg in &mut self.args {
            arg.zeroize();
        }
    }
}

/// Finds the running client's credentials.
///
/// Strategies, in order:
///
/// 1. The `LeagueClientUx` command line (`--app-port`,
///    `--remoting-auth-token`).
/// 2. The `lockfile` next to the client executable or in a known install
///    directory.
/// 3. The newest `*_LeagueClientUx.log`, which records the original command
///    line.
///
/// Steps 2 and 3 matter for the Chinese (Tencent/WeGame) client: WeGame often
/// launches the client elevated, so a non-elevated process cannot read its
/// command line, and the client may leave a zero-byte `lockfile`. The session
/// log is readable without elevation and contains the same arguments.
pub fn discover() -> Discovery {
    let processes = league_processes();
    let client_running = !processes.is_empty();

    if let Some(credentials) = credentials_from_command_lines(&processes) {
        return Discovery {
            credentials: Some(credentials),
            client_running,
        };
    }

    let install_roots = install_roots(&processes);
    let credentials = credentials_from_lockfiles(&install_roots)
        .or_else(|| credentials_from_logs(&install_roots));

    Discovery {
        credentials,
        client_running,
    }
}

fn league_processes() -> Vec<LeagueProcess> {
    let Ok(mut system) = System::new() else {
        return Vec::new();
    };

    let refresh = ProcessRefreshKind::nothing()
        .with_cmd(UpdateKind::Always)
        .with_exe(UpdateKind::OnlyIfNotSet)
        .without_tasks();
    system.refresh_processes_specifics(ProcessesToUpdate::All, true, refresh);

    let mut processes: Vec<LeagueProcess> = system
        .processes()
        .iter()
        .filter(|(_, process)| is_league_client_process(process.name()))
        .map(|(pid, process)| LeagueProcess {
            pid: pid.as_u32(),
            exe: process_image_path(pid.as_u32()).or_else(|| {
                process
                    .exe()
                    .filter(|exe| !exe.as_os_str().is_empty())
                    .map(Path::to_path_buf)
            }),
            args: process
                .cmd()
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect(),
        })
        .collect();

    // Deterministic order: prefer the most recently started client.
    processes.sort_by_key(|process| std::cmp::Reverse(process.pid));
    processes
}

fn credentials_from_command_lines(processes: &[LeagueProcess]) -> Option<LcuCredentials> {
    for process in processes {
        let mut credentials = parse_lcu_args(process.args.iter());

        // Some Windows configurations return the command line as one string.
        if credentials.is_none() && process.args.len() == 1 {
            credentials = parse_lcu_args(process.args[0].split_whitespace());
        }

        if let Some(credentials) = credentials.filter(port_accepts_connections) {
            return Some(credentials);
        }
    }

    None
}

/// Install directories derived from the client executable, plus known paths.
fn install_roots(processes: &[LeagueProcess]) -> Vec<PathBuf> {
    let mut roots = Vec::new();

    for process in processes {
        let Some(directory) = process.exe.as_deref().and_then(Path::parent) else {
            continue;
        };
        push_unique(&mut roots, directory.to_path_buf());
        if let Some(parent) = directory.parent() {
            push_unique(&mut roots, parent.to_path_buf());
        }
    }

    for root in COMMON_INSTALL_ROOTS {
        push_unique(&mut roots, PathBuf::from(root));
    }

    roots
}

fn push_unique(roots: &mut Vec<PathBuf>, root: PathBuf) {
    if !root.as_os_str().is_empty() && !roots.contains(&root) {
        roots.push(root);
    }
}

fn credentials_from_lockfiles(roots: &[PathBuf]) -> Option<LcuCredentials> {
    for root in roots {
        for candidate in [
            root.join("lockfile"),
            root.join("LeagueClient").join("lockfile"),
            root.join("Game").join("lockfile"),
        ] {
            // An empty file is exactly what the CN client leaves behind, so a
            // failed parse is not an error — keep looking.
            if let Ok(mut contents) = std::fs::read_to_string(&candidate) {
                let credentials = parse_lockfile(&contents);
                contents.zeroize();
                if let Some(credentials) = credentials.filter(port_accepts_connections) {
                    return Some(credentials);
                }
            }
        }
    }

    None
}

fn port_accepts_connections(credentials: &LcuCredentials) -> bool {
    let address = std::net::SocketAddr::from(([127, 0, 0, 1], credentials.port));
    std::net::TcpStream::connect_timeout(&address, std::time::Duration::from_millis(300)).is_ok()
}

fn credentials_from_logs(roots: &[PathBuf]) -> Option<LcuCredentials> {
    for root in roots {
        for directory in log_directories(root) {
            if let Some(credentials) = credentials_from_log_directory(&directory) {
                return Some(credentials);
            }
        }
    }

    None
}

fn log_directories(root: &Path) -> [PathBuf; 3] {
    [
        root.join("Logs").join("LeagueClient Logs"),
        root.join("Game").join("Logs").join("LeagueClient Logs"),
        root.join("LeagueClient")
            .join("Logs")
            .join("LeagueClient Logs"),
    ]
}

fn credentials_from_log_directory(directory: &Path) -> Option<LcuCredentials> {
    let mut logs: Vec<(SystemTime, PathBuf)> = std::fs::read_dir(directory)
        .ok()?
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let name = path.file_name()?.to_str()?;
            if !name.ends_with(UX_LOG_SUFFIX) {
                return None;
            }

            let modified = entry
                .metadata()
                .ok()
                .and_then(|metadata| metadata.modified().ok())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            Some((modified, path))
        })
        .collect();

    // Newest session first: an older log belongs to a client that has exited.
    logs.sort_by_key(|left| std::cmp::Reverse(left.0));

    logs.into_iter()
        .take(MAX_LOG_CANDIDATES)
        .find_map(|(_, path)| credentials_from_log_file(&path))
}

fn credentials_from_log_file(path: &Path) -> Option<LcuCredentials> {
    let mut head = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(LOG_HEAD_BYTES)
        .read_to_end(&mut head)
        .ok()?;

    // The client writes UTF-8 with stray NUL bytes; older builds used UTF-16,
    // which becomes valid text once the NUL bytes are removed.
    let text: String = String::from_utf8_lossy(&head)
        .chars()
        .filter(|character| *character != '\0')
        .collect();

    let port = argument_value(&text, "--app-port=")?
        .parse::<u16>()
        .ok()
        .filter(|port| *port > 0)?;

    let token = argument_value(&text, "--remoting-auth-token=")?;
    if !is_valid_token(token) {
        return None;
    }

    Some(LcuCredentials {
        port,
        token: token.to_owned(),
    })
}

/// Reads a `--flag=value` argument out of free-form text.
///
/// The value ends at the next whitespace, quote, or NUL byte.
fn argument_value<'a>(text: &'a str, flag: &str) -> Option<&'a str> {
    let mut offset = 0;

    while let Some(index) = text[offset..].find(flag) {
        let value_start = offset + index + flag.len();
        let rest = &text[value_start..];
        let value_end = rest
            .find(|character: char| {
                character.is_whitespace() || character == '"' || character == '\0'
            })
            .unwrap_or(rest.len());
        let value = rest[..value_end].trim_matches('"');

        if !value.is_empty() {
            return Some(value);
        }

        offset = value_start;
    }

    None
}

fn is_valid_token(token: &str) -> bool {
    !token.is_empty()
        && token.chars().all(|character| {
            character.is_ascii_alphanumeric()
                || character == '-'
                || character == '_'
                || character == '='
        })
}

/// Finds `--app-port` and `--remoting-auth-token` in a League process command line.
pub fn parse_lcu_args<I, S>(args: I) -> Option<LcuCredentials>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut args: Vec<String> = args
        .into_iter()
        .map(|arg| arg.as_ref().to_owned())
        .collect();

    let mut port = None;
    let mut token = None;
    let mut index = 0;

    while index < args.len() {
        let arg = args[index].trim_matches('"');

        if let Some(value) = arg.strip_prefix("--app-port=") {
            port = parse_port(value);
        } else if arg == "--app-port" {
            if let Some(value) = args.get(index + 1) {
                port = parse_port(value.trim_matches('"'));
                index += 1;
            }
        } else if let Some(value) = arg.strip_prefix("--remoting-auth-token=") {
            token = clean_token(value);
        } else if arg == "--remoting-auth-token" {
            if let Some(value) = args.get(index + 1) {
                token = clean_token(value);
                index += 1;
            }
        }

        index += 1;
    }

    // Do not leave a copy of the token in the temporary argument vector.
    for arg in &mut args {
        arg.zeroize();
    }

    match (port, token) {
        (Some(port), Some(token)) if port > 0 && !token.is_empty() => {
            Some(LcuCredentials { port, token })
        }
        _ => None,
    }
}

/// Parses the client's `lockfile` (`name:pid:port:password:protocol`).
pub fn parse_lockfile(contents: &str) -> Option<LcuCredentials> {
    let parts: Vec<&str> = contents.trim().splitn(5, ':').collect();
    if parts.len() < 5 {
        return None;
    }

    let port = parts[2].parse::<u16>().ok().filter(|port| *port > 0)?;
    let token = parts[3].trim();
    if token.is_empty() {
        return None;
    }

    Some(LcuCredentials {
        port,
        token: token.to_owned(),
    })
}

/// Returns the full executable path of a process.
///
/// `PROCESS_QUERY_LIMITED_INFORMATION` is permitted across integrity levels,
/// so this still works when the League client runs elevated — unlike reading
/// the command line, which needs `PROCESS_VM_READ`.
#[cfg(windows)]
fn process_image_path(pid: u32) -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;

    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };

    // SAFETY: the process id comes from the system process list; a null
    // handle is a valid failure result and is checked below.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return None;
    }

    // Long paths may exceed MAX_PATH; 32768 UTF-16 code units is the API cap.
    let mut buffer = vec![0u16; 32_768];
    let mut length = buffer.len() as u32;
    // SAFETY: `buffer` is writable for `length` UTF-16 code units and the
    // handle was opened with the required access right.
    let ok = unsafe {
        QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, buffer.as_mut_ptr(), &mut length)
    };
    // SAFETY: `handle` came from OpenProcess and is closed exactly once.
    unsafe { CloseHandle(handle) };

    if ok == 0 || length == 0 {
        return None;
    }

    buffer.truncate(length as usize);
    Some(PathBuf::from(std::ffi::OsString::from_wide(&buffer)))
}

#[cfg(not(windows))]
fn process_image_path(_pid: u32) -> Option<PathBuf> {
    None
}

fn is_league_client_process(name: &OsStr) -> bool {
    let name = name.to_string_lossy().to_ascii_lowercase();
    let file_name = name.rsplit(['\\', '/']).next().unwrap_or(name.as_str());
    file_name == "leagueclientux.exe" || file_name == "leagueclientux"
}

fn parse_port(value: &str) -> Option<u16> {
    value
        .trim()
        .trim_matches('"')
        .parse::<u16>()
        .ok()
        .filter(|port| *port > 0)
}

fn clean_token(value: &str) -> Option<String> {
    let token = value.trim().trim_matches('"');
    (!token.is_empty()).then(|| token.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_standard_league_client_arguments() {
        let args = [
            r"C:\Riot Games\League of Legends\LeagueClientUx.exe",
            "--app-port=54321",
            "--remoting-auth-token=abc123",
            "--locale=zh_CN",
        ];

        let credentials = parse_lcu_args(args).expect("credentials");
        assert_eq!(credentials.port, 54321);
        assert_eq!(credentials.token, "abc123");
    }

    #[test]
    fn parses_space_separated_arguments() {
        let args = ["--app-port", "12345", "--remoting-auth-token", "token"];
        let credentials = parse_lcu_args(args).expect("credentials");

        assert_eq!(credentials.port, 12345);
        assert_eq!(credentials.token, "token");
    }

    #[test]
    fn rejects_missing_or_invalid_values() {
        assert!(parse_lcu_args(["--app-port=0", "--remoting-auth-token=x"]).is_none());
        assert!(parse_lcu_args(["--app-port=1234"]).is_none());
        assert!(parse_lcu_args(["--remoting-auth-token=x"]).is_none());
        assert!(parse_lcu_args(["--app-port=abc", "--remoting-auth-token=x"]).is_none());
    }

    #[test]
    fn parses_lockfile() {
        let credentials =
            parse_lockfile("LeagueClient:1234:54321:secret-token:https").expect("credentials");

        assert_eq!(credentials.port, 54321);
        assert_eq!(credentials.token, "secret-token");
    }

    #[test]
    fn rejects_bad_lockfile() {
        assert!(parse_lockfile("too:short").is_none());
        assert!(parse_lockfile("LeagueClient:1234:0:token:https").is_none());
        assert!(parse_lockfile("LeagueClient:1234:54321::https").is_none());
        // The CN client leaves an empty file behind.
        assert!(parse_lockfile("").is_none());
    }

    #[test]
    fn recognizes_client_process_names() {
        assert!(is_league_client_process(OsStr::new("LeagueClientUx.exe")));
        assert!(is_league_client_process(OsStr::new(
            r"C:\Riot Games\LeagueClientUx.exe"
        )));
        assert!(!is_league_client_process(OsStr::new("LeagueClient.exe")));
        assert!(!is_league_client_process(OsStr::new(
            "LeagueClientUxRender.exe"
        )));
    }

    #[test]
    fn debug_output_hides_token() {
        let credentials = LcuCredentials {
            port: 1234,
            token: "super-secret".to_owned(),
        };

        let debug = format!("{credentials:?}");
        assert!(debug.contains("1234"));
        assert!(!debug.contains("super-secret"));
    }

    #[test]
    fn reads_arguments_from_client_log_line() {
        // Real CN client log line (`--region=TENCENT`).
        let text = "000000.000|   OKAY| Command line arguments: \
                    --riotclient-auth-token=other --riotclient-app-port=99999 \
                    --region=TENCENT --remoting-auth-token=lK_KN5XQI4F4kDPt-8Am-A \
                    --app-port=50414 --app-pid=15424\r\n";

        assert_eq!(argument_value(text, "--app-port="), Some("50414"));
        assert_eq!(
            argument_value(text, "--remoting-auth-token="),
            Some("lK_KN5XQI4F4kDPt-8Am-A")
        );
        // Riot Client arguments must not be mistaken for the LCU ones.
        assert_eq!(
            argument_value(text, "--riotclient-app-port="),
            Some("99999")
        );
    }

    #[test]
    fn parses_credentials_from_ux_log() {
        let directory = tempfile::tempdir().expect("temp dir");
        let log = directory
            .path()
            .join("2026-11-07T20-00-00_1234_LeagueClientUx.log");
        std::fs::write(
            &log,
            "000000.000|   OKAY| Command line arguments: --region=TENCENT \
             --remoting-auth-token=abc-DEF_123 --app-port=50414\r\n",
        )
        .expect("write log");

        let credentials = credentials_from_log_directory(directory.path()).expect("credentials");
        assert_eq!(credentials.port, 50414);
        assert_eq!(credentials.token, "abc-DEF_123");
    }

    #[test]
    fn strips_nul_bytes_from_client_logs() {
        let directory = tempfile::tempdir().expect("temp dir");
        let log = directory.path().join("session_LeagueClientUx.log");
        let mut contents = b"--app-port=1234 --remoting-auth-token=abc-DEF_123\r\n".to_vec();
        contents.extend_from_slice(b"\0trailing\0binary\0junk\0");
        std::fs::write(&log, contents).expect("write log");

        let credentials = credentials_from_log_directory(directory.path()).expect("credentials");
        assert_eq!(credentials.port, 1234);
        assert_eq!(credentials.token, "abc-DEF_123");
    }

    #[test]
    fn finds_credentials_in_cn_client_log_layout() {
        let install = tempfile::tempdir().expect("temp dir");
        let logs = install
            .path()
            .join("Game")
            .join("Logs")
            .join("LeagueClient Logs");
        std::fs::create_dir_all(&logs).expect("log dir");
        std::fs::write(
            logs.join("2026-11-07T20-00-00_1_LeagueClientUx.log"),
            "--app-port=50414 --remoting-auth-token=abc-DEF_123",
        )
        .expect("write log");

        let credentials =
            credentials_from_logs(&[install.path().to_path_buf()]).expect("credentials");
        assert_eq!(credentials.port, 50414);
        assert_eq!(credentials.token, "abc-DEF_123");
    }

    #[test]
    fn ignores_non_ux_logs() {
        let directory = tempfile::tempdir().expect("temp dir");
        std::fs::write(
            directory.path().join("x_LeagueClientUxRender.log"),
            "--app-port=1234 --remoting-auth-token=abc",
        )
        .expect("write log");

        assert!(credentials_from_log_directory(directory.path()).is_none());
    }

    #[test]
    fn skips_logs_without_credentials() {
        let directory = tempfile::tempdir().expect("temp dir");
        std::fs::write(
            directory.path().join("empty_LeagueClientUx.log"),
            "no launch arguments here",
        )
        .expect("write log");

        assert!(credentials_from_log_directory(directory.path()).is_none());
    }

    // Windows path semantics; the discovery code only runs on Windows.
    #[cfg(windows)]
    #[test]
    fn install_roots_include_executable_directories() {
        let processes = vec![LeagueProcess {
            pid: 1,
            exe: Some(PathBuf::from(
                r"D:\Games\英雄联盟\LeagueClient\LeagueClientUx.exe",
            )),
            args: Vec::new(),
        }];

        let roots = install_roots(&processes);
        assert!(roots.contains(&PathBuf::from(r"D:\Games\英雄联盟\LeagueClient")));
        assert!(roots.contains(&PathBuf::from(r"D:\Games\英雄联盟")));
    }
}
