//! Install and uninstall the `kumu` CLI symlink on the user's `PATH`.
//!
//! Kumu bundles its CLI inside the app binary itself (`Kumu.app/Contents/MacOS/kumu`).
//! Drag-to-Applications `.dmg` installs never touch `/usr/local/bin` or
//! `/opt/homebrew/bin`, so this module lets users link (or unlink) `kumu` into
//! a standard directory on their `PATH` with one click from Settings, the menu
//! bar, or the command palette.

use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

/// Current installation state of the `kumu` CLI symlink on `PATH`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CliInstallStatus {
    /// `link_path` resolves to the currently running `kumu` executable.
    Installed { link_path: PathBuf },
    /// `link_path` is a Kumu symlink pointing to a different / stale path.
    NeedsUpdate {
        link_path: PathBuf,
        current_target: PathBuf,
    },
    /// A non-Kumu file or foreign symlink already occupies `path`.
    Conflict { path: PathBuf },
    /// No `kumu` binary was found; installing will link into `target_dir`.
    NotInstalled {
        target_dir: PathBuf,
        requires_admin: bool,
    },
}

/// Failure modes for installing or uninstalling the `kumu` CLI symlink.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CliInstallError {
    /// The user dismissed the macOS administrator authorization dialog.
    Canceled,
    /// A non-Kumu file already exists at the target path.
    Conflict(PathBuf),
    /// Any other filesystem or command error.
    Io(String),
}

impl fmt::Display for CliInstallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Canceled => write!(f, "已取消授权"),
            Self::Conflict(path) => {
                write!(f, "{} 已被其他程序占用，未覆盖", path.display())
            }
            Self::Io(message) => write!(f, "{message}"),
        }
    }
}

/// Probe the current `kumu` CLI installation status using the running binary
/// and the user's `PATH`.
pub fn status() -> CliInstallStatus {
    let Ok(exe) = current_executable() else {
        return CliInstallStatus::NotInstalled {
            target_dir: PathBuf::from("/usr/local/bin"),
            requires_admin: !dir_is_writable(Path::new("/usr/local/bin")),
        };
    };
    let path_dirs = effective_path_dirs();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    probe_status(&exe, &path_dirs, home.as_deref())
}

/// Install or update the `kumu` CLI symlink, returning the symlink path.
pub fn install() -> Result<PathBuf, CliInstallError> {
    let exe = current_executable()
        .map_err(|err| CliInstallError::Io(format!("无法确定 Kumu 可执行文件路径：{err}")))?;
    let path_dirs = effective_path_dirs();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    install_for(&exe, &path_dirs, home.as_deref(), run_privileged_shell)
}

/// Remove an existing Kumu-managed CLI symlink, returning the removed path.
pub fn uninstall() -> Result<PathBuf, CliInstallError> {
    let exe = current_executable()
        .map_err(|err| CliInstallError::Io(format!("无法确定 Kumu 可执行文件路径：{err}")))?;
    let path_dirs = effective_path_dirs();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    uninstall_for(&exe, &path_dirs, home.as_deref(), run_privileged_shell)
}

fn current_executable() -> std::io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    Ok(fs::canonicalize(&exe).unwrap_or(exe))
}

fn effective_path_dirs() -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let mut dirs = Vec::new();
    let sources = [
        crate::term::shell_env::captured_path(),
        std::env::var_os("PATH"),
    ];
    for raw in sources.into_iter().flatten() {
        for dir in std::env::split_paths(&raw) {
            if !dir.as_os_str().is_empty() && seen.insert(dir.clone()) {
                dirs.push(dir);
            }
        }
    }
    dirs
}

fn standard_dirs(home: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs = vec![
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
    ];
    if let Some(home) = home
        && !home.as_os_str().is_empty()
    {
        dirs.push(home.join(".local/bin"));
    }
    dirs
}

