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
            // no credential. Skipped while unwinding: `eprintln!` can itself
            // panic (for example when stderr is closed), and a panic in a
            // drop during unwinding aborts the process.
            Err(error) if !std::thread::panicking() => eprintln!(
                "mock-lcu: 无法删除发现文件 {}：{error}",
                self.path.display()
            ),
            Err(_) => {}
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

    // Every component created between the canonical root and the log directory
    // must still be a real directory: `create_dir_all` can be raced, so a
    // symlinked intermediate is rejected, not just the leaf.
    reject_symlinked_components(&root, &directory)?;
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
        // On Unix any local user can plant a marker in a world-writable
        // directory (notably the system temp dir), so such a marker is not
        // evidence of a real install and must not deny the run. The chosen
        // root itself is always checked.
        #[cfg(unix)]
        if ancestor != canonical_root && is_world_writable(ancestor) {
            continue;
        }

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

/// True when any local user may create entries in `directory` on Unix.
///
/// Markers found there are attacker-plantable and therefore ignored (see
/// [`reject_real_install_markers`]).
#[cfg(unix)]
fn is_world_writable(directory: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    std::fs::symlink_metadata(directory)
        .map(|metadata| metadata.permissions().mode() & 0o002 != 0)
        .unwrap_or(false)
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

/// Re-checks every component between the canonical root and `directory`.
///
/// The pre-creation checks can be raced: `create_dir_all` may walk a component
/// that is replaced by a symlink in between. Walking the created path again
/// rejects a symlinked intermediate, not only the leaf directory.
fn reject_symlinked_components(root: &Path, directory: &Path) -> Result<(), CliError> {
    let relative = directory.strip_prefix(root).map_err(|_| {
        CliError::runtime(format!(
            "日志目录不在 --install-root 内：{}",
            directory.display()
        ))
    })?;

    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component);
        ensure_not_symlink(&current)?;
    }
    Ok(())
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
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        // Deliberately not truncated yet: a hardlink to an external file
        // must be rejected before its contents are destroyed.
        .mode(0o600)
        // Never follow a symlink at the final component; close-on-exec keeps
        // the descriptor out of any spawned child.
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() {
        return Err(std::io::Error::new(
            ErrorKind::InvalidInput,
            "发现文件不是普通文件",
        ));
    }
    // `O_NOFOLLOW` does not stop a hardlink, which is still a regular file.
    // A hardlink to an outside file would let the write escape the root, so
    // reject it before truncating.
    if metadata.nlink() > 1 {
        return Err(std::io::Error::new(
            ErrorKind::InvalidInput,
            "发现文件存在额外硬链接",
        ));
    }
    // `mode` only applies on creation; tighten a pre-existing file too.
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.set_len(0)?;
    write_contents_or_remove(&mut file, path, contents)
}

/// Prepares a destination that is about to be opened with `create_new` on
/// platforms without `O_NOFOLLOW`: reject a symlink leaf and any other
/// non-regular entry, and remove a stale regular file.
///
/// Compiled in tests on Unix as well so this Windows-only hardening is
/// exercised on the CI host.
#[cfg(any(not(unix), test))]
fn prepare_new_lockfile_leaf(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(std::io::Error::new(
            ErrorKind::InvalidInput,
            "发现文件不是普通文件",
        )),
        Ok(metadata) if metadata.file_type().is_file() => std::fs::remove_file(path),
        Ok(_) => Err(std::io::Error::new(
            ErrorKind::InvalidInput,
            "发现文件不是普通文件",
        )),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(not(unix))]
fn write_lockfile_inner(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    // Windows has no `O_NOFOLLOW`; check the leaf without following it, drop a
    // stale regular file, then create exclusively so a pre-existing leaf can
    // never be truncated or written through.
    prepare_new_lockfile_leaf(path)?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    write_contents_or_remove(&mut file, path, contents)
}

/// Writes and flushes `contents`, removing the created file when either step
/// fails so a truncated or zero-byte lockfile is not left behind.
fn write_contents_or_remove(
    file: &mut impl std::io::Write,
    path: &Path,
    contents: &[u8],
) -> std::io::Result<()> {
    if let Err(error) = file.write_all(contents).and_then(|()| file.flush()) {
        let _ = std::fs::remove_file(path);
        return Err(error);
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A writer that always fails so the partial-file cleanup path is
    /// exercised without a real full disk.
    struct FailingWriter;

    impl std::io::Write for FailingWriter {
        fn write(&mut self, _buffer: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("injected write failure"))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn removes_partial_lockfile_when_write_fails() {
        let root = tempfile::tempdir().expect("temp dir");
        let path = root.path().join("lockfile");
        std::fs::write(&path, b"").expect("pre-create");

        let error = write_contents_or_remove(&mut FailingWriter, &path, b"payload")
            .expect_err("the injected write must fail");
        assert_eq!(error.kind(), ErrorKind::Other);
        assert!(
            !path.exists(),
            "a partially written lockfile must be removed"
        );
    }

    #[test]
    fn stale_regular_lockfile_leaf_is_removed() {
        let root = tempfile::tempdir().expect("temp dir");
        let path = root.path().join("lockfile");
        std::fs::write(&path, b"stale").expect("stale leaf");

        prepare_new_lockfile_leaf(&path).expect("a stale regular leaf may be replaced");
        assert!(!path.exists(), "the stale leaf must be removed");
    }

    #[test]
    fn accepts_clean_directory_components() {
        let root = tempfile::tempdir().expect("temp dir");
        let directory = root
            .path()
            .join("Game")
            .join("Logs")
            .join("LeagueClient Logs");
        std::fs::create_dir_all(&directory).expect("dirs");

        reject_symlinked_components(root.path(), &directory).expect("a clean tree is accepted");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_leaf_before_create_new() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("temp dir");
        let outside = tempfile::tempdir().expect("outside");
        let target = outside.path().join("target");
        std::fs::write(&target, b"untouched").expect("target");
        let leaf = root.path().join("lockfile");
        symlink(&target, &leaf).expect("leaf symlink");

        let error = prepare_new_lockfile_leaf(&leaf).expect_err("a symlink leaf is rejected");
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(
            std::fs::read(&target).expect("target read"),
            b"untouched",
            "the symlink target must not be touched"
        );
        assert!(std::fs::symlink_metadata(&leaf)
            .expect("leaf metadata")
            .file_type()
            .is_symlink());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_intermediate_component() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("temp dir");
        let outside = tempfile::tempdir().expect("outside");
        let game = root.path().join("Game");
        std::fs::create_dir(&game).expect("game dir");
        symlink(outside.path(), game.join("Logs")).expect("logs symlink");
        let directory = game.join("Logs").join("LeagueClient Logs");

        let error = reject_symlinked_components(root.path(), &directory)
            .expect_err("a symlinked intermediate component must be rejected");
        assert_eq!(error.exit_code(), 2, "{error}");
    }
}
