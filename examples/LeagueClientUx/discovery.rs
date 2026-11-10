//! Scoped `lockfile` and client-log discovery writers (ADR-0007).
//!
//! Every filesystem write is bounded: an absolute, already existing install
//! root, no UNC paths, no `..`, no symlinks, and only the two documented
//! output shapes. Files are owner-only on Unix and removed when the returned
//! [`DiscoveryFile`] is dropped.

use std::path::{Component, Path, PathBuf};

use super::cli::{is_valid_token, CliError};

/// Install prefixes belonging to a real Riot/Tencent/WeGame installation.
///
/// This mirrors the app's `COMMON_INSTALL_ROOTS`; the mock must never write
/// into one of them. Paths are compared after separator and case folding.
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
        // Best effort: a missing file is already the desired state.
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Writes `<root>/lockfile` as exactly
/// `LeagueClientUx:<pid>:<port>:<token>:https`.
pub fn write_lockfile(root: &Path, port: u16, token: &str) -> Result<DiscoveryFile, CliError> {
    let root = validate_install_root(root)?;
    require_discovery_token(token)?;

    let path = root.join("lockfile");
    ensure_not_symlink(&path)?;
    let contents = format!("LeagueClientUx:{}:{port}:{token}:https", std::process::id());
    write_private_file(&path, contents.as_bytes())?;

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

    std::fs::create_dir_all(&directory).map_err(|error| {
        CliError::runtime(format!("无法创建日志目录 {}：{error}", directory.display()))
    })?;

    // The created directory must still be a real directory (not a symlink
    // planted by a race) before the file is written.
    ensure_not_symlink(&directory)?;

    std::fs::metadata(&directory)
        .map_err(|error| CliError::runtime(format!("无法检查日志目录：{error}")))
        .and_then(|metadata| {
            if metadata.is_dir() {
                Ok(())
            } else {
                Err(CliError::runtime("日志目录不是目录"))
            }
        })?;

    let path = directory.join(format!("{}_LeagueClientUx.log", log_file_stem(&directory)));
    ensure_not_symlink(&path)?;

    let contents = format!(
        "000000.000|   OKAY| Command line arguments: --app-port={port} \
         --remoting-auth-token={token} --app-pid={} --region=MOCK\r\n",
        std::process::id()
    );
    write_private_file(&path, contents.as_bytes())?;

    Ok(DiscoveryFile { path })
}

/// Builds the `<timestamp>` part of the log file name.
///
/// Unix seconds are enough to look like the client's timestamp. An extremely
/// unlikely same-second collision in one root gets a numeric suffix so an
/// existing log is never truncated.
fn log_file_stem(directory: &Path) -> String {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let stem = stamp.to_string();

    let mut candidate = stem.clone();
    let mut suffix = 1u32;
    while directory
        .join(format!("{candidate}_LeagueClientUx.log"))
        .exists()
    {
        candidate = format!("{stem}-{suffix}");
        suffix = suffix.saturating_add(1);
    }
    candidate
}

fn require_discovery_token(token: &str) -> Result<(), CliError> {
    if !is_valid_token(token) {
        return Err(CliError::usage(
            "发现文件令牌必须为 1..=256 字节且仅包含 [A-Za-z0-9_=-]",
        ));
    }
    Ok(())
}

/// Validates the write root before any filesystem mutation (ADR-0007).
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

    // Compare Windows install prefixes on the folded text form so the check
    // also fires when a Windows path is supplied on a non-Windows host.
    let folded = raw.replace('/', "\\").to_ascii_lowercase();
    if FORBIDDEN_INSTALL_PREFIXES
        .iter()
        .any(|prefix| folded.starts_with(prefix))
    {
        return Err(CliError::usage(format!(
            "--install-root 指向真实的英雄联盟安装目录，拒绝写入：{}",
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

    ensure_not_symlink(root)?;
    let metadata = std::fs::metadata(root).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            CliError::usage(format!("--install-root 目录不存在：{}", root.display()))
        } else {
            CliError::runtime(format!("无法检查 --install-root：{error}"))
        }
    })?;
    if !metadata.is_dir() {
        return Err(CliError::usage(format!(
            "--install-root 不是目录：{}",
            root.display()
        )));
    }

    reject_real_install_markers(root)?;

    Ok(root.to_path_buf())
}

/// Refuses roots that are plainly a real League installation.
fn reject_real_install_markers(root: &Path) -> Result<(), CliError> {
    if root.join("LeagueClient.exe").exists() {
        return Err(CliError::usage(format!(
            "--install-root 包含 LeagueClient.exe，拒绝写入真实安装目录：{}",
            root.display()
        )));
    }

    let ux = root.join("LeagueClientUx.exe");
    if ux.exists() && !is_own_executable(&ux) {
        return Err(CliError::usage(format!(
            "--install-root 包含非 mock 自身的 LeagueClientUx.exe：{}",
            root.display()
        )));
    }

    Ok(())
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
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(CliError::runtime(format!(
            "无法检查路径 {}：{error}",
            path.display()
        ))),
    }
}

fn write_private_file(path: &Path, contents: &[u8]) -> Result<(), CliError> {
    write_private_file_inner(path, contents)
        .map_err(|error| CliError::runtime(format!("无法写入 {}：{error}", path.display())))
}

#[cfg(unix)]
fn write_private_file_inner(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    // `mode` only applies on creation; tighten a pre-existing file too.
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.write_all(contents)?;
    file.flush()
}

#[cfg(not(unix))]
fn write_private_file_inner(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    // Windows keeps the default user ACL of the destination directory.
    std::fs::write(path, contents)
}