fn probe_status(
    current_exe: &Path,
    path_dirs: &[PathBuf],
    home: Option<&Path>,
) -> CliInstallStatus {
    // Follow PATH's actual lookup order so we report the `kumu` binary that
    // new terminals will resolve; only fall back to standard directories when
    // PATH is empty.
    let source_dirs: Vec<PathBuf> = if path_dirs.is_empty() {
        standard_dirs(home)
    } else {
        path_dirs.to_vec()
    };
    let mut scan_dirs = Vec::new();
    let mut seen = HashSet::new();
    for dir in source_dirs {
        if seen.insert(dir.clone()) {
            scan_dirs.push(dir);
        }
    }

    let mut first_needs_update: Option<(PathBuf, PathBuf)> = None;
    let mut first_conflict: Option<PathBuf> = None;

    for dir in &scan_dirs {
        let candidate = dir.join("kumu");
        let Ok(meta) = fs::symlink_metadata(&candidate) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            let resolved = resolve_symlink_target(&candidate);
            if same_executable(&resolved, current_exe) {
                return CliInstallStatus::Installed {
                    link_path: candidate,
                };
            }
            if looks_like_kumu(&resolved) {
                if first_needs_update.is_none() {
                    first_needs_update = Some((candidate, resolved));
                }
            } else if first_conflict.is_none() {
                first_conflict = Some(candidate);
            }
        } else if same_executable(&candidate, current_exe) {
            return CliInstallStatus::Installed {
                link_path: candidate,
            };
        } else if first_conflict.is_none() {
            first_conflict = Some(candidate);
        }
    }

    if let Some((link_path, current_target)) = first_needs_update {
        return CliInstallStatus::NeedsUpdate {
            link_path,
            current_target,
        };
    }

    if let Some(path) = first_conflict {
        return CliInstallStatus::Conflict { path };
    }

    let target_dir = pick_install_dir(path_dirs, home);
    let requires_admin = !dir_can_write_or_create(&target_dir);
    CliInstallStatus::NotInstalled {
        target_dir,
        requires_admin,
    }
}

/// Choose the best destination directory for a fresh install:
/// 1. `/opt/homebrew/bin` if it exists, is on `PATH`, and is writable by the user.
/// 2. `/usr/local/bin` if it exists, is on `PATH`, and is writable by the user.
/// 3. `$HOME/.local/bin` if it is on `PATH` and writable (or creatable).
/// 4. Fall back to `/usr/local/bin` (always on default macOS `PATH`, uses admin
///    authorization if not writable).
fn pick_install_dir(path_dirs: &[PathBuf], home: Option<&Path>) -> PathBuf {
    let on_path = |dir: &Path| path_dirs.iter().any(|entry| entry == dir);

    let homebrew = PathBuf::from("/opt/homebrew/bin");
    if homebrew.is_dir() && on_path(&homebrew) && dir_is_writable(&homebrew) {
        return homebrew;
    }

    let usr_local = PathBuf::from("/usr/local/bin");
    if usr_local.is_dir() && on_path(&usr_local) && dir_is_writable(&usr_local) {
        return usr_local;
    }

    if let Some(home) = home
        && !home.as_os_str().is_empty()
    {
        let local_bin = home.join(".local/bin");
        if on_path(&local_bin) && dir_can_write_or_create(&local_bin) {
            return local_bin;
        }
    }

    usr_local
}

/// Whether the current user can write into `dir`, or create it inside its
/// nearest existing ancestor directory without administrator privileges.
fn dir_can_write_or_create(dir: &Path) -> bool {
    if dir.is_dir() {
        return dir_is_writable(dir);
    }
    if dir.exists() {
        return false;
    }
    let mut cursor = dir.parent();
    while let Some(parent) = cursor {
        if parent.exists() {
            return parent.is_dir() && dir_is_writable(parent);
        }
        cursor = parent.parent();
    }
    false
}

fn dir_is_writable(dir: &Path) -> bool {
    if !dir.is_dir() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        unsafe extern "C" {
            fn access(path: *const std::ffi::c_char, mode: std::ffi::c_int) -> std::ffi::c_int;
        }
        const W_OK: std::ffi::c_int = 2;
        let Ok(cpath) = CString::new(dir.as_os_str().as_bytes()) else {
            return false;
        };
        unsafe { access(cpath.as_ptr(), W_OK) == 0 }
    }
    #[cfg(not(unix))]
    {
        fs::metadata(dir).is_ok_and(|meta| !meta.permissions().readonly())
    }
}

fn resolve_symlink_target(link_path: &Path) -> PathBuf {
    let Ok(raw) = fs::read_link(link_path) else {
        return link_path.to_path_buf();
    };
    if raw.is_absolute() {
        raw
    } else if let Some(parent) = link_path.parent() {
        parent.join(raw)
    } else {
        raw
    }
}

fn same_executable(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => false,
    }
}

fn looks_like_kumu(target: &Path) -> bool {
    target.ends_with("Kumu.app/Contents/MacOS/kumu")
}

