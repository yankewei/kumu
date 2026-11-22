use std::cell::{Cell, RefCell};
use std::sync::Arc;

use gpui::*;

use super::{CursorAt, PAD_X, PAD_Y, PaintCache};
use crate::term::link;
use crate::term::{Cell as TermCell, CursorShape, Rgb, Snapshot};
use crate::ui::home::sprites;

#[derive(Clone)]
pub(super) struct Typeface {
    pub(super) family: SharedString,
    pub(super) size: f32,
}

pub(super) fn cell_size(window: &Window, typeface: &Typeface) -> (Pixels, Pixels) {
    let font = cell_font(typeface, false, false);
    let system = window.text_system();
    let id = system.resolve_font(&font);
    let size_px = px(typeface.size);
    let width = system
        .advance(id, size_px, 'M')
        .map(|advance| advance.width)
        .unwrap_or(size_px * 0.6)
        .max(px(1.));
    let height = system.ascent(id, size_px) + system.descent(id, size_px).abs();
    // Width stays the font advance; boxes snap edges to device pixels instead.
    let scale = window.scale_factor().max(1.);
    (
        width,
        px(((f32::from(height) * 1.2 * scale).ceil() / scale).max(1.)),
    )
}

fn cell_font(typeface: &Typeface, bold: bool, italic: bool) -> Font {
    let mut font = gpui::font(typeface.family.clone());
    font.weight = if bold {
        FontWeight::BOLD
    } else {
        FontWeight::MEDIUM
    };
    font.style = if italic {
        FontStyle::Italic
    } else {
        FontStyle::Normal
    };
    font
}

pub(super) fn paint_color(rgb: Rgb) -> Hsla {
    Rgba {
        r: rgb.0 as f32 / 255.,
        g: rgb.1 as f32 / 255.,
        b: rgb.2 as f32 / 255.,
        a: 1.,
    }
    .into()
}

