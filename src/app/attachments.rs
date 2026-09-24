//! Images attached to prompts, kept in kumu's own folder beside the database.
//!
//! Every attachment is copied there before it is sent — a pasted screenshot has
//! no file at all, and a picked one may be moved or deleted later — so the
//! history can keep showing what was sent, and a backend that hands the agent a
//! path (Codex) hands it one that stays put. Files are named by their content,
//! so attaching the same image twice stores it once.

use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use crate::agent::ImageAttachment;
use crate::db::db_path;

/// The image types every agent kumu drives accepts: the Messages API's four,
/// which the other protocols' models share.
const IMAGE_TYPES: [(&str, &str); 5] = [
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
];

/// The `(extension, MIME type)` of an image file kumu can attach, judged by its
/// extension.
fn image_type(path: &Path) -> Option<(&'static str, &'static str)> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    IMAGE_TYPES.iter().copied().find(|(known, _)| *known == ext)
}

/// The MIME type of an image file kumu can attach, judged by its extension.
pub fn image_mime(path: &Path) -> Option<&'static str> {
    image_type(path).map(|(_, mime)| mime)
}

/// Store image bytes (a pasted screenshot) as an attachment.
pub fn stash_bytes(bytes: &[u8], mime: &str) -> Result<ImageAttachment, String> {
    let Some(&(ext, mime)) = IMAGE_TYPES.iter().find(|(_, known)| *known == mime) else {
        return Err(format!("不支持的图片格式：{mime}"));
    };
    stash_in(&attachments_dir(), bytes, ext, mime)
}

/// Copy an image file (picked, dropped or pasted from Finder) in as an
/// attachment.
pub fn stash_file(path: &Path) -> Result<ImageAttachment, String> {
    let Some((ext, mime)) = image_type(path) else {
        return Err(format!("不支持的图片格式：{}", path.display()));
    };
    let bytes = std::fs::read(path)
        .map_err(|error| format!("读取图片 {} 失败：{error}", path.display()))?;
    stash_in(&attachments_dir(), &bytes, ext, mime)
}

fn attachments_dir() -> PathBuf {
    db_path()
        .parent()
        .map(|dir| dir.join("attachments"))
        .unwrap_or_else(|| PathBuf::from("attachments"))
}

fn stash_in(dir: &Path, bytes: &[u8], ext: &str, mime: &str) -> Result<ImageAttachment, String> {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    let path = dir.join(format!("{:016x}.{ext}", hasher.finish()));
    if !path.exists() {
        std::fs::create_dir_all(dir)
            .and_then(|()| std::fs::write(&path, bytes))
            .map_err(|error| format!("保存图片失败：{error}"))?;
    }
    Ok(ImageAttachment {
        path,
        mime: mime.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{image_mime, stash_in};

    #[test]
    fn only_the_four_shared_image_types_attach() {
        assert_eq!(image_mime(Path::new("a/shot.PNG")), Some("image/png"));
        assert_eq!(image_mime(Path::new("photo.jpg")), Some("image/jpeg"));
        assert_eq!(image_mime(Path::new("icon.svg")), None);
        assert_eq!(image_mime(Path::new("README")), None);
    }

    /// The same bytes land in the same file, so re-attaching stores nothing new.
    #[test]
    fn identical_images_share_one_file() {
        let dir = std::env::temp_dir().join(format!("kumu-attach-{}", std::process::id()));
        let first = stash_in(&dir, b"png bytes", "png", "image/png").unwrap();
        let again = stash_in(&dir, b"png bytes", "png", "image/png").unwrap();
        let other = stash_in(&dir, b"other", "png", "image/png").unwrap();
        assert_eq!(first.path, again.path);
        assert_ne!(first.path, other.path);
        assert_eq!(std::fs::read(&first.path).unwrap(), b"png bytes");
        std::fs::remove_dir_all(&dir).ok();
    }
}
