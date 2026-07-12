//! Light ambient canvas effects (the second wave): shooting-star comets, snowfall,
//! fireflies, a rotating wireframe cube, an ASCII aquarium and layered ocean waves.
//!
//! Same contract as the first-wave canvas effects in the parent module: drawn only into the
//! blank filler `zone`, every glyph is display-width 1, all phase derives from
//! [`App::anim_frame`] (`f`) plus [`hash32`] so frames are deterministic and resume cleanly,
//! and each effect degrades in retro mode either at the source (forked glyph sets) or through
//! the CP437 scrubber. Nothing here allocates per frame beyond tiny fixed buffers.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

use super::{hash32, lerp_color, put_char, wave};
use crate::app::App;
use crate::theme::ThemeRole as R;

// ── comets ──────────────────────────────────────────────────────────────────

/// Occasional shooting stars: up to two diagonal streaks with fading tails, each on its own
/// hashed launch cycle so the sky stays mostly calm with a rare bright crossing.
pub(super) fn comets(frame: &mut Frame, app: &App, zone: Rect, f: u64) {
    let w = i64::from(zone.width);
    let h = i64::from(zone.height);
    if w < 10 || h < 4 {
        return;
    }
    let head_color = Color::Rgb(255, 255, 255);
    let tail_base = app.theme.color(R::AccentAlt);
    let dim = app.theme.color(R::TextSubtle);
    let (head_glyph, tail_glyphs): (char, [char; 3]) = if app.retro_mode() {
        ('*', ['+', '.', '.'])
    } else {
        ('✦', ['✧', '·', '·'])
    };
    let buf = frame.buffer_mut();
    for slot in 0..2u64 {
        // Each slot fires once per cycle; most of the cycle is empty sky.
        let period = 140 + u64::from(hash32(slot * 13 + 5) % 160);
        let cycle = f / period + slot; // +slot desyncs the two launchers
        let t = (f % period) as i64;
        let travel = w / 2 + h; // enough steps to cross and leave the zone
        if t >= travel {
            continue;
        }
        // Launch geometry hashed per cycle: start somewhere on the top edge, fall down-right
        // or down-left (2 columns per row keeps the streak at a classic shallow angle).
        let start_x = i64::from(hash32(cycle.wrapping_mul(31) + slot) % u32::from(zone.width));
        let dir: i64 = if hash32(cycle.wrapping_mul(17) + slot).is_multiple_of(2) {
            1
        } else {
            -1
        };
        for k in 0..4i64 {
            let step = t - k;
            if step < 0 {
                continue;
            }
            let x = start_x + dir * step * 2;
            let y = step;
            if x < 0 || x >= w || y >= h {
                continue;
            }
            let (glyph, color) = if k == 0 {
                (head_glyph, head_color)
            } else {
                let fade = (k - 1) as f64 / 2.0;
                (
                    tail_glyphs[(k - 1) as usize],
                    lerp_color(tail_base, dim, fade),
                )
            };
            put_char(
                buf,
                (i64::from(zone.left()) + x) as u16,
                (i64::from(zone.top()) + y) as u16,
                glyph,
                Style::default().fg(color),
            );
        }
    }
}

// ── snow ────────────────────────────────────────────────────────────────────

const SNOW_GLYPHS: [char; 4] = ['❄', '*', '·', '.'];
const SNOW_GLYPHS_RETRO: [char; 4] = ['*', '*', '.', '.'];

/// Sparse snowfall: flakes drift down at hashed speeds with a gentle sinusoidal wobble,
/// heavier flakes reading brighter and faster than the fine ones behind them.
pub(super) fn snow(frame: &mut Frame, app: &App, zone: Rect, f: u64) {
    let w = u32::from(zone.width);
    let h = u64::from(zone.height);
    if w < 4 || h < 2 {
        return;
    }
    let count = ((w * u32::from(zone.height)) / 40).clamp(8, 50);
    let glyphs = if app.retro_mode() {
        &SNOW_GLYPHS_RETRO
    } else {
        &SNOW_GLYPHS
    };
    let bright = Color::Rgb(235, 240, 250);
    let dim = app.theme.color(R::TextSubtle);
    let buf = frame.buffer_mut();
    for i in 0..u64::from(count) {
        // Size class picks glyph, speed and brightness together (big flakes fall faster).
        let class = (hash32(i * 3 + 7) % 4) as usize;
        let speed = 6 - class as u64; // frames per row: fine flakes are the slowest
        let seed = u64::from(hash32(i * 5 + 1));
        let yv = (f / speed + seed) % (h + 5);
        if yv >= h {
            continue; // brief off-screen gap so the field shifts instead of looping rigidly
        }
        let base_x = i64::from(hash32(i * 7 + 3) % w);
        let sway = (wave(f + seed, 46 + (i % 5) * 7) - 0.5) * 3.0;
        let x = base_x + sway.round() as i64;
        if x < 0 || x >= i64::from(w) {
            continue;
        }
        let color = lerp_color(dim, bright, class as f64 / 3.0);
        put_char(
            buf,
            zone.left() + x as u16,
            zone.top() + yv as u16,
            glyphs[3 - class],
            Style::default().fg(color),
        );
    }
}

