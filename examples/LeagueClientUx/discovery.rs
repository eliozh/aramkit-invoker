//! Scoped `lockfile` and client-log discovery writers (ADR-0007).
//!
//! Every filesystem write is bounded: an absolute, already existing install
//! root, no UNC paths, no `..`, no symlinks, and only the two documented
//! output shapes. The root is canonicalized first and every subsequent write
//! uses that canonical path, so a symlinked ancestor is either resolved into a
//! safe real path or rejected. Files are owner-only on Unix and removed when
//! the returned [`DiscoveryFile`] is dropped.

use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

use super::cli::{is_valid_token, CliError};

/// Install prefixes belonging to a real Riot/Tencent/WeGame installation.
///
/// This mirrors the app's `COMMON_INSTALL_ROOTS`; the mock must never write
/// into one of them. Both the canonicalized prefix and its folded text form
/// are compared against the canonicalized root.
const FORBIDDEN_INSTALL_PREFIXES: &[&str] = &[
    r"c:\riot games\",
    r"d:\riot games\",
    r"e:\riot games\",
    r"f:\riot games\",
    r"c:\program files\riot games\",
    r"d:\program files\riot games\",
    r"c:\program files\tencent\",
    r"d:\program files\tencent\",
    r"c:\program files\wegame\",
    r"d:\wegameapps\",
    r"d:\wegame\",
    r"e:\wegame\",
    r"d:\腾讯游戏\",
    r"e:\腾讯游戏\",
];

/// Timestamped log names are retried at most this many times before giving up.
const MAX_LOG_NAME_ATTEMPTS: u32 = 1000;

/// A discovery file created by the mock. It is deleted when dropped, so a
/// clean stop (including an early error return after the write) removes it.
#[derive(Debug)]
pub struct DiscoveryFile {
    path: PathBuf,
}

impl DiscoveryFile {
    /// The absolute path of the file that was written.
    #[allow(dead_code)]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for DiscoveryFile {
    fn drop(&mut self) {
        match std::fs::remove_file(&self.path) {
            // A missing file is already the desired state.
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            // Surface a failed cleanup (permissions, read-only root) instead of
            // silently leaving a token-bearing file behind. The path carries
            // no credential.
            Err(error) => eprintln!(
                "mock-lcu: 无法删除发现文件 {}：{error}",
                self.path.display()
            ),
        }
    }
}

/// Writes `<root>/lockfile` as exactly
/// `LeagueClientUx:<pid>:<port>:<token>:https`.
pub fn write_lockfile(root: &Path, port: u16, token: &str) -> Result<DiscoveryFile, CliError> {
    let root = validate_install_root(root)?;
    require_discovery_token(token)?;

    let path = root.join("lockfile");
    // A pre-existing leaf that is not a regular file is rejected both here and
    // again atomically by the `O_NOFOLLOW` open below.
    reject_non_regular(&path)?;
    let contents = format!("LeagueClientUx:{}:{port}:{token}:https", std::process::id());
    write_lockfile_contents(&path, contents.as_bytes())?;

    Ok(DiscoveryFile { path })
}

/// Writes `<root>/Game/Logs/LeagueClient Logs/<timestamp>_LeagueClientUx.log`.
///
/// The first bytes contain the same `--app-port=<p> --remoting-auth-token=<t>`
/// pair the real client logs, so the app's log parser can read it.
pub fn write_log(root: &Path, port: u16, token: &str) -> Result<DiscoveryFile, CliError> {
    let root = validate_install_root(root)?;
    require_discovery_token(token)?;

    // Reject a symlinked ancestor before creating or writing anything.
    let game = root.join("Game");
    let logs = game.join("Logs");
    let directory = logs.join("LeagueClient Logs");
    ensure_not_symlink(&game)?;
    ensure_not_symlink(&logs)?;
    ensure_not_symlink(&directory)?;

    create_private_dirs(&directory).map_err(|error| {
        CliError::runtime(format!("无法创建日志目录 {}：{error}", directory.display()))
    })?;

    // The created directory must still be a real directory (not a symlink
    // planted by a race) before the file is written.
    ensure_not_symlink(&directory)?;
    let metadata = std::fs::symlink_metadata(&directory)
        .map_err(|error| CliError::runtime(format!("无法检查日志目录：{error}")))?;
    if !metadata.is_dir() {
        return Err(CliError::runtime("日志目录不是目录"));
    }

    let contents = format!(
        "000000.000|   OKAY| Command line arguments: --app-port={port} \
         --remoting-auth-token={token} --app-pid={} --region=MOCK\r\n",
        std::process::id()
    );

    // `create_new` never truncates an existing log. A same-second collision
    // (or a pre-created name) gets a numeric suffix.
    let stem = timestamp_stem();
    for attempt in 0..MAX_LOG_NAME_ATTEMPTS {
        let name = if attempt == 0 {
            format!("{stem}_LeagueClientUx.log")
        } else {
            format!("{stem}-{attempt}_LeagueClientUx.log")
        };
        let path = directory.join(name);
        match create_log_file_inner(&path, contents.as_bytes()) {
            Ok(()) => return Ok(DiscoveryFile { path }),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(CliError::runtime(format!(
                    "无法写入 {}：{error}",
                    path.display()
                )))
            }
        }
    }

    Err(CliError::runtime("无法为日志发现文件找到唯一文件名"))
}

