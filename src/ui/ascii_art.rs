//! Retro-mode image rendering: classic luminance-ramp ASCII art in the 16 ANSI colors.
//!
//! Retro mode targets terminals with no graphics protocol and a 256-glyph font — there the
//! normal art path fails twice: half-block cells need truecolor SGR the console may not
//! honor, and the frame scrubber would flatten them into `#` mush anyway. So retro mode
//! draws images the way the 90s did: one printable-ASCII glyph per cell graded by
//! brightness, tinted with the nearest of the 16 ANSI palette entries every console
//! carries. The grid is rebuilt only when the image identity or the target size changes;
//! rendering a cached grid is a plain cell blit.

use std::cell::RefCell;
use std::collections::HashMap;

use image::DynamicImage;
use image::imageops::FilterType;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Color;

/// Dark→bright glyph ramp — the classic ten-step ASCII-art ramp. Index 0 is a space so the
/// darkest cells melt into the background instead of printing noise.
const RAMP: &[u8] = b" .:-=+*#%@";

/// One cache slot per drawing surface, so the player art and the About icon (both visible
/// while the About card floats over the player) don't evict each other every frame.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum Slot {
    AlbumArt,
    AboutIcon,
}

struct Cached {
    id: String,
    width: u16,
    height: u16,
    cells: Vec<(char, Color)>,
}

thread_local! {
    static CACHE: RefCell<HashMap<Slot, Cached>> = RefCell::new(HashMap::new());
}

/// Draw `img` as ASCII art filling `rect`. `id` names the image (track id, asset name); the
/// cell grid is rebuilt only when it or the rect size changes.
pub fn render_image(frame: &mut Frame, slot: Slot, id: &str, img: &DynamicImage, rect: Rect) {
    render_cached(frame, slot, id, rect, || Some(img.clone()));
}

/// Like [`render_image`], but decodes `png` lazily — only when the cache slot is stale — so
/// an embedded asset isn't re-decoded per frame.
pub fn render_png(frame: &mut Frame, slot: Slot, id: &str, png: &[u8], rect: Rect) {
    render_cached(frame, slot, id, rect, || image::load_from_memory(png).ok());
}

fn render_cached(
    frame: &mut Frame,
    slot: Slot,
    id: &str,
    rect: Rect,
    source: impl FnOnce() -> Option<DynamicImage>,
) {
    if rect.width == 0 || rect.height == 0 {
        return;
    }
    CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        let stale = cache
            .get(&slot)
            .is_none_or(|c| c.id != id || c.width != rect.width || c.height != rect.height);
        if stale {
            let Some(img) = source() else {
                return;
            };
            cache.insert(
                slot,
                Cached {
                    id: id.to_owned(),
                    width: rect.width,
                    height: rect.height,
                    cells: build(&img, rect.width, rect.height),
                },
            );
        }
        let cells = &cache.get(&slot).expect("inserted above").cells;
        let buf = frame.buffer_mut();
        for y in 0..rect.height {
            for x in 0..rect.width {
                let (ch, color) = cells[usize::from(y) * usize::from(rect.width) + usize::from(x)];
                if ch == ' ' {
                    continue; // let the background through — blank is part of the art
                }
                if let Some(cell) = buf.cell_mut((rect.x + x, rect.y + y)) {
                    cell.set_char(ch).set_fg(color);
                }
            }
        }
    });
}

/// Sample the image at one pixel per cell (the target rect already encodes the terminal's
/// cell aspect) and grade each sample into a ramp glyph plus an ANSI-16 tint. Brightness is
/// auto-levelled between the 5th and 95th luminance percentiles so dim covers still show
/// their structure instead of a near-blank grid.
fn build(img: &DynamicImage, w: u16, h: u16) -> Vec<(char, Color)> {
    let rgba = img
        .resize_exact(u32::from(w), u32::from(h), FilterType::Triangle)
        .to_rgba8();
    let lumas: Vec<f32> = rgba
        .pixels()
        .map(|p| {
            // Alpha-scaled so transparent regions (icon corners) read as darkness → space.
            let a = f32::from(p[3]) / 255.0;
            (0.2126 * f32::from(p[0]) + 0.7152 * f32::from(p[1]) + 0.0722 * f32::from(p[2])) / 255.0
                * a
        })
        .collect();
    let mut sorted = lumas.clone();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let lo = sorted[sorted.len() * 5 / 100];
    let hi = sorted[(sorted.len() * 95 / 100).min(sorted.len() - 1)];
    let span = (hi - lo).max(0.05);

    rgba.pixels()
        .zip(&lumas)
        .map(|(p, &luma)| {
            let t = ((luma - lo) / span).clamp(0.0, 1.0);
            let idx = (t * (RAMP.len() - 1) as f32).round() as usize;
            (
                RAMP[idx.min(RAMP.len() - 1)] as char,
                ansi_color(p[0], p[1], p[2], t),
            )
        })
        .collect()
}

