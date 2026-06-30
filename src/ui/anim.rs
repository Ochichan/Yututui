//! Optional player-window animations. Every effect is individually gated by an
//! [`AnimationsConfig`](crate::config::AnimationsConfig) flag plus the global `master` switch;
//! with all flags off nothing here ever runs (the per-frame clock in `main.rs` stays asleep, so
//! the player renders byte-for-byte like a build with this module absent — the app's "fast and
//! light" identity is preserved).
//!
//! Two families:
//! * **Element-level** — restyle existing widgets in place (title shimmer, beating heart,
//!   breathing border, control pulse, seekbar comet, status-line spinner / EQ bars). These return
//!   a tweaked `Style`/`Line` or draw a small overlay; when their flag is off they return the
//!   plain value, so the call site stays a one-liner.
//! * **Canvas** — drawn straight into the back buffer in the blank filler zone only (never over
//!   album art or lyrics): matrix rain, spinning donut, faux visualizer, starfield, bouncing logo.
//!
//! All phase comes from [`App::anim_frame`] (a frame counter frozen while paused), so the effects
//! are deterministic and resume cleanly. Canvas glyphs are all display-width 1, so direct cell
//! writes never drift the layout.

use std::cell::RefCell;
use std::sync::OnceLock;

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::App;
use crate::theme::ThemeRole as R;

// ── shared helpers ──────────────────────────────────────────────────────────

/// A smooth 0→1→0 pulse over `period` frames (0 at the ends, 1 at the middle).
fn wave(frame: u64, period: u64) -> f64 {
    let p = period.max(1);
    let t = (frame % p) as f64 / p as f64;
    0.5 - 0.5 * (std::f64::consts::TAU * t).cos()
}

/// Deterministic per-cell scramble (splitmix64 finaliser) — used so rain columns / stars keep a
/// stable identity across frames without any RNG state.
fn hash32(x: u64) -> u32 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    ((z ^ (z >> 31)) & 0xFFFF_FFFF) as u32
}

/// Resolve any [`Color`] to RGB so two theme colours can be blended. Named/indexed colours fall
/// back to a neutral grey (themes here are RGB, so the precise table only matters for safety).
fn to_rgb(c: Color) -> (u8, u8, u8) {
    match c {
        Color::Rgb(r, g, b) => (r, g, b),
        Color::Black => (0, 0, 0),
        Color::Red => (205, 49, 49),
        Color::Green => (13, 188, 121),
        Color::Yellow => (229, 229, 16),
        Color::Blue => (36, 114, 200),
        Color::Magenta => (188, 63, 188),
        Color::Cyan => (17, 168, 205),
        Color::Gray => (118, 118, 118),
        Color::DarkGray => (84, 84, 84),
        Color::LightRed => (241, 76, 76),
        Color::LightGreen => (35, 209, 139),
        Color::LightYellow => (245, 245, 67),
        Color::LightBlue => (59, 142, 234),
        Color::LightMagenta => (214, 112, 214),
        Color::LightCyan => (41, 184, 219),
        Color::White => (229, 229, 229),
        _ => (160, 160, 160),
    }
}

/// Linear blend between two colours (`t` clamped to `0.0..=1.0`).
fn lerp_color(a: Color, b: Color, t: f64) -> Color {
    let t = t.clamp(0.0, 1.0);
    let (ar, ag, ab) = to_rgb(a);
    let (br, bg, bb) = to_rgb(b);
    let mix = |x: u8, y: u8| (f64::from(x) + (f64::from(y) - f64::from(x)) * t).round() as u8;
    Color::Rgb(mix(ar, br), mix(ag, bg), mix(ab, bb))
}

/// Write a single character into one back-buffer cell (no-op if out of bounds). `set_style`
/// patches, so a fg-only style leaves the existing background intact.
fn put_char(buf: &mut Buffer, x: u16, y: u16, ch: char, style: Style) {
    if let Some(cell) = buf.cell_mut((x, y)) {
        cell.set_char(ch).set_style(style);
    }
}