fn install_for<F>(
    current_exe: &Path,
    path_dirs: &[PathBuf],
    home: Option<&Path>,
    privileged_runner: F,
) -> Result<PathBuf, CliInstallError>
where
    F: FnOnce(&str) -> Result<(), CliInstallError>,
{
    let link_path = match probe_status(current_exe, path_dirs, home) {
        CliInstallStatus::Installed { link_path } => return Ok(link_path),
        CliInstallStatus::Conflict { path } => return Err(CliInstallError::Conflict(path)),
        CliInstallStatus::NeedsUpdate { link_path, .. } => link_path,
        CliInstallStatus::NotInstalled { target_dir, .. } => target_dir.join("kumu"),
    };

    match link_direct(current_exe, &link_path) {
        Ok(()) => Ok(link_path),
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
            let Some(parent) = link_path.parent() else {
                return Err(CliInstallError::Io(format!(
                    "无效的目标路径：{}",
                    link_path.display()
                )));
            };
            let script = format!(
                "mkdir -p {} && ln -sfn {} {}",
                shell_quote(parent),
                shell_quote(current_exe),
                shell_quote(&link_path),
            );
            privileged_runner(&script)?;
            Ok(link_path)
        }
        Err(err) => Err(CliInstallError::Io(format!(
            "创建 {} 失败：{err}",
            link_path.display()
        ))),
    }
}

fn uninstall_for<F>(
    current_exe: &Path,
    path_dirs: &[PathBuf],
    home: Option<&Path>,
    privileged_runner: F,
) -> Result<PathBuf, CliInstallError>
where
    F: FnOnce(&str) -> Result<(), CliInstallError>,
{
    let link_path = match probe_status(current_exe, path_dirs, home) {
        CliInstallStatus::Installed { link_path }
        | CliInstallStatus::NeedsUpdate { link_path, .. } => link_path,
        CliInstallStatus::Conflict { path } => return Err(CliInstallError::Conflict(path)),
        CliInstallStatus::NotInstalled { .. } => {
            return Err(CliInstallError::Io("尚未安装 kumu 命令行工具".to_string()));
        }
    };

    let meta = fs::symlink_metadata(&link_path)
        .map_err(|err| CliInstallError::Io(format!("读取 {} 失败：{err}", link_path.display())))?;
    if !meta.file_type().is_symlink() {
        return Err(CliInstallError::Conflict(link_path));
    }

    match fs::remove_file(&link_path) {
        Ok(()) => Ok(link_path),
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
            let script = format!("rm -f {}", shell_quote(&link_path));
            privileged_runner(&script)?;
            Ok(link_path)
        }
        Err(err) => Err(CliInstallError::Io(format!(
            "移除 {} 失败：{err}",
            link_path.display()
        ))),
    }
}

fn link_direct(target: &Path, link_path: &Path) -> std::io::Result<()> {
    if let Some(parent) = link_path.parent()
        && !parent.exists()
    {
        fs::create_dir_all(parent)?;
    }
    if let Ok(meta) = fs::symlink_metadata(link_path) {
        if !meta.file_type().is_symlink() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "target path is not a symlink",
            ));
        }
        fs::remove_file(link_path)?;
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link_path)
    }
    #[cfg(not(unix))]
    {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "symlinks are only supported on Unix",
        ))
    }
}

fn shell_quote(path: &Path) -> String {
    let raw = path.to_string_lossy();
    let escaped = raw.replace('\'', "'\\''");
    format!("'{escaped}'")
}