/// The nearest ANSI-16 tint for a cell. The glyph already carries brightness, so color only
/// needs the hue family plus a normal/bright split: near-gray cells grade DarkGray→Gray→White,
/// chromatic cells are brightness-normalized (hue preserved) and matched against the six VGA
/// hue families. Black is never returned — a visible glyph on a dark console must stay visible.
fn ansi_color(r: u8, g: u8, b: u8, luma: f32) -> Color {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    if max.saturating_sub(min) < 32 || max == 0 {
        return if luma > 0.8 {
            Color::White
        } else if luma > 0.4 {
            Color::Gray
        } else {
            Color::DarkGray
        };
    }
    let scale = 255.0 / f32::from(max);
    let (rs, gs, bs) = (
        f32::from(r) * scale,
        f32::from(g) * scale,
        f32::from(b) * scale,
    );
    // Bright-variant VGA reference points per hue family (normal, bright picked by level).
    const HUES: [((f32, f32, f32), Color, Color); 6] = [
        ((255.0, 85.0, 85.0), Color::Red, Color::LightRed),
        ((85.0, 255.0, 85.0), Color::Green, Color::LightGreen),
        ((255.0, 255.0, 85.0), Color::Yellow, Color::LightYellow),
        ((85.0, 85.0, 255.0), Color::Blue, Color::LightBlue),
        ((255.0, 85.0, 255.0), Color::Magenta, Color::LightMagenta),
        ((85.0, 255.0, 255.0), Color::Cyan, Color::LightCyan),
    ];
    let dist = |x: (f32, f32, f32)| (rs - x.0).powi(2) + (gs - x.1).powi(2) + (bs - x.2).powi(2);
    // A pale color lands closer to white than to any hue point once normalized.
    let mut best = (
        dist((255.0, 255.0, 255.0)),
        if luma > 0.6 {
            Color::White
        } else {
            Color::Gray
        },
    );
    for &(ref_pt, normal, bright) in &HUES {
        let d = dist(ref_pt);
        if d < best.0 {
            best = (d, if luma > 0.55 { bright } else { normal });
        }
    }
    best.1
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgba, RgbaImage};

    fn flat(rgba: [u8; 4], w: u32, h: u32) -> DynamicImage {
        DynamicImage::ImageRgba8(RgbaImage::from_pixel(w, h, Rgba(rgba)))
    }

    #[test]
    fn cells_are_printable_ascii_only() {
        let img = flat([180, 40, 40, 255], 64, 64);
        for (ch, _) in build(&img, 20, 10) {
            assert!(ch.is_ascii() && !ch.is_ascii_control(), "{ch:?}");
        }
    }

    #[test]
    fn grid_matches_requested_dimensions() {
        let img = flat([10, 200, 90, 255], 64, 64);
        assert_eq!(build(&img, 33, 7).len(), 33 * 7);
    }

    #[test]
    fn transparent_pixels_become_blanks() {
        // A fully transparent image must render as spaces (the popup/panel background),
        // not as a solid block of bright glyphs.
        let img = flat([255, 255, 255, 0], 16, 16);
        assert!(build(&img, 8, 4).iter().all(|&(ch, _)| ch == ' '));
    }

    #[test]
    fn hue_families_map_to_their_ansi_colors() {
        assert!(matches!(
            ansi_color(200, 30, 30, 0.9),
            Color::LightRed | Color::Red
        ));
        assert!(matches!(
            ansi_color(30, 200, 30, 0.3),
            Color::Green | Color::LightGreen
        ));
        assert!(matches!(
            ansi_color(40, 40, 220, 0.9),
            Color::LightBlue | Color::Blue
        ));
        // Near-gray stays in the gray family, and black is never produced.
        assert!(matches!(
            ansi_color(120, 120, 125, 0.5),
            Color::Gray | Color::DarkGray | Color::White
        ));
    }
}