/// Extract the `[start_col, start_col + width)` display-column slice of `s` (CJK-aware), used for
/// the title marquee window.
fn col_window(s: &str, start_col: usize, width: usize) -> String {
    let mut out = String::new();
    let mut col = 0usize;
    let mut taken = 0usize;
    for ch in s.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if col < start_col {
            col += w;
            continue;
        }
        if taken + w > width {
            break;
        }
        out.push(ch);
        taken += w;
    }
    out
}

// ── element-level effects ───────────────────────────────────────────────────

/// Build the animated now-playing title line, or `None` when neither the title-shimmer nor the
/// heart-pulse flag is on (the caller then renders its plain title unchanged). Handles three
/// combinations: heart-only (pulse the `♥`, plain body), title-only (shimmer + marquee, static
/// heart) and both.
pub fn title_line(
    app: &App,
    title: &str,
    artist: &str,
    liked: bool,
    width: u16,
) -> Option<Line<'static>> {
    let a = app.animations();
    if !(a.master && (a.title || a.heart)) {
        return None;
    }
    let f = app.anim_frame();
    let mut spans: Vec<Span<'static>> = Vec::new();

    // Leading heart marker (only when the track is favourited), pulsing if `heart` is on.
    if liked {
        let style = if a.heart {
            let t = wave(f, 28);
            Style::default()
                .fg(lerp_color(
                    app.theme.color(R::Error),
                    app.theme.color(R::AccentAlt),
                    t,
                ))
                .add_modifier(Modifier::BOLD)
        } else {
            app.theme.style(R::TextPrimary).add_modifier(Modifier::BOLD)
        };
        spans.push(Span::styled("♥ ", style));
    }

    let body = format!("{title} — {artist}");
    if a.title {
        let base = app.theme.color(R::TextPrimary);
        let bright = app.theme.color(R::Accent);
        let used = if liked { 2 } else { 0 };
        let avail = (width as usize).saturating_sub(used);
        let total = UnicodeWidthStr::width(body.as_str());

        // Long titles scroll; short ones sit still. `•`-separated wrap-around for a clean loop.
        let window = if total > avail && avail > 4 {
            let loop_s = format!("{body}   •   ");
            let period = UnicodeWidthStr::width(loop_s.as_str()).max(1);
            let start = (f / 4) as usize % period;
            col_window(&format!("{loop_s}{loop_s}"), start, avail)
        } else {
            body
        };

        // A bright band sweeps across the characters (shimmer).
        let chars: Vec<char> = window.chars().collect();
        let span_len = chars.len() as f64 + 8.0;
        let head = (f as f64 / 2.0) % span_len;
        let mut col = 0.0f64;
        for ch in chars {
            let d = (col - head).abs();
            let b = (1.0 - d / 3.0).clamp(0.0, 1.0);
            spans.push(Span::styled(
                ch.to_string(),
                Style::default()
                    .fg(lerp_color(base, bright, b))
                    .add_modifier(Modifier::BOLD),
            ));
            col += UnicodeWidthChar::width(ch).unwrap_or(1) as f64;
        }
    } else {
        // Heart-only: the body stays exactly as the plain path would render it.
        spans.push(Span::styled(
            body,
            app.theme.style(R::TextPrimary).add_modifier(Modifier::BOLD),
        ));
    }

    Some(Line::from(spans).alignment(Alignment::Center))
}

/// The outer border style, "breathing" between the border colour and the accent when the
/// `border` flag is on; otherwise the unchanged base.
pub fn border_style(app: &App, base: Style) -> Style {
    let a = app.animations();
    if !(a.master && a.border) {
        return base;
    }
    let t = wave(app.anim_frame(), 90);
    base.fg(lerp_color(
        app.theme.color(R::BorderPrimary),
        app.theme.color(R::Accent),
        t,
    ))
}

/// The transport-controls style, pulsing between the control colour and the alt-accent when the
/// `controls` flag is on; otherwise the unchanged base.
pub fn controls_style(app: &App, base: Style) -> Style {
    let a = app.animations();
    if !(a.master && a.controls) {
        return base;
    }
    let t = wave(app.anim_frame(), 36);
    base.fg(lerp_color(
        app.theme.color(R::PlayerControl),
        app.theme.color(R::AccentAlt),
        t,
    ))
}

