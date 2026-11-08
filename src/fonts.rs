//! Fonts the terminal draws with: JetBrains Mono for the grid, Symbols Nerd
//! Font for icons prompts and agent TUIs print. Bundled so every machine gets
//! the same look; GPUI falls back to proportional system fonts otherwise and
//! the cell grid breaks.
//!
//! Four static faces (Medium / Bold / Medium Italic / Bold Italic) so
//! `font_kit` can match cell bold/italic to real glyphs — a single Variable
//! file only exposes one default weight and bold looks identical to medium.

use std::borrow::Cow;

use gpui_kit::App;

/// Terminal body text unless the theme picks another mono family.
pub const TERM_FONT: &str = "JetBrains Mono";
/// Nerd Font icons (private-use), under any mono that lacks them.
pub const SYMBOLS_FONT: &str = "Symbols Nerd Font";

const BUNDLED: &[&[u8]] = &[
    include_bytes!("../assets/fonts/JetBrainsMono-Medium.ttf"),
    include_bytes!("../assets/fonts/JetBrainsMono-Bold.ttf"),
    include_bytes!("../assets/fonts/JetBrainsMono-MediumItalic.ttf"),
    include_bytes!("../assets/fonts/JetBrainsMono-BoldItalic.ttf"),
    include_bytes!("../assets/fonts/SymbolsNerdFont-Regular.ttf"),
];

/// Register the bundled faces with the text system. Call once at startup
/// before any terminal shapes a line.
pub fn install(cx: &App) {
    let fonts: Vec<Cow<'static, [u8]>> = BUNDLED.iter().copied().map(Cow::Borrowed).collect();
    if let Err(err) = cx.text_system().add_fonts(fonts) {
        eprintln!("[fonts] bundled terminal fonts not loaded: {err:#}");
    }
}