/// Builds the `<timestamp>` part of the log file name.
///
/// Unix seconds are enough to look like the client's timestamp.
fn timestamp_stem() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
        .to_string()
}

fn require_discovery_token(token: &str) -> Result<(), CliError> {
    if !is_valid_token(token) {
        return Err(CliError::usage(
            "发现文件令牌必须为 1..=256 字节且仅包含 [A-Za-z0-9_=-]",
        ));
    }
    Ok(())
}

/// Validates the write root and returns its canonical path.
///
/// The raw input is only used for the syntactic checks; all writes and all
/// real-install comparisons use the canonical path, so a symlinked ancestor
/// cannot redirect a write outside the intended tree (ADR-0007).
fn validate_install_root(root: &Path) -> Result<PathBuf, CliError> {
    if root.as_os_str().is_empty() {
        return Err(CliError::usage("--install-root 不能为空"));
    }

    let raw = root.to_string_lossy();
    if raw.starts_with(r"\\") || raw.starts_with("//") {
        return Err(CliError::usage(format!(
            "--install-root 不允许 UNC 路径：{}",
            root.display()
        )));
    }

    if !root.is_absolute() {
        return Err(CliError::usage(format!(
            "--install-root 必须是绝对路径：{}",
            root.display()
        )));
    }

    if root
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(CliError::usage(format!(
            "--install-root 不允许包含 ..：{}",
            root.display()
        )));
    }

    // The chosen root's own entry may not be a symlink. Symlinked ancestors are
    // resolved by `canonicalize` below instead.
    ensure_not_symlink(root)?;

    let canonical = std::fs::canonicalize(root).map_err(|error| {
        if error.kind() == ErrorKind::NotFound {
            CliError::usage(format!("--install-root 目录不存在：{}", root.display()))
        } else {
            CliError::runtime(format!("无法检查 --install-root：{error}"))
        }
    })?;

    let metadata = std::fs::metadata(&canonical)
        .map_err(|error| CliError::runtime(format!("无法检查 --install-root：{error}")))?;
    if !metadata.is_dir() {
        return Err(CliError::usage(format!(
            "--install-root 不是目录：{}",
            root.display()
        )));
    }

    if is_forbidden_install(&canonical) {
        return Err(CliError::usage(format!(
            "--install-root 指向真实的英雄联盟安装目录，拒绝写入：{}",
            root.display()
        )));
    }

    reject_real_install_markers(&canonical)?;

    Ok(canonical)
}

/// True when the canonical root is equal to or inside a known install prefix.
fn is_forbidden_install(canonical_root: &Path) -> bool {
    let candidate = normalized_for_match(canonical_root);
    FORBIDDEN_INSTALL_PREFIXES.iter().any(|prefix| {
        // Compare canonical prefixes when they exist on this host; the folded
        // text form remains as a fallback for other-platform paths.
        let canonical_match = std::fs::canonicalize(prefix.trim_end_matches('\\'))
            .map(|canonical| prefix_matches(&normalized_for_match(&canonical), &candidate))
            .unwrap_or(false);
        canonical_match || prefix_matches(prefix, &candidate)
    })
}