/// A short braille spinner glyph for the front of the status line, or `None` when the `spinner`
/// flag is off. Frames come from the well-known `throbber-widgets-tui` braille set.
pub fn spinner_prefix(app: &App) -> Option<String> {
    let a = app.animations();
    if !(a.master && a.spinner) {
        return None;
    }
    let syms = throbber_widgets_tui::BRAILLE_EIGHT.symbols;
    let i = (app.anim_frame() / 2) as usize % syms.len();
    Some(syms[i].to_owned())
}

/// A row of faux VU bars (`▁▂▃▅▇`) for the status line, or `None` when the `eq_bars` flag is off.
/// Procedural (no real audio tap) — each bar mixes two out-of-phase waves so it looks lively.
pub fn eq_bars(app: &App) -> Option<String> {
    let a = app.animations();
    if !(a.master && a.eq_bars) {
        return None;
    }
    const BLOCKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let f = app.anim_frame();
    let mut s = String::with_capacity(5);
    for i in 0..5u64 {
        let t = wave(f + i * 7, 16 + i * 3) * 0.6 + wave(f * 2 + i * 5, 9 + i) * 0.4;
        let idx = (t * (BLOCKS.len() - 1) as f64).round() as usize;
        s.push(BLOCKS[idx.min(BLOCKS.len() - 1)]);
    }
    Some(s)
}

/// Overlay a bright "comet" sweeping across the filled portion of the seekbar gauge (just recolours
/// cell backgrounds, so the time label stays put). No-op when the `seekbar` flag is off.
pub fn seekbar_overlay(frame: &mut Frame, app: &App, area: Rect, ratio: f64) {
    let a = app.animations();
    if !(a.master && a.seekbar) || area.width == 0 || area.height == 0 {
        return;
    }
    let filled = (f64::from(area.width) * ratio).round() as u16;
    if filled == 0 {
        return;
    }
    let f = app.anim_frame();
    let head = (f % u64::from(filled)) as u16;
    let gauge = app.theme.color(R::GaugeFilled);
    let buf = frame.buffer_mut();
    // A 3-cell comet, brightest at the head, fading behind it.
    for k in 0..3u16 {
        if head < k {
            continue;
        }
        let x = area.x + head - k;
        if head - k >= filled {
            continue;
        }
        let t = 1.0 - f64::from(k) / 3.0;
        if let Some(cell) = buf.cell_mut((x, area.y)) {
            cell.set_bg(lerp_color(gauge, Color::Rgb(255, 255, 255), t));
        }
    }
}

// ── canvas effects ──────────────────────────────────────────────────────────

/// Draw all enabled canvas effects into a blank `zone` (back to front: rain, starfield, then the
/// visualizer strip, the donut and finally the bouncing logo on top). Called only for the blank
/// filler region, so it never overdraws album art or lyrics.
pub fn render_canvas(frame: &mut Frame, app: &App, zone: Rect) {
    let a = app.animations();
    if !a.master || zone.width < 4 || zone.height < 2 {
        return;
    }
    let f = app.anim_frame();
    if a.rain {
        rain(frame, app, zone, f);
    }
    if a.starfield {
        starfield(frame, app, zone, f);
    }
    if a.visualizer {
        visualizer(frame, app, zone, f);
    }
    if a.donut {
        donut(frame, app, zone, f);
    }
    if a.bounce {
        bounce(frame, app, zone, f);
    }
}

const RAIN_GLYPHS: [char; 22] = [
    '0', '1', '7', '9', '=', '+', '*', '/', '<', '>', ':', '.', '#', '$', '%', '&', '?', 'Z', 'X',
    'A', 'V', '|',
];

#[derive(Clone, Copy)]
struct RainColumn {
    col: u64,
    speed: u64,
    len: u64,
    period: u64,
    offset: u64,
}

#[derive(Default)]
struct RainScratch {
    width: u16,
    height: u16,
    columns: Vec<RainColumn>,
}

thread_local! {
    static RAIN_SCRATCH: RefCell<RainScratch> = RefCell::new(RainScratch::default());
}