/// WCAG relative luminance of an sRGB cell color.
fn luminance(rgb: Rgb) -> f32 {
    let ch = |v: u8| {
        let v = v as f32 / 255.0;
        if v <= 0.03928 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * ch(rgb.0) + 0.7152 * ch(rgb.1) + 0.0722 * ch(rgb.2)
}

fn contrast_ratio(a: Rgb, b: Rgb) -> f32 {
    let (x, y) = (luminance(a), luminance(b));
    (x.max(y) + 0.05) / (x.min(y) + 0.05)
}

/// Keep light-background text readable without flattening its colors. Dark
/// backgrounds retain the existing near-invisible-text safeguard.
fn readable_fg(fg: Rgb, bg: Rgb, text: &str) -> Rgb {
    if sprites::is_sprite(text) || preserves_graphics_color(text) {
        return fg;
    }
    let light_background = luminance(bg) > 0.5;
    let minimum = if light_background { 4.5 } else { 1.1 };
    if contrast_ratio(fg, bg) >= minimum {
        return fg;
    }
    let light = Rgb(0xf2, 0xf2, 0xf4);
    let dark = Rgb(0x1b, 0x1b, 0x1e);
    let target = if light_background {
        Rgb(0, 0, 0)
    } else if contrast_ratio(light, bg) >= contrast_ratio(dark, bg) {
        light
    } else {
        dark
    };
    if !light_background {
        return target;
    }
    // Find the smallest blend that meets the contrast floor, preserving the
    // original hue instead of replacing every pale color with the same ink.
    let (mut low, mut high) = (0., 1.);
    let mut result = target;
    for _ in 0..8 {
        let amount = (low + high) / 2.;
        let mix =
            |from: u8, to: u8| (from as f32 + (to as f32 - from as f32) * amount).round() as u8;
        let candidate = Rgb(
            mix(fg.0, target.0),
            mix(fg.1, target.1),
            mix(fg.2, target.2),
        );
        if contrast_ratio(candidate, bg) >= minimum {
            high = amount;
            result = candidate;
        } else {
            low = amount;
        }
    }
    result
}

/// Color of the IME preview at the cursor cell. A visible cursor lends its
/// color; a hidden one leaves only the default foreground, never a fixed gray
/// that a light theme would wash out. `readable_fg` keeps the preview legible
/// on either background without recoloring a color that already reads.
fn marked_fg(snapshot: &Snapshot, text: &str) -> Rgb {
    let base = snapshot.cursor.map_or(snapshot.fg, |cursor| cursor.color);
    readable_fg(base, snapshot.bg, text)
}

fn preserves_graphics_color(text: &str) -> bool {
    text.chars().next().is_some_and(|c| {
        matches!(
            c as u32,
            0x2500..=0x259f | 0xe0b0..=0xe0d7 | 0x1fb00..=0x1fbff | 0x1cc00..=0x1cebf
        )
    })
}

/// Force-width shaping moves glyph origins; it does not shrink fallback glyphs
/// (emoji). Scale the font down when a non-wide cell overflows its column.
fn fallback_size(
    c: &TermCell,
    font: &Font,
    width: Pixels,
    size: f32,
    ts: &WindowTextSystem,
) -> Option<Pixels> {
    if c.wide || c.text.is_ascii() {
        return None;
    }
    let first = c.text.chars().next()?;
    let size_px = px(size);
    let primary = ts.resolve_font(font);
    if ts.advance(primary, size_px, first).is_ok() {
        return None;
    }
    let run = TextRun {
        len: c.text.len(),
        font: font.clone(),
        ..Default::default()
    };
    let line = ts.shape_line(c.text.clone().into(), size_px, &[run], None);
    (line.width > width).then(|| size_px * f32::from(width) / f32::from(line.width))
}

fn nerd_icon(text: &str) -> Option<char> {
    let mut chars = text.chars();
    let c = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    matches!(
        c as u32,
        0xe000..=0xf8ff | 0xf0000..=0xffffd | 0x100000..=0x10fffd
    )
    .then_some(c)
}

pub(super) enum RowOp {
    Bg { from: usize, to: usize, color: Hsla },
    Sprite { x: usize, text: String, color: Hsla },
    Text { x: usize, line: Box<ShapedLine> },
}

/// Backgrounds merged into runs, then sprites / text: same-style runs forced
/// onto the cell width so glyphs sit on the grid.
fn row_ops(
    row: &[TermCell],
    cell_w: Pixels,
    term_bg: Rgb,
    typeface: &Typeface,
    window: &mut Window,
) -> Vec<RowOp> {
    let mut ops = Vec::new();
    let mut x = 0;
    while x < row.len() {
        let Some(bg) = row[x].bg else {
            x += 1;
            continue;
        };
        let start = x;
        while x < row.len() && row[x].bg == Some(bg) {
            x += 1;
        }
        ops.push(RowOp::Bg {
            from: start,
            to: x,
            color: paint_color(bg),
        });
    }
    let mut x = 0;
    while x < row.len() {
        let start = x;
        let c = &row[start];
        x += 1;
        if c.text.is_empty() && !c.underline && !c.strike {
            continue;
        }
        let cell_bg = c.bg.unwrap_or(term_bg);
        let sprite = sprites::is_sprite(&c.text);
        let mut color = paint_color(readable_fg(c.fg, cell_bg, &c.text));
        if c.faint {
            color.a = if luminance(cell_bg) > 0.5 && !sprite && !preserves_graphics_color(&c.text) {
                0.75
            } else {
                0.6
            };
        }
        if sprite {
            ops.push(RowOp::Sprite {
                x: start,
                text: c.text.clone(),
                color,
            });
            continue;
        }
        let mut text_font = cell_font(typeface, c.bold, c.italic);
        let ts = window.text_system();
        // Icons the mono face lacks: shape from the bundled Nerd symbols font.
        if let Some(icon) = nerd_icon(&c.text) {
            let primary = ts.resolve_font(&text_font);
            if ts.advance(primary, px(typeface.size), icon).is_err() {
                text_font = gpui::font(crate::fonts::SYMBOLS_FONT);
                text_font.weight = FontWeight::NORMAL;
            }
        }
        let fitted = fallback_size(c, &text_font, cell_w, typeface.size, ts);
        let mut text = if c.text.is_empty() {
            " ".to_owned()
        } else {
            c.text.clone()
        };
        if !c.wide && fitted.is_none() {
            while x < row.len() {
                let d = &row[x];
                let same = !d.wide
                    && d.fg == c.fg
                    && d.bg == c.bg
                    && d.bold == c.bold
                    && d.italic == c.italic
                    && d.faint == c.faint
                    && d.underline == c.underline
                    && d.strike == c.strike;
                if !same
                    || (d.text.is_empty() && !d.underline && !d.strike)
                    || sprites::is_sprite(&d.text)
                    || nerd_icon(&d.text).is_some()
                    || fallback_size(d, &text_font, cell_w, typeface.size, ts).is_some()
                {
                    break;
                }
                text.push_str(if d.text.is_empty() { " " } else { &d.text });
                x += 1;
            }
        }
        if text.trim().is_empty() && !c.underline && !c.strike {
            continue;
        }
        let run = TextRun {
            len: text.len(),
            font: text_font,
            color,
            background_color: None,
            underline: c.underline.then(|| UnderlineStyle {
                thickness: px(1.0),
                color: Some(color),
                wavy: false,
            }),
            strikethrough: c.strike.then(|| StrikethroughStyle {
                thickness: px(1.0),
                color: Some(color),
            }),
        };
        let force = (!c.wide).then_some(cell_w);
        let line = window.text_system().shape_line(
            text.into(),
            fitted.unwrap_or(px(typeface.size)),
            &[run],
            force,
        );
        ops.push(RowOp::Text {
            x: start,
            line: Box::new(line),
        });
    }
    ops
}

/// One frame's grid: what to draw and the caches it draws through.
pub(super) struct GridPaint<'a> {
    pub(super) bounds: Bounds<Pixels>,
    pub(super) snapshot: &'a Snapshot,
    pub(super) cell: (Pixels, Pixels),
    pub(super) focused: bool,
    pub(super) marked: Option<&'a str>,
    pub(super) typeface: &'a Typeface,
    pub(super) painted: &'a RefCell<PaintCache>,
    pub(super) cursor_at: &'a Cell<CursorAt>,
    /// Find-bar highlight for a match and for the selected one.
    pub(super) search_hit: Hsla,
    pub(super) search_active: Hsla,
    /// Visible runs of a ⌘-hovered link, underlined so the click target shows.
    pub(super) link: &'a [link::Span],
}