// ── fireflies ───────────────────────────────────────────────────────────────

/// Fireflies wandering on smooth per-fly Lissajous paths, each breathing between the subtle
/// text colour and a warm glow on its own phase — the calm counterpart to the starfield.
pub(super) fn fireflies(frame: &mut Frame, app: &App, zone: Rect, f: u64) {
    let w = f64::from(zone.width);
    let h = f64::from(zone.height);
    if zone.width < 8 || zone.height < 3 {
        return;
    }
    let count = ((u32::from(zone.width) * u32::from(zone.height)) / 60).clamp(6, 24);
    let dim = app.theme.color(R::TextSubtle);
    let glow = app.theme.color(R::Warning);
    let (bright_glyph, dim_glyph) = if app.retro_mode() {
        ('o', '.')
    } else {
        ('●', '·')
    };
    let buf = frame.buffer_mut();
    for i in 0..u64::from(count) {
        // Two incommensurate angular speeds per axis make the path wander without looping
        // visibly; amplitudes keep every fly comfortably inside the zone.
        let s = |k: u64| f64::from(hash32(i * 11 + k));
        let wx = 0.008 + (s(1) % 100.0) / 9000.0;
        let wy = 0.011 + (s(2) % 100.0) / 8000.0;
        let px = (s(3) % 628.0) / 100.0;
        let py = (s(4) % 628.0) / 100.0;
        let x = (w / 2.0) + (f as f64 * wx + px).sin() * (w / 2.0 - 1.0);
        let y = (h / 2.0) + (f as f64 * wy + py).sin() * (h / 2.0 - 0.6);
        let xi = x.floor().clamp(0.0, w - 1.0) as u16;
        let yi = y.floor().clamp(0.0, h - 1.0) as u16;
        // Slow personal breath; fully dark part of the cycle leaves the cell alone.
        let b = wave(f + u64::from(hash32(i * 17 + 9) % 120), 110);
        if b < 0.2 {
            continue;
        }
        let glyph = if b > 0.62 { bright_glyph } else { dim_glyph };
        put_char(
            buf,
            zone.left() + xi,
            zone.top() + yi,
            glyph,
            Style::default().fg(lerp_color(dim, glow, b)),
        );
    }
}

// ── wireframe cube ──────────────────────────────────────────────────────────

/// The cube's 8 corners in model space.
const CUBE_VERTS: [(f64, f64, f64); 8] = [
    (-1.0, -1.0, -1.0),
    (1.0, -1.0, -1.0),
    (1.0, 1.0, -1.0),
    (-1.0, 1.0, -1.0),
    (-1.0, -1.0, 1.0),
    (1.0, -1.0, 1.0),
    (1.0, 1.0, 1.0),
    (-1.0, 1.0, 1.0),
];

/// The 12 edges as vertex-index pairs.
const CUBE_EDGES: [(usize, usize); 12] = [
    (0, 1),
    (1, 2),
    (2, 3),
    (3, 0),
    (4, 5),
    (5, 6),
    (6, 7),
    (7, 4),
    (0, 4),
    (1, 5),
    (2, 6),
    (3, 7),
];