/// Classic matrix digital rain: each column is an independently-falling head with a fading green
/// trail. Speed / length / phase are hashed from the column index so columns desync but stay
/// stable frame to frame.
fn rain(frame: &mut Frame, _app: &App, zone: Rect, f: u64) {
    let h = u64::from(zone.height);
    RAIN_SCRATCH.with(|scratch| {
        let mut scratch = scratch.borrow_mut();
        if scratch.width != zone.width || scratch.height != zone.height {
            scratch.width = zone.width;
            scratch.height = zone.height;
            scratch.columns.clear();
            scratch.columns.reserve(usize::from(zone.width));
            let h_mod = h.max(1) as u32;
            for col in 0..u64::from(zone.width) {
                let speed = 1 + u64::from(hash32(col) % 3);
                let len = 4 + u64::from(hash32(col.wrapping_mul(2_654_435_761)) % h_mod);
                let period = h + len + 3;
                let offset = u64::from(hash32(col ^ 0x9E37_79B9)) % period;
                scratch.columns.push(RainColumn {
                    col,
                    speed,
                    len,
                    period,
                    offset,
                });
            }
        }

        let buf = frame.buffer_mut();
        for (i, column) in scratch.columns.iter().enumerate() {
            let x = zone.left() + i as u16;
            let head = (f / column.speed + column.offset) % column.period;
            for k in 0..=column.len {
                if head < k {
                    continue;
                }
                let row = head - k;
                if row >= h {
                    continue;
                }
                let y = zone.top() + row as u16;
                let g = RAIN_GLYPHS[(hash32(
                    column
                        .col
                        .wrapping_mul(31)
                        .wrapping_add(row)
                        .wrapping_add(f / 6),
                ) as usize)
                    % RAIN_GLYPHS.len()];
                let color = if k == 0 {
                    Color::Rgb(200, 255, 200)
                } else {
                    let b = 1.0 - k as f64 / column.len as f64;
                    Color::Rgb((30.0 * b) as u8, (60.0 + 180.0 * b) as u8, (40.0 * b) as u8)
                };
                put_char(buf, x, y, g, Style::default().fg(color));
            }
        }
    });
}

const STAR_GLYPHS: [char; 8] = ['·', '✦', '✧', '*', '+', '.', '°', '⋆'];

#[derive(Clone, Copy)]
struct Star {
    x: u16,
    speed: u64,
    seed: u64,
}

#[derive(Default)]
struct StarScratch {
    width: u16,
    height: u16,
    count: u32,
    stars: Vec<Star>,
}

thread_local! {
    static STAR_SCRATCH: RefCell<StarScratch> = RefCell::new(StarScratch::default());
}

/// Drifting stars / musical sparkles rising slowly up the zone, each twinkling between a subtle and
/// an accent colour. Density scales with the zone area.
fn starfield(frame: &mut Frame, app: &App, zone: Rect, f: u64) {
    let w = u32::from(zone.width);
    let h = u64::from(zone.height);
    let count = ((w * u32::from(zone.height)) / 40).clamp(6, 60);
    let dim = app.theme.color(R::TextSubtle);
    let bright = app.theme.color(R::AccentAlt);
    let wave24: [f64; 24] = std::array::from_fn(|i| wave(i as u64, 24));
    STAR_SCRATCH.with(|scratch| {
        let mut scratch = scratch.borrow_mut();
        if scratch.width != zone.width || scratch.height != zone.height || scratch.count != count {
            scratch.width = zone.width;
            scratch.height = zone.height;
            scratch.count = count;
            scratch.stars.clear();
            scratch.stars.reserve(count as usize);
            for i in 0..u64::from(count) {
                let x = (hash32(i * 2 + 1) % w) as u16;
                let speed = 1 + u64::from(hash32(i * 3 + 2) % 4);
                let seed = u64::from(hash32(i * 5 + 3));
                scratch.stars.push(Star { x, speed, seed });
            }
        }

        let buf = frame.buffer_mut();
        for (i, star) in scratch.stars.iter().enumerate() {
            let i = i as u64;
            let yv = (f / star.speed + star.seed) % (h + 4);
            if yv >= h {
                continue; // off-screen pause → a twinkle gap
            }
            let y = zone.top() + (h - 1 - yv) as u16;
            let g = STAR_GLYPHS[(hash32(i * 7 + f / 20) as usize) % STAR_GLYPHS.len()];
            let color = lerp_color(dim, bright, wave24[((f + i * 5) % 24) as usize]);
            put_char(buf, zone.left() + star.x, y, g, Style::default().fg(color));
        }
    });
}