/// Where the cell grid starts inside its canvas: padded, with the leftover
/// height split evenly above and below, like herdr.
pub(super) fn grid_origin(
    bounds: Bounds<Pixels>,
    cell_h: Pixels,
    rows: u16,
    scale: f32,
) -> Point<Pixels> {
    let spare =
        (f32::from(bounds.size.height) - 2. * PAD_Y - f32::from(cell_h) * f32::from(rows)).max(0.);
    let snap = |value: Pixels| px((f32::from(value) * scale).round() / scale);
    point(
        snap(bounds.origin.x + px(PAD_X)),
        snap(bounds.origin.y + px(PAD_Y + (spare / 2.).floor())),
    )
}

pub(super) fn paint_grid(grid: GridPaint<'_>, window: &mut Window, cx: &mut App) -> Point<Pixels> {
    let GridPaint {
        bounds,
        snapshot,
        cell: (cell_w, cell_h),
        focused,
        marked,
        typeface,
        painted,
        cursor_at,
        search_hit,
        search_active,
        link,
    } = grid;
    window.paint_quad(fill(bounds, paint_color(snapshot.bg)));
    let scale = window.scale_factor();
    let snap = |value: Pixels| px((f32::from(value) * scale).round() / scale);
    // Center leftover height between top and bottom padding, like herdr.
    let origin = grid_origin(bounds, cell_h, snapshot.rows, scale);
    let at = |x: usize, y: usize| origin + point(cell_w * x as f32, cell_h * y as f32);
    let snap_box = |b: Bounds<Pixels>| {
        let (x0, x1) = (snap(b.origin.x), snap(b.origin.x + b.size.width));
        let (y0, y1) = (snap(b.origin.y), snap(b.origin.y + b.size.height));
        Bounds::new(point(x0, y0), size(x1 - x0, y1 - y0))
    };
    let cell = (cell_w, cell_h);
    let font_key = (typeface.family.clone(), typeface.size.to_bits());

    {
        let mut cache = painted.borrow_mut();
        // A cell, font, or terminal-background change invalidates cached row ops.
        // Full-dirty snapshots already allocate fresh row Arcs so `Arc::ptr_eq`
        // naturally misses once without wiping the cache on subsequent repaints.
        if cache.cell != cell || cache.font != font_key || cache.bg != snapshot.bg {
            cache.rows.clear();
        }
        cache.cell = cell;
        cache.font = font_key;
        cache.bg = snapshot.bg;
        cache.rows.truncate(snapshot.cells.len());
        for (y, row) in snapshot.cells.iter().enumerate() {
            let reuse = cache
                .rows
                .get(y)
                .is_some_and(|(cached, _)| Arc::ptr_eq(cached, row));
            if reuse {
                continue;
            }
            let ops = row_ops(row, cell_w, snapshot.bg, typeface, window);
            if y < cache.rows.len() {
                cache.rows[y] = (Arc::clone(row), ops);
            } else {
                cache.rows.push((Arc::clone(row), ops));
            }
        }
    }

    let cache = painted.borrow();
    for (y, (_, ops)) in cache.rows.iter().enumerate() {
        for op in ops {
            if let RowOp::Bg { from, to, color } = op {
                let b = Bounds::new(at(*from, y), size(cell_w * (*to - *from) as f32, cell_h));
                window.paint_quad(fill(snap_box(b), *color));
            }
        }
    }
    let cell_bounds = size(cell_w, cell_h);
    if let Some(search) = &snapshot.search {
        for hit in &search.hits {
            if hit.to <= hit.from {
                continue;
            }
            let b = Bounds::new(
                at(hit.from as usize, hit.row as usize),
                size(cell_w * (hit.to - hit.from) as f32, cell_h),
            );
            let color = if hit.selected {
                search_active
            } else {
                search_hit
            };
            window.paint_quad(fill(snap_box(b), color));
        }
    }
    for (y, (_, ops)) in cache.rows.iter().enumerate() {
        for op in ops {
            match op {
                RowOp::Bg { .. } => {}
                RowOp::Sprite { x, text, color } => {
                    sprites::paint(text, Bounds::new(at(*x, y), cell_bounds), *color, window);
                }
                RowOp::Text { x, line } => {
                    let _ = line.paint(at(*x, y), cell_h, TextAlign::Left, None, window, cx);
                }
            }
        }
    }
    drop(cache);

    // The hovered link's underline sits on the bottom edge of its cells, over
    // the text, so it reads as a link without recoloring it.
    for span in link {
        if span.to <= span.from || usize::from(span.row) >= snapshot.cells.len() {
            continue;
        }
        let bounds = Bounds::new(
            at(usize::from(span.from), usize::from(span.row)) + point(px(0.), cell_h - px(1.5)),
            size(cell_w * f32::from(span.to - span.from), px(1.5)),
        );
        window.paint_quad(fill(snap_box(bounds), paint_color(snapshot.fg)));
    }

    let mut cursor_bounds = None;
    if let Some(cur) = snapshot.cursor {
        let (x, y) = (cur.x as usize, cur.y as usize);
        let wide = snapshot
            .cells
            .get(y)
            .and_then(|row| row.get(x))
            .is_some_and(|c| c.wide);
        let w = cell_w * if wide { 2.0 } else { 1.0 };
        let color = paint_color(cur.color);
        let p = at(x, y);
        cursor_bounds = Some(Bounds::new(p, size(w, cell_h)));
        match (focused, cur.shape) {
            (true, CursorShape::Block) => {
                window.paint_quad(fill(snap_box(Bounds::new(p, size(w, cell_h))), color));
                if let Some(c) = snapshot
                    .cells
                    .get(y)
                    .and_then(|row| row.get(x))
                    .filter(|c| !c.text.is_empty())
                {
                    let run = TextRun {
                        len: c.text.len(),
                        font: cell_font(typeface, c.bold, c.italic),
                        color: paint_color(snapshot.bg),
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    };
                    let line = window.text_system().shape_line(
                        c.text.clone().into(),
                        px(typeface.size),
                        &[run],
                        None,
                    );
                    let _ = line.paint(p, cell_h, TextAlign::Left, None, window, cx);
                }
            }
            (true, CursorShape::Bar) => {
                window.paint_quad(fill(snap_box(Bounds::new(p, size(px(2.0), cell_h))), color));
            }
            (true, CursorShape::Underline) => {
                window.paint_quad(fill(
                    snap_box(Bounds::new(
                        p + point(px(0.0), cell_h - px(2.0)),
                        size(w, px(2.0)),
                    )),
                    color,
                ));
            }
            _ => {
                window.paint_quad(outline(
                    snap_box(Bounds::new(p, size(w, cell_h))),
                    color,
                    BorderStyle::Solid,
                ));
            }
        }
    } else if let Some((x, y)) = snapshot.cursor_cell {
        // Hidden cursor still hosts the IME preview.
        cursor_bounds = Some(Bounds::new(
            at(x as usize, y as usize),
            size(cell_w, cell_h),
        ));
    }

    // Composition preview at the cursor, underlined on the terminal background.
    if let (Some(bounds), Some(text)) = (cursor_bounds, marked.filter(|t| !t.is_empty())) {
        let fg = paint_color(marked_fg(snapshot, text));
        let run = TextRun {
            len: text.len(),
            font: cell_font(typeface, false, false),
            color: fg,
            background_color: None,
            underline: Some(UnderlineStyle {
                thickness: px(1.0),
                color: Some(fg),
                wavy: false,
            }),
            strikethrough: None,
        };
        let line = window
            .text_system()
            .shape_line(text.into(), px(typeface.size), &[run], None);
        window.paint_quad(fill(
            snap_box(Bounds::new(bounds.origin, size(line.width, cell_h))),
            paint_color(snapshot.bg),
        ));
        let _ = line.paint(bounds.origin, cell_h, TextAlign::Left, None, window, cx);
    }

    cursor_at.set(CursorAt {
        bounds: cursor_bounds,
        cell: (cell_w, cell_h),
    });
    origin
}

