//! File drag-and-drop and clipboard payload resolution for terminal panes.
//!
//! Paths are formatted with Ghostty-style backslash escaping and sent through
//! the terminal's normal paste path (bracketed paste when mode 2004 is on), so
//! local coding agents recognize dropped or pasted image paths as attachments
//! while plain shells receive properly escaped arguments.

use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};

use gpui::{ClipboardEntry, ClipboardItem, Image, ImageFormat};

/// Subdirectory under `$TMPDIR` holding screenshots pasted from the clipboard.
const CLIPBOARD_DIR_NAME: &str = "kumu-clipboard";

/// Prefix for temporary files created by Kumu's clipboard image paste.
const IMAGE_FILE_PREFIX: &str = "paste-";

/// Temporary clipboard screenshots older than this are pruned lazily on save.
const MAX_IMAGE_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// What a clipboard read turns into for a terminal pane.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum ClipboardPayload {
    /// Text ready to send through `terminal_paste` (plain text or escaped paths).
    Text(String),
    /// An image to persist to a temporary file before pasting its path.
    Image(Image),
}

/// Escape shell-sensitive characters with backslashes, matching Ghostty's
/// `Ghostty.Shell.escape` while keeping UTF-8 filenames readable.
pub(super) fn escape_shell_path(path: &Path) -> String {
    let raw = path.to_string_lossy();
    let mut escaped = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if matches!(
            ch,
            '\\' | ' '
                | '('
                | ')'
                | '['
                | ']'
                | '{'
                | '}'
                | '<'
                | '>'
                | '"'
                | '\''
                | '`'
                | '!'
                | '#'
                | '$'
                | '&'
                | ';'
                | '|'
                | '*'
                | '?'
                | '\t'
        ) {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

/// Format one or more dropped/copied paths as space-separated, shell-escaped
/// absolute paths. Returns `None` when no non-empty path is present.
pub(super) fn format_dropped_paths(paths: &[PathBuf]) -> Option<String> {
    let mut out = String::new();
    for path in paths {
        if path.as_os_str().is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&escape_shell_path(path));
    }
    (!out.is_empty()).then_some(out)
}

/// Choose what a terminal paste should send from a platform clipboard item.
///
/// Priority matches macOS terminal conventions (and Ghostty's
/// `getOpinionatedStringContents`):
/// 1. Copied files (`ExternalPaths`) take precedence over `String`, because
///    Finder writes both the full file paths and a bare filename string.
/// 2. Plain text (`String`) is pasted verbatim.
/// 3. Images (`Image`, e.g. `⌘⌃⇧4` screenshots) are persisted to a temporary
///    file so any local coding agent can attach or read them by path.
pub(super) fn resolve_clipboard(item: &ClipboardItem) -> Option<ClipboardPayload> {
    let mut paths = Vec::new();
    for entry in item.entries() {
        if let ClipboardEntry::ExternalPaths(external) = entry {
            paths.extend(external.paths().iter().cloned());
        }
    }
    if let Some(formatted) = format_dropped_paths(&paths) {
        return Some(ClipboardPayload::Text(formatted));
    }

    let mut text = String::new();
    for entry in item.entries() {
        if let ClipboardEntry::String(string) = entry {
            text.push_str(string.text());
        }
    }
    if !text.is_empty() {
        return Some(ClipboardPayload::Text(text));
    }

    for entry in item.entries() {
        if let ClipboardEntry::Image(image) = entry
            && !image.bytes.is_empty()
        {
            return Some(ClipboardPayload::Image(image.clone()));
        }
    }

    None
}

/// Persist a clipboard image to Kumu's private temporary directory and return
/// its on-disk path. Safe to call on a background executor.
pub(super) fn save_clipboard_image(image: &Image) -> io::Result<PathBuf> {
    let dir = std::env::temp_dir().join(CLIPBOARD_DIR_NAME);
    save_clipboard_image_in(&dir, image, SystemTime::now())
}

fn save_clipboard_image_in(dir: &Path, image: &Image, now: SystemTime) -> io::Result<PathBuf> {
    ensure_private_dir(dir)?;
    prune_stale_images(dir, now);

    let filename = format!(
        "{IMAGE_FILE_PREFIX}{:016x}.{}",
        image.id,
        image_ext(image.format)
    );
    let target = dir.join(filename);

    // Reuse an already-materialized file for repeated pastes of the same image.
    if let Ok(meta) = fs::symlink_metadata(&target)
        && meta.file_type().is_file()
        && meta.len() == image.bytes.len() as u64
    {
        return Ok(target);
    }

    static NEXT_TMP: AtomicU64 = AtomicU64::new(0);
    let seq = NEXT_TMP.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(".tmp-{}-{seq}", std::process::id()));

    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);

    let mut file = options.open(&tmp)?;
    if let Err(error) = file.write_all(&image.bytes).and_then(|()| file.flush()) {
        let _ = fs::remove_file(&tmp);
        return Err(error);
    }
    drop(file);

    if let Err(error) = fs::rename(&tmp, &target) {
        let _ = fs::remove_file(&tmp);
        return Err(error);
    }
    Ok(target)
}

fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    match fs::symlink_metadata(dir) {
        Ok(meta) => {
            if meta.file_type().is_symlink() || !meta.file_type().is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("剪贴板临时目录无效：{}", dir.display()),
                ));
            }
            #[cfg(unix)]
            {
                let mode = meta.permissions().mode() & 0o777;
                if mode != 0o700 {
                    let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
                }
            }
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let mut builder = DirBuilder::new();
            builder.recursive(true);
            #[cfg(unix)]
            builder.mode(0o700);
            builder.create(dir)
        }
        Err(error) => Err(error),
    }
}

fn prune_stale_images(dir: &Path, now: SystemTime) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !name.starts_with(IMAGE_FILE_PREFIX) {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let Ok(modified) = meta.modified() else {
            continue;
        };
        if now
            .duration_since(modified)
            .is_ok_and(|age| age > MAX_IMAGE_AGE)
        {
            let _ = fs::remove_file(entry.path());
        }
    }
}

fn image_ext(format: ImageFormat) -> &'static str {
    match format {
        ImageFormat::Png => "png",
        ImageFormat::Jpeg => "jpg",
        ImageFormat::Webp => "webp",
        ImageFormat::Gif => "gif",
        ImageFormat::Svg => "svg",
        ImageFormat::Bmp => "bmp",
        ImageFormat::Tiff => "tiff",
        ImageFormat::Ico => "ico",
        ImageFormat::Pnm => "pnm",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{ClipboardString, ExternalPaths};

    #[test]
    fn escapes_spaces_shell_metachars_and_preserves_utf8() {
        let path = Path::new("/Users/me/Desktop/截屏 2026-11-22 (1) [a&b]$x'y.png");
        assert_eq!(
            escape_shell_path(path),
            r"/Users/me/Desktop/截屏\ 2026-11-22\ \(1\)\ \[a\&b\]\$x\'y.png"
        );
    }

    #[test]
    fn formats_multiple_paths_separated_by_single_space() {
        let paths = vec![
            PathBuf::from("/tmp/a.rs"),
            PathBuf::from(""),
            PathBuf::from("/tmp/my shot.png"),
        ];
        assert_eq!(
            format_dropped_paths(&paths).as_deref(),
            Some(r"/tmp/a.rs /tmp/my\ shot.png")
        );
        assert_eq!(format_dropped_paths(&[]), None);
    }

    #[test]
    fn clipboard_prefers_external_paths_over_finder_bare_filename() {
        let item = ClipboardItem {
            entries: vec![
                ClipboardEntry::ExternalPaths(ExternalPaths(
                    [PathBuf::from("/Users/me/Desktop/my shot.png")]
                        .into_iter()
                        .collect(),
                )),
                ClipboardEntry::String(ClipboardString::new("my shot.png".to_string())),
            ],
        };
        assert_eq!(
            resolve_clipboard(&item),
            Some(ClipboardPayload::Text(
                r"/Users/me/Desktop/my\ shot.png".to_string()
            ))
        );
    }

    #[test]
    fn clipboard_resolves_plain_text_and_image_fallback() {
        let text = ClipboardItem::new_string("hello world".to_string());
        assert_eq!(
            resolve_clipboard(&text),
            Some(ClipboardPayload::Text("hello world".to_string()))
        );

        let image = Image {
            format: ImageFormat::Png,
            bytes: vec![0x89, 0x50, 0x4E, 0x47],
            id: 0xdead_beef,
        };
        let item = ClipboardItem::new_image(&image);
        assert_eq!(
            resolve_clipboard(&item),
            Some(ClipboardPayload::Image(image))
        );
    }

    #[test]
    fn saves_clipboard_image_and_prunes_stale_files() {
        let dir = std::env::temp_dir().join(format!(
            "kumu-clipboard-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&dir);

        let img = Image {
            format: ImageFormat::Png,
            bytes: vec![1, 2, 3, 4],
            id: 0x0123_4567_89ab_cdef,
        };
        let now = SystemTime::now();
        let saved = save_clipboard_image_in(&dir, &img, now).expect("save image");
        assert!(saved.ends_with("paste-0123456789abcdef.png"));
        assert_eq!(fs::read(&saved).unwrap(), vec![1, 2, 3, 4]);

        // Re-pasting the same image reuses the existing file.
        let saved_again = save_clipboard_image_in(&dir, &img, now).expect("reuse image");
        assert_eq!(saved_again, saved);

        // Advancing `now` past MAX_IMAGE_AGE prunes the old screenshot when a new one is saved.
        let newer = Image {
            format: ImageFormat::Jpeg,
            bytes: vec![9, 8, 7],
            id: 0x42,
        };
        let future = now + MAX_IMAGE_AGE + Duration::from_secs(60);
        let newer_path = save_clipboard_image_in(&dir, &newer, future).expect("save newer");
        assert!(newer_path.exists());
        assert!(!saved.exists());

        let _ = fs::remove_dir_all(&dir);
    }
}