/// A decorative (non-audio-reactive) spectrum: bars rising from the bottom strip of the zone, each
/// mixing two waves, coloured from the gauge colour up to the accent.
fn visualizer(frame: &mut Frame, app: &App, zone: Rect, f: u64) {
    const BLOCKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let strip = (zone.height / 3).clamp(2, 8);
    let baseline = i32::from(zone.bottom());
    let top = i32::from(zone.top());
    let filled = app.theme.color(R::GaugeFilled);
    let accent = app.theme.color(R::Accent);
    let wave14: [f64; 14] = std::array::from_fn(|i| wave(i as u64, 14));
    let wave9: [f64; 9] = std::array::from_fn(|i| wave(i as u64, 9));
    let row_styles: [Style; 8] = std::array::from_fn(|i| {
        let row = i as u16;
        if row < strip {
            Style::default().fg(lerp_color(
                filled,
                accent,
                f64::from(row + 1) / f64::from(strip),
            ))
        } else {
            Style::default()
        }
    });
    let buf = frame.buffer_mut();
    for bx in 0..zone.width {
        let x = zone.left() + bx;
        let phase = u64::from(bx) * 3;
        let t =
            wave14[((f + phase) % 14) as usize] * 0.6 + wave9[((f * 2 + phase) % 9) as usize] * 0.4;
        let cells = t * f64::from(strip);
        let full = cells.floor() as u16;
        let frac = cells - f64::from(full);
        for row in 0..strip {
            let yy = baseline - 1 - i32::from(row);
            if yy < top {
                break;
            }
            let y = yy as u16;
            let style = row_styles[row as usize];
            if row < full {
                put_char(buf, x, y, '█', style);
            } else if row == full && frac > 0.05 {
                let idx = (frac * (BLOCKS.len() - 1) as f64).round() as usize;
                put_char(buf, x, y, BLOCKS[idx.min(BLOCKS.len() - 1)], style);
            }
        }
    }
}

const DONUT_LUM: &[u8; 12] = b".,-~:;=!*#$@";

#[derive(Default)]
struct DonutScratch {
    zbuf: Vec<f64>,
    out: Vec<u8>,
}

thread_local! {
    static DONUT_SCRATCH: RefCell<DonutScratch> = RefCell::new(DonutScratch::default());
}

fn donut_trig_steps() -> &'static [(f64, f64)] {
    static STEPS: OnceLock<Vec<(f64, f64)>> = OnceLock::new();
    STEPS.get_or_init(|| {
        let mut steps = Vec::new();
        let mut angle = 0.0f64;
        while angle < std::f64::consts::TAU {
            steps.push(angle.sin_cos());
            angle += 0.06;
        }
        steps
    })
}

