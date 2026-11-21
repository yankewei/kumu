//! Bundled assets embedded into the binary via `rust-embed` (app icons, agent
//! logos, vendored Lucide SVGs, and terminal fonts).

use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "assets"]
struct AppAssets;

pub struct KumuAssetSource;

/// Embedded `assets/AppIcon.icns` bytes for setting the macOS Dock icon during
/// unbundled `cargo run` launches.
pub fn app_icon() -> Option<Cow<'static, [u8]>> {
    AppAssets::get("AppIcon.icns").map(|f| f.data)
}

impl AssetSource for KumuAssetSource {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if path.is_empty() {
            return Ok(None);
        }
        Ok(AppAssets::get(path).map(|f| f.data))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(AppAssets::iter()
            .filter(|p| p.starts_with(path))
            .map(SharedString::from)
            .collect())
    }
}