/// A rotating 3-D wireframe cube, perspective-projected into the middle of the zone — the
/// donut's angular little sibling at a fraction of its cost (12 edge rasters instead of a
/// full solid-of-revolution sweep). Edge brightness follows depth so the near face pops.
pub(super) fn cube(frame: &mut Frame, app: &App, zone: Rect, f: u64) {
    let w = f64::from(zone.width);
    let h = f64::from(zone.height);
    if zone.width < 12 || zone.height < 6 {
        return;
    }
    let (sa, ca) = (f as f64 * 0.030).sin_cos();
    let (sb, cb) = (f as f64 * 0.019).sin_cos();
    let dist = 4.2;
    let scale = (w * 0.28).min(h * 0.56);
    let near = app.theme.color(R::Accent);
    let far = app.theme.color(R::TextSubtle);
    let (edge_glyph, vert_glyph) = if app.retro_mode() {
        ('+', 'O')
    } else {
        ('·', '●')
    };

    // Rotate (Y then X) and project each vertex once.
    let mut proj = [(0.0f64, 0.0f64, 0.0f64); 8];
    for (i, &(x, y, z)) in CUBE_VERTS.iter().enumerate() {
        let (xr, zr) = (x * cb + z * sb, -x * sb + z * cb);
        let (yr, zr) = (y * ca - zr * sa, y * sa + zr * ca);
        let zc = zr + dist;
        let px = w / 2.0 + scale * xr / zc;
        let py = h / 2.0 - scale * 0.5 * yr / zc;
        proj[i] = (px, py, zc);
    }

    let buf = frame.buffer_mut();
    let mut plot = |x: f64, y: f64, depth: f64, glyph: char, bold: bool| {
        if x < 0.0 || y < 0.0 || x >= w || y >= h {
            return;
        }
        // depth runs ~[dist-√3, dist+√3]; map near→bright.
        let t = ((depth - (dist - 1.8)) / 3.6).clamp(0.0, 1.0);
        let mut style = Style::default().fg(lerp_color(near, far, t));
        if bold {
            style = style.add_modifier(Modifier::BOLD);
        }
        put_char(
            buf,
            zone.left() + x as u16,
            zone.top() + y as u16,
            glyph,
            style,
        );
    };

    for &(a, b) in &CUBE_EDGES {
        let (x0, y0, z0) = proj[a];
        let (x1, y1, z1) = proj[b];
        let steps = ((x1 - x0).abs().max((y1 - y0).abs()).ceil() as usize).max(1);
        for s in 0..=steps {
            let t = s as f64 / steps as f64;
            plot(
                x0 + (x1 - x0) * t,
                y0 + (y1 - y0) * t,
                z0 + (z1 - z0) * t,
                edge_glyph,
                false,
            );
        }
    }
    for &(x, y, z) in &proj {
        plot(x, y, z, vert_glyph, true);
    }
}

// ── aquarium ────────────────────────────────────────────────────────────────

const FISH_RIGHT: [&str; 3] = ["><((º>", "><>", "><((('>"];
const FISH_LEFT: [&str; 3] = ["<º))><", "<><", "<')))><"];
const FISH_RIGHT_RETRO: [&str; 3] = ["><((o>", "><>", "><(('>"];
const FISH_LEFT_RETRO: [&str; 3] = ["<o))><", "<><", "<')><"];
const FISH_PALETTE: [R; 4] = [R::Accent, R::AccentAlt, R::Success, R::Warning];
const BUBBLE_GLYPHS: [char; 3] = ['°', 'º', '·'];
const BUBBLE_GLYPHS_RETRO: [char; 3] = ['o', 'o', '.'];

