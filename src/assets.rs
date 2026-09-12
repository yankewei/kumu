//! App asset source: kumu's own `assets/` folder (agent logos, …) layered on
//! top of gpui-kit's bundled icon set, which stays the fallback.

use std::borrow::Cow;

use gpui_kit::gpui::{AssetSource, Result, SharedString};

/// Files embedded from the crate-root `assets/` folder.
#[derive(rust_embed::RustEmbed)]
#[folder = "assets"]
struct KumuAssets;

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
        // kumu's own icons layer on top of gpui-kit's bundled set (the reason
        // chip's brain glyph, which the bundled set does not ship).
        assert!(KumuAssetSource.load("icons/brain.svg").unwrap().is_some());
        // gpui-kit's bundled icons resolve through the fallback.
        assert!(
            KumuAssetSource
                .load("icons/settings.svg")
                .unwrap()
                .is_some()
        );
        assert!(KumuAssetSource.load("missing.svg").unwrap().is_none());
    }
}
