//! Bridges Kumu's embedded assets (`crate::assets::KumuAssetSource`) into NSH
//! [`ImageSrc`] values so SVG icons, agent logos, and PNG badges render natively
//! in Pure Rust NSH views without external asset servers.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use gpui::AssetSource;
use nsh_base::theme;
use nsh_core::element::{ImageSrc, Svg, image, svg};
use nsh_core::prelude::*;

use crate::assets::KumuAssetSource;

static ASSET_CACHE: OnceLock<Mutex<HashMap<String, ImageSrc>>> = OnceLock::new();

fn cache() -> &'static Mutex<HashMap<String, ImageSrc>> {
    ASSET_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn encode_base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[((n >> 6) & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(n & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// Resolve an embedded asset path (`.svg` or `.png`) to a cached [`ImageSrc`].
pub fn asset_src(path: &str) -> ImageSrc {
    if let Ok(guard) = cache().lock()
        && let Some(cached) = guard.get(path)
    {
        return cached.clone();
    }

    let src = match KumuAssetSource.load(path).ok().flatten() {
        Some(bytes) if path.ends_with(".svg") => match std::str::from_utf8(bytes.as_ref()) {
            Ok(markup) => ImageSrc::svg_markup(markup),
            Err(_) => ImageSrc::from(""),
        },
        Some(bytes) if path.ends_with(".png") => {
            ImageSrc::from(format!("data:image/png;base64,{}", encode_base64(&bytes)))
        }
        Some(bytes) => ImageSrc::from(format!(
            "data:application/octet-stream;base64,{}",
            encode_base64(&bytes)
        )),
        None => ImageSrc::from(""),
    };

    if let Ok(mut guard) = cache().lock() {
        guard.insert(path.to_owned(), src.clone());
    }
    src
}

/// Crisp vector icon from `KumuAssetSource` (follows `text_color` via `currentColor`).
pub fn icon_svg(path: &str, size: f32) -> Svg {
    svg(asset_src(path)).w(px(size)).h(px(size)).flex_shrink_0()
}

/// Render an agent's logo (SVG tinted with `muted_foreground`, PNG in full color,
/// or fallback `icons/bot.svg`).
pub fn agent_logo(logo: Option<&'static str>, size: f32) -> AnyElement {
    match logo {
        Some(path) if path.ends_with(".svg") => icon_svg(path, size)
            .text_color(move || theme().muted_foreground)
            .into_element(),
        Some(path) => image(asset_src(path))
            .w(px(size))
            .h(px(size))
            .flex_shrink_0()
            .into_element(),
        None => icon_svg("icons/bot.svg", size)
            .text_color(move || theme().muted_foreground)
            .into_element(),
    }
}