/// A little ASCII aquarium: fish cruise across on their own lanes (both directions, hashed
/// species/speed/colour), while bubbles wobble up from the depths.
pub(super) fn aquarium(frame: &mut Frame, app: &App, zone: Rect, f: u64) {
    let w = i64::from(zone.width);
    let h = u64::from(zone.height);
    if w < 14 || h < 3 {
        return;
    }
    let retro = app.retro_mode();
    let buf = frame.buffer_mut();

    // Fish: one per ~3 rows, capped so a huge zone doesn't become a trawler's net.
    let fish_count = (h / 3).clamp(2, 5);
    for i in 0..fish_count {
        let rightward = hash32(i * 7 + 2).is_multiple_of(2);
        let species = (hash32(i * 11 + 4) % 3) as usize;
        let sprite: &str = match (rightward, retro) {
            (true, false) => FISH_RIGHT[species],
            (true, true) => FISH_RIGHT_RETRO[species],
            (false, false) => FISH_LEFT[species],
            (false, true) => FISH_LEFT_RETRO[species],
        };
        let len = sprite.chars().count() as i64;
        let lane = (u64::from(hash32(i * 5 + 1)) % h) as u16;
        let speed = 2 + u64::from(hash32(i * 3 + 8) % 3);
        let span = (w + 2 * len) as u64;
        let pos = ((f / speed + u64::from(hash32(i * 13 + 6))) % span) as i64 - len;
        let x0 = if rightward { pos } else { w - len - pos };
        // A subtle vertical bob so the tank feels alive without lane collisions.
        let bob = if wave(f + i * 9, 70) > 0.75 && lane + 1 < zone.height {
            1
        } else {
            0
        };
        let color = app
            .theme
            .color(FISH_PALETTE[(hash32(i * 17) as usize) % FISH_PALETTE.len()]);
        for (k, ch) in sprite.chars().enumerate() {
            let x = x0 + k as i64;
            if x < 0 || x >= w {
                continue;
            }
            put_char(
                buf,
                zone.left() + x as u16,
                zone.top() + lane + bob,
                ch,
                Style::default().fg(color),
            );
        }
    }

    // Bubbles: thin columns of air wobbling upward, popping just under the surface.
    let bubbles = (u32::from(zone.width) / 8).clamp(4, 12);
    let dim = app.theme.color(R::TextSubtle);
    let bright = app.theme.color(R::AccentAlt);
    let glyphs = if retro {
        &BUBBLE_GLYPHS_RETRO
    } else {
        &BUBBLE_GLYPHS
    };
    for i in 0..u64::from(bubbles) {
        let seed = u64::from(hash32(i * 19 + 3));
        let speed = 2 + u64::from(hash32(i * 23 + 5) % 3);
        let yv = (f / speed + seed) % (h + 3);
        if yv >= h {
            continue;
        }
        let y = (h - 1 - yv) as u16;
        let base_x = i64::from(hash32(i * 29 + 7) % u32::from(zone.width));
        let x = base_x + ((wave(f + seed, 24) - 0.5) * 2.0).round() as i64;
        if x < 0 || x >= w {
            continue;
        }
        // Bubbles grow as they rise: dot → ring near the surface.
        let stage = ((yv * 3) / h.max(1)).min(2) as usize;
        put_char(
            buf,
            zone.left() + x as u16,
            zone.top() + y,
            glyphs[2 - stage],
            Style::default().fg(lerp_color(dim, bright, yv as f64 / h as f64)),
        );
    }
}

// ── ocean waves ─────────────────────────────────────────────────────────────

/// Layered ocean swell along the bottom of the zone: two-to-four out-of-phase sine crests,
/// deeper layers darker and slower, with sparse foam flecks on the top crest.
pub(super) fn waves(frame: &mut Frame, app: &App, zone: Rect, f: u64) {
    let w = zone.width;
    if w < 8 || zone.height < 3 {
        return;
    }
    let layers = (zone.height / 4).clamp(2, 4);
    let deep = app.theme.color(R::GaugeFilled);
    let surf = app.theme.color(R::Accent);
    let foam_color = Color::Rgb(235, 245, 255);
    let (crest_glyph, foam_glyph) = if app.retro_mode() {
        ('~', '*')
    } else {
        ('≈', '✦')
    };
    let bottom = zone.bottom() - 1;
    let top = zone.top();
    let buf = frame.buffer_mut();
    for layer in 0..layers {
        // Front layers ride higher amplitude and faster phase; each gets its own base row.
        let li = f64::from(layer);
        let base = i32::from(bottom) - i32::from(layer) * 2;
        let amp = 0.8 + li * 0.35;
        let freq = 0.22 - li * 0.03;
        let phase = f as f64 * (0.06 + li * 0.02);
        let color = lerp_color(surf, deep, li / f64::from(layers.max(1)));
        let style = Style::default().fg(color);
        for x in 0..w {
            let sway = (f64::from(x) * freq + phase).sin() * amp;
            let y = base - sway.round() as i32;
            if y < i32::from(top) || y > i32::from(bottom) {
                continue;
            }
            put_char(buf, zone.left() + x, y as u16, crest_glyph, style);
            // Foam: rare bright flecks dancing on the front crest only.
            if layer == 0
                && hash32(u64::from(x) * 37 + f / 8).is_multiple_of(23)
                && y > i32::from(top)
            {
                put_char(
                    buf,
                    zone.left() + x,
                    (y - 1) as u16,
                    foam_glyph,
                    Style::default().fg(foam_color),
                );
            }
        }
    }
}
