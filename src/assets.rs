//! App asset source: kumu's own `assets/` folder (agent logos, …) layered on
//! top of gpui-kit's bundled icon set, which stays the fallback.

use std::borrow::Cow;

use gpui_kit::gpui::{AssetSource, Result, SharedString};

/// Files embedded from the crate-root `assets/` folder.
#[derive(rust_embed::RustEmbed)]
#[folder = "assets"]
struct KumuAssets;

/// kumu's app icon, embedded so it is available however the binary was started
/// (the Dock icon needs it before any bundle exists — see `dock_icon`).
pub fn app_icon() -> Option<Cow<'static, [u8]>> {
    KumuAssets::get("AppIcon.icns").map(|file| file.data)
}

pub struct KumuAssetSource;

impl AssetSource for KumuAssetSource {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if path.is_empty() {
            return Ok(None);
        }
        if let Some(file) = KumuAssets::get(path) {
            return Ok(Some(file.data));
        }
        // gpui-kit's Assets reports misses as Err; normalize to None.
        Ok(gpui_kit::assets::Assets.load(path).unwrap_or(None))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut all = gpui_kit::assets::Assets.list(path)?;
        all.extend(
            KumuAssets::iter()
                .filter(|p| p.starts_with(path))
                .map(SharedString::from),
        );
        Ok(all)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_kumu_and_fallback_assets() {
        assert!(KumuAssetSource.load("omp.png").unwrap().is_some());
        assert!(KumuAssetSource.load("opencode.png").unwrap().is_some());
        assert!(KumuAssetSource.load("codex.svg").unwrap().is_some());
        assert!(KumuAssetSource.load("pi.png").unwrap().is_some());
        assert!(KumuAssetSource.load("claude.png").unwrap().is_some());
        assert!(KumuAssetSource.load("cursor.svg").unwrap().is_some());
        assert!(KumuAssetSource.load("grok.svg").unwrap().is_some());
        assert!(KumuAssetSource.load("split-right.svg").unwrap().is_some());
        assert!(KumuAssetSource.load("split-down.svg").unwrap().is_some());
        assert!(
            KumuAssetSource
                .load("icons/git-branch.svg")
                .unwrap()
                .is_some()
        );
        assert!(
            KumuAssetSource
                .load("icons/smartphone.svg")
                .unwrap()
                .is_some()
        );
        // gpui-kit's bundled icons resolve through the fallback.
        assert!(
            KumuAssetSource
                .load("icons/settings.svg")
                .unwrap()
                .is_some()
        );
        assert!(KumuAssetSource.load("missing.svg").unwrap().is_none());
        // The Dock icon loads through here; losing the file silently falls
        // back to macOS's generic executable icon.
        assert!(app_icon().is_some());
    }
}