#[cfg(test)]
mod tests {
    use core::prelude::v1::test;

    use super::{contrast_ratio, marked_fg, readable_fg};
    use crate::term::{Rgb, Snapshot};

    #[test]
    fn hidden_cursor_ime_preview_uses_the_terminal_foreground() {
        // No cursor means no color to inherit: the default foreground must
        // survive the light theme instead of the old fixed gray.
        let light = Snapshot::blank_with("", "", Rgb(0x40, 0x3f, 0x44), Rgb(0xf9, 0xf9, 0xf8));
        let fg = marked_fg(&light, "wo'shi'shei");
        assert_eq!(fg, Rgb(0x40, 0x3f, 0x44));
        assert!(contrast_ratio(fg, light.bg) >= 4.5);

        let dark = Snapshot::blank_with("", "", Rgb(0xd7, 0xda, 0xdf), Rgb(0x22, 0x25, 0x2b));
        assert_eq!(marked_fg(&dark, "nihao"), Rgb(0xd7, 0xda, 0xdf));
    }

    #[test]
    fn pale_terminal_text_gains_contrast_without_losing_its_color() {
        let bg = Rgb(0xf9, 0xf9, 0xf8);
        // Theme gray and ANSI accents, plus a truecolor gray supplied by a TUI.
        for fg in [
            Rgb(0x8a, 0x8b, 0x90),
            Rgb(0xd1, 0x68, 0x6f),
            Rgb(0x7f, 0xae, 0x6a),
            Rgb(0xd5, 0xab, 0x60),
            Rgb(0x94, 0xa4, 0xf5),
            Rgb(0xbc, 0x74, 0xd2),
            Rgb(0x56, 0xb6, 0xc2),
            Rgb(0xaa, 0xaa, 0xaa),
        ] {
            let adjusted = readable_fg(fg, bg, "text");
            assert!(contrast_ratio(adjusted, bg) >= 4.5, "{fg:?}: {adjusted:?}");
            assert_ne!(adjusted, Rgb(0x1b, 0x1b, 0x1e));
            assert_eq!(fg.0.cmp(&fg.1), adjusted.0.cmp(&adjusted.1));
            assert_eq!(fg.1.cmp(&fg.2), adjusted.1.cmp(&adjusted.2));
        }
    }

    #[test]
    fn readable_text_and_terminal_graphics_keep_their_colors() {
        let bg = Rgb(0xf9, 0xf9, 0xf8);
        let body = Rgb(0x40, 0x3f, 0x44);
        assert_eq!(readable_fg(body, bg, "text"), body);
        let pale = Rgb(0xe0, 0xe0, 0xe0);
        for graphic in ["─", "█", "⠿", "\u{e0b0}"] {
            assert_eq!(readable_fg(pale, bg, graphic), pale);
        }
    }

    #[test]
    fn dark_backgrounds_keep_the_existing_contrast_behavior() {
        let bg = Rgb(0x22, 0x25, 0x2b);
        let gray = Rgb(0x7c, 0x82, 0x8c);
        assert_eq!(readable_fg(gray, bg, "text"), gray);
        assert_eq!(readable_fg(bg, bg, "text"), Rgb(0xf2, 0xf2, 0xf4));
    }
}