/// Folds a path into a comparable form: lowercase, `\` separators, no
/// extended-length prefix, no trailing separators, and (on Windows) no
/// trailing dots/spaces in any component.
fn normalized_for_match(path: &Path) -> String {
    let text = path
        .to_string_lossy()
        .replace('/', "\\")
        .to_ascii_lowercase();
    let text = text.strip_prefix(r"\\?\").unwrap_or(text.as_str());
    let mut parts: Vec<String> = text.split('\\').map(normalize_component).collect();
    while parts.last().is_some_and(|part| part.is_empty()) {
        parts.pop();
    }
    parts.join("\\")
}

#[cfg(windows)]
fn normalize_component(component: &str) -> String {
    // Windows silently ignores trailing dots and spaces in a path component;
    // fold them away so `C:\Riot Games. ` still matches the prefix.
    component.trim_end_matches(['.', ' ']).to_owned()
}

#[cfg(not(windows))]
fn normalize_component(component: &str) -> String {
    component.to_owned()
}

fn prefix_matches(prefix: &str, candidate: &str) -> bool {
    let prefix = prefix.trim_end_matches('\\');
    candidate == prefix || candidate.starts_with(&format!("{prefix}\\"))
}

/// Refuses roots whose canonical path or any canonical ancestor is plainly a
/// real League installation.
fn reject_real_install_markers(canonical_root: &Path) -> Result<(), CliError> {
    for ancestor in canonical_root.ancestors() {
        if marker_present(&ancestor.join("LeagueClient.exe")) {
            return Err(CliError::usage(format!(
                "--install-root 的上级目录包含 LeagueClient.exe，拒绝写入真实安装目录：{}",
                ancestor.display()
            )));
        }

        let ux = ancestor.join("LeagueClientUx.exe");
        if marker_present(&ux) && !is_own_executable(&ux) {
            return Err(CliError::usage(format!(
                "--install-root 的上级目录包含非 mock 自身的 LeagueClientUx.exe：{}",
                ancestor.display()
            )));
        }
    }

    Ok(())
}

/// Marker detection must not follow links: a dangling or planted symlink with
/// the marker name still counts as a real-install marker.
fn marker_present(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// True when `marker` is this process's own executable.
fn is_own_executable(marker: &Path) -> bool {
    let Ok(current) = std::env::current_exe() else {
        return false;
    };
    match (
        std::fs::canonicalize(marker),
        std::fs::canonicalize(current),
    ) {
        (Ok(marker), Ok(current)) => marker == current,
        _ => false,
    }
}

/// Rejects a path whose own entry is a symbolic link.
fn ensure_not_symlink(path: &Path) -> Result<(), CliError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(CliError::usage(format!(
            "--install-root 不允许符号链接：{}",
            path.display()
        ))),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(CliError::runtime(format!(
            "无法检查路径 {}：{error}",
            path.display()
        ))),
    }
}

/// Rejects an existing leaf that is not a regular file (symlink, directory,
/// FIFO, device). The atomic `O_NOFOLLOW` open closes the race on Unix.
fn reject_non_regular(path: &Path) -> Result<(), CliError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(()),
        Ok(_) => Err(CliError::usage(format!(
            "发现文件路径不是普通文件：{}",
            path.display()
        ))),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(CliError::runtime(format!(
            "无法检查路径 {}：{error}",
            path.display()
        ))),
    }
}

/// Creates the log directory hierarchy with owner-only permissions on Unix.
fn create_private_dirs(directory: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(directory)
}

fn write_lockfile_contents(path: &Path, contents: &[u8]) -> Result<(), CliError> {
    write_lockfile_inner(path, contents)
        .map_err(|error| CliError::runtime(format!("无法写入 {}：{error}", path.display())))
}

#[cfg(unix)]
fn write_lockfile_inner(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        // Never follow a symlink at the final component; close-on-exec keeps
        // the descriptor out of any spawned child.
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    if !file.metadata()?.file_type().is_file() {
        return Err(std::io::Error::new(
            ErrorKind::InvalidInput,
            "发现文件不是普通文件",
        ));
    }
    // `mode` only applies on creation; tighten a pre-existing file too.
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.write_all(contents)?;
    file.flush()
}

#[cfg(not(unix))]
fn write_lockfile_inner(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    // Windows keeps the default user ACL of the destination directory.
    std::fs::write(path, contents)
}

#[cfg(unix)]
fn create_log_file_inner(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt;

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC)
        .open(path)?;
    file.write_all(contents)?;
    file.flush()
}

#[cfg(not(unix))]
fn create_log_file_inner(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(contents)?;
    file.flush()
}