fn run_privileged_shell(script: &str) -> Result<(), CliInstallError> {
    #[cfg(target_os = "macos")]
    {
        let escaped = script.replace('\\', "\\\\").replace('"', "\\\"");
        let apple_script = format!("do shell script \"{escaped}\" with administrator privileges");
        let output = std::process::Command::new("/usr/bin/osascript")
            .args(["-e", &apple_script])
            .output()
            .map_err(|err| CliInstallError::Io(format!("无法启动系统授权弹窗：{err}")))?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if stderr.contains("User canceled") || stderr.contains("-128") {
            return Err(CliInstallError::Canceled);
        }
        Err(CliInstallError::Io(if stderr.is_empty() {
            "管理员授权失败".to_string()
        } else {
            stderr
        }))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = script;
        Err(CliInstallError::Io(
            "没有写入目标目录的权限，请手动创建符号链接".to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kumu-cli-install-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn install_status_and_uninstall_round_trip() {
        let root = temp_dir("roundtrip");
        let home = root.join("home");
        fs::create_dir_all(&home).expect("create home");
        // Intentionally leave `~/.local/bin` uncreated to verify requires_admin
        // is false when its parent directory is writable.
        let local_bin = home.join(".local/bin");

        let exe = root.join("Kumu.app/Contents/MacOS/kumu");
        fs::create_dir_all(exe.parent().unwrap()).expect("create app dir");
        fs::write(&exe, b"kumu-bin").expect("write fake exe");
        let exe = fs::canonicalize(&exe).expect("canonical exe");

        let path_dirs = vec![local_bin.clone()];

        // Initially not installed; picks creatable ~/.local/bin without admin.
        assert_eq!(
            probe_status(&exe, &path_dirs, Some(&home)),
            CliInstallStatus::NotInstalled {
                target_dir: local_bin.clone(),
                requires_admin: false,
            }
        );

        // Install creates directory + symlink and status becomes Installed.
        let installed = install_for(&exe, &path_dirs, Some(&home), |_| {
            panic!("should not require privilege")
        })
        .expect("install succeeds");
        assert_eq!(installed, local_bin.join("kumu"));
        assert_eq!(
            probe_status(&exe, &path_dirs, Some(&home)),
            CliInstallStatus::Installed {
                link_path: installed.clone(),
            }
        );

        // A link outside the active PATH is not reported as installed.
        let other_path = vec![root.join("other-bin")];
        assert!(matches!(
            probe_status(&exe, &other_path, Some(&home)),
            CliInstallStatus::NotInstalled { .. }
        ));

        // Moving the app makes the old symlink NeedsUpdate, and install updates it.
        let new_exe = root.join("Applications/Kumu.app/Contents/MacOS/kumu");
        fs::create_dir_all(new_exe.parent().unwrap()).expect("create new app dir");
        fs::write(&new_exe, b"kumu-bin-v2").expect("write fake exe v2");
        let new_exe = fs::canonicalize(&new_exe).expect("canonical new exe");
        assert!(matches!(
            probe_status(&new_exe, &path_dirs, Some(&home)),
            CliInstallStatus::NeedsUpdate { ref link_path, .. } if *link_path == installed
        ));

        let updated = install_for(&new_exe, &path_dirs, Some(&home), |_| {
            panic!("should not require privilege")
        })
        .expect("update succeeds");
        assert_eq!(updated, installed);
        assert_eq!(
            probe_status(&new_exe, &path_dirs, Some(&home)),
            CliInstallStatus::Installed {
                link_path: installed.clone(),
            }
        );

        // Uninstall removes symlink and restores NotInstalled.
        let removed = uninstall_for(&new_exe, &path_dirs, Some(&home), |_| {
            panic!("should not require privilege")
        })
        .expect("uninstall succeeds");
        assert_eq!(removed, installed);
        assert!(matches!(
            probe_status(&new_exe, &path_dirs, Some(&home)),
            CliInstallStatus::NotInstalled { .. }
        ));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn foreign_binary_or_symlink_is_reported_as_conflict_and_never_overwritten() {
        let root = temp_dir("conflict");
        let home = root.join("home");
        let local_bin = home.join(".local/bin");
        fs::create_dir_all(&local_bin).expect("create .local/bin");

        let foreign = local_bin.join("kumu");
        fs::write(&foreign, b"other-tool").expect("write foreign file");

        let exe = root.join("Kumu.app/Contents/MacOS/kumu");
        fs::create_dir_all(exe.parent().unwrap()).expect("create app dir");
        fs::write(&exe, b"kumu-bin").expect("write fake exe");

        let path_dirs = vec![local_bin.clone()];
        assert_eq!(
            probe_status(&exe, &path_dirs, Some(&home)),
            CliInstallStatus::Conflict {
                path: foreign.clone(),
            }
        );
        assert_eq!(
            install_for(&exe, &path_dirs, Some(&home), |_| Ok(())),
            Err(CliInstallError::Conflict(foreign.clone()))
        );
        assert_eq!(
            uninstall_for(&exe, &path_dirs, Some(&home), |_| Ok(())),
            Err(CliInstallError::Conflict(foreign.clone()))
        );
        assert_eq!(
            fs::read(&foreign).expect("foreign preserved"),
            b"other-tool"
        );

        // A symlink pointing to a non-Kumu tool also named `kumu` (e.g. from Homebrew)
        // must be treated as Conflict, never overwritten as NeedsUpdate.
        fs::remove_file(&foreign).expect("remove foreign file");
        let cellar_bin = root.join("Cellar/other-kumu/1.0/bin/kumu");
        fs::create_dir_all(cellar_bin.parent().unwrap()).expect("create cellar dir");
        fs::write(&cellar_bin, b"homebrew-other").expect("write cellar binary");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&cellar_bin, &foreign).expect("symlink foreign kumu");

        assert_eq!(
            probe_status(&exe, &path_dirs, Some(&home)),
            CliInstallStatus::Conflict {
                path: foreign.clone(),
            }
        );
        assert_eq!(
            install_for(&exe, &path_dirs, Some(&home), |_| Ok(())),
            Err(CliInstallError::Conflict(foreign.clone()))
        );
        assert_eq!(
            fs::read_link(&foreign).expect("foreign symlink preserved"),
            cellar_bin
        );

        let _ = fs::remove_dir_all(&root);
    }
}