/// The classic spinning ASCII torus (Andy Sloane's donut), z-buffered into the centre of the zone.
/// Two rotation angles advance with the frame counter. Luminance picks a glyph and a colour ramp
/// from accent to alt-accent.
fn donut(frame: &mut Frame, app: &App, zone: Rect, f: u64) {
    let w = i32::from(zone.width);
    let h = i32::from(zone.height);
    if w < 8 || h < 5 {
        return;
    }
    let (sa, ca) = (f as f64 * 0.04).sin_cos();
    let (sb, cb) = (f as f64 * 0.02).sin_cos();
    let cells = (w * h) as usize;
    let k1 = (f64::from(w) * 0.32).min(f64::from(h) * 0.62);
    let k2 = 5.0;
    let steps = donut_trig_steps();
    let base = app.theme.color(R::Accent);
    let bright = app.theme.color(R::AccentAlt);
    let styles: [Style; 12] = std::array::from_fn(|ci| {
        let t = ci as f64 / (DONUT_LUM.len() - 1) as f64;
        Style::default().fg(lerp_color(base, bright, t))
    });

    DONUT_SCRATCH.with(|scratch| {
        let mut scratch = scratch.borrow_mut();
        if scratch.zbuf.len() != cells {
            scratch.zbuf.resize(cells, 0.0);
            scratch.out.resize(cells, 0);
        } else {
            scratch.zbuf.fill(0.0);
            scratch.out.fill(0);
        }

        for &(st, ct) in steps {
            for &(sp, cp) in steps {
                let circle_x = 2.0 + ct; // R2 + R1*cos(theta), R1 = 1, R2 = 2
                let circle_y = st;
                let x = circle_x * (cb * cp + sa * sb * sp) - circle_y * ca * sb;
                let y = circle_x * (sb * cp - sa * cb * sp) + circle_y * ca * cb;
                let z = k2 + ca * circle_x * sp + circle_y * sa;
                let ooz = 1.0 / z;
                let xp = (f64::from(w) / 2.0 + k1 * ooz * x) as i32;
                let yp = (f64::from(h) / 2.0 - k1 * ooz * y * 0.5) as i32;
                if xp >= 0 && xp < w && yp >= 0 && yp < h {
                    let idx = (xp + yp * w) as usize;
                    if ooz > scratch.zbuf[idx] {
                        let lum =
                            cp * ct * sb - ca * ct * sp - sa * st + cb * (ca * st - ct * sa * sp);
                        scratch.zbuf[idx] = ooz;
                        let li = if lum > 0.0 { (lum * 8.0) as usize } else { 0 };
                        scratch.out[idx] = (li.min(DONUT_LUM.len() - 1) + 1) as u8;
                    }
                }
            }
        }

        let buf = frame.buffer_mut();
        for yy in 0..h {
            for xx in 0..w {
                let v = scratch.out[(xx + yy * w) as usize];
                if v == 0 {
                    continue;
                }
                let ci = (v as usize - 1).min(DONUT_LUM.len() - 1);
                put_char(
                    buf,
                    zone.left() + xx as u16,
                    zone.top() + yy as u16,
                    DONUT_LUM[ci] as char,
                    styles[ci],
                );
            }
        }
    });
}

/// DVD-style bouncing logo: an ASCII tag ricochets around the zone on a triangle-wave path, its
/// colour cycling each time it crosses a wall.
fn bounce(frame: &mut Frame, app: &App, zone: Rect, f: u64) {
    const LABEL: &str = "<ytm-tui>";
    let tw = LABEL.chars().count() as i64;
    let w = i64::from(zone.width);
    let h = i64::from(zone.height);
    if w <= tw || h < 2 {
        return;
    }
    let span_x = (w - tw).max(1);
    let span_y = (h - 1).max(1);
    // Triangle wave in `0..=span`.
    let tri = |t: i64, span: i64| -> i64 {
        let period = span * 2;
        let m = t.rem_euclid(period);
        if m < span { m } else { period - m }
    };
    let t = f as i64;
    let x = i64::from(zone.left()) + tri(t / 2, span_x);
    let y = i64::from(zone.top()) + tri(t / 3, span_y);
    let cyc = (t / span_x.max(1) + t / span_y.max(1)) as usize;
    const PALETTE: [R; 5] = [R::Accent, R::AccentAlt, R::Success, R::Warning, R::Error];
    let color = app.theme.color(PALETTE[cyc % PALETTE.len()]);
    let right = i64::from(zone.right());
    let buf = frame.buffer_mut();
    for (i, ch) in LABEL.chars().enumerate() {
        let cx = x + i as i64;
        if cx < i64::from(zone.left()) || cx >= right {
            continue;
        }
        put_char(
            buf,
            cx as u16,
            y as u16,
            ch,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;

    /// The core promise: with every animation toggle off (the default), the element-level helpers
    /// produce *nothing* — `None` builders and identity styles — so the player renders exactly as a
    /// build without this module would. (The per-frame clock is gated separately in `main.rs`.)
    #[test]
    fn all_effects_off_is_a_no_op() {
        let _guard = crate::i18n::lock_for_test();
        let app = App::new(100);
        assert!(!app.animations().master, "animations are off by default");
        assert!(title_line(&app, "Title", "Artist", true, 40).is_none());
        assert!(spinner_prefix(&app).is_none());
        assert!(eq_bars(&app).is_none());
        let base = Style::default()
            .fg(Color::Rgb(1, 2, 3))
            .add_modifier(Modifier::BOLD);
        assert_eq!(border_style(&app, base), base);
        assert_eq!(controls_style(&app, base), base);
    }
}
