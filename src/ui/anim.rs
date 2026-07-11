//! Optional UI animations. Every effect is individually gated by an
//! [`AnimationsConfig`](crate::config::AnimationsConfig) flag plus the global `master` switch;
//! with all flags off nothing here ever runs (the per-frame clock in `main.rs` stays asleep, so
//! the app renders byte-for-byte like a build with this module absent — the "fast and
//! light" identity is preserved).
//!
//! Four families:
//! * **Element-level** — restyle existing widgets in place (title shimmer, beating heart,
//!   breathing border, control pulse, seekbar comet, status-line spinner / EQ bars, lyrics glow,
//!   selection breathing, blinking carets, activity dots). These return a tweaked
//!   `Style`/`Line`/`String` or draw a small overlay; when their flag is off they return the
//!   plain value, so the call site stays a one-liner.
//! * **One-shots** — event feedback armed centrally by `App::detect_fx` (title intro on a track
//!   change, status typewriter, volume flash, like burst, seek ripple, tab pop, list cascade,
//!   popup fade-in). Each reads its start frame from [`App::fx`](crate::app::FxState) and its
//!   window from [`fx_window`]; the window is defined in wall-clock ms and converted through
//!   [`App::anim_ms_frames`], so a one-shot feels the same length at 5 fps and at 60 fps.
//! * **Canvas** — drawn straight into the back buffer in the blank filler zone only (never over
//!   album art or lyrics): matrix rain, spinning donut, faux visualizer, starfield, bouncing logo.
//! * **Overlay** — the About card's sparkles and brand gradient.
//!
//! All phase comes from [`App::anim_frame`] (a frame counter frozen while paused, except while a
//! one-shot keeps the clock briefly awake), so the effects are deterministic and resume cleanly.
//! Canvas/overlay glyphs are all display-width 1, so direct cell writes never drift the layout;
//! everything degrades in retro mode either at the source or through the CP437 scrubber.

use std::cell::RefCell;
use std::sync::OnceLock;

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{App, Mode};
use crate::theme::ThemeRole as R;
use crate::ui::marquee::col_window;

pub use crate::ui::marquee::selected_marquee;

/// One-shot effect windows, in wall-clock milliseconds. `App::detect_fx` arms the clock with
/// these; the matching render helpers here derive their progress from the same values, so
/// trigger and drawing can never disagree about an effect's length.
pub mod fx_window {
    /// Title letter-cascade on a track change.
    pub const TRACK_INTRO_MS: u64 = 800;
    /// Heart burst on a like.
    pub const LIKE_MS: u64 = 700;
    /// Volume gauge flash after a volume nudge.
    pub const VOLUME_MS: u64 = 750;
    /// Ripple at the seekbar head after a seek.
    pub const SEEK_MS: u64 = 350;
    /// Active-tab pop on a view/tab switch.
    pub const SWITCH_MS: u64 = 350;
    /// List-row cascade on fresh content.
    pub const LIST_MS: u64 = 600;
    /// Popup fade-in materialize.
    pub const POPUP_MS: u64 = 160;
    /// Flash on a newly-current synced-lyric line.
    pub const LYRIC_MS: u64 = 450;
    /// Typewriter speed for the status toast (display columns per second).
    pub const TOAST_COLS_PER_SEC: u64 = 70;

    /// The toast window for a message `cols` wide: type time plus a short glow tail, capped so
    /// a long message speeds up rather than lingering past the status TTL.
    pub fn toast_ms(cols: usize) -> u64 {
        (250 + cols as u64 * 1000 / TOAST_COLS_PER_SEC).min(1600)
    }
}

// ── shared helpers ──────────────────────────────────────────────────────────

/// A smooth 0→1→0 pulse over `period` frames (0 at the ends, 1 at the middle).
fn wave(frame: u64, period: u64) -> f64 {
    let p = period.max(1);
    let t = (frame % p) as f64 / p as f64;
    0.5 - 0.5 * (std::f64::consts::TAU * t).cos()
}

/// Fast start, gentle landing — the house easing for one-shot motion.
fn ease_out_cubic(t: f64) -> f64 {
    1.0 - (1.0 - t.clamp(0.0, 1.0)).powi(3)
}

/// Linear progress (0..1) through a one-shot window `ms` long that started at frame `start`.
/// `None` before it starts or after it ends — every one-shot renderer keys off this, so an
/// expired effect costs one comparison and draws nothing.
fn fx_t(app: &App, start: Option<u64>, ms: u64) -> Option<f64> {
    let start = start?;
    let f = app.anim_frame();
    if f < start {
        return None; // stale slot from before a frame-counter context change
    }
    let len = app.anim_ms_frames(ms).max(1);
    let elapsed = f - start;
    if elapsed >= len {
        return None;
    }
    Some(elapsed as f64 / len as f64)
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

        // A bright band sweeps across the characters (shimmer). Outside the ~6-cell band
        // the interpolation factor is exactly 0, so the whole tail shares one Style —
        // group equal-style runs into one Span each instead of one heap String per char
        // (this runs at the animation FPS). Cell output is identical either way.
        let span_len = window.chars().count() as f64 + 8.0;
        let head = (f as f64 / 2.0) % span_len;
        let mut col = 0.0f64;
        let mut run = String::new();
        let mut run_style: Option<Style> = None;
        for ch in window.chars() {
            let d = (col - head).abs();
            let b = (1.0 - d / 3.0).clamp(0.0, 1.0);
            let style = Style::default()
                .fg(lerp_color(base, bright, b))
                .add_modifier(Modifier::BOLD);
            if run_style != Some(style) {
                if let Some(prev) = run_style.take()
                    && !run.is_empty()
                {
                    spans.push(Span::styled(std::mem::take(&mut run), prev));
                }
                run_style = Some(style);
            }
            run.push(ch);
            col += UnicodeWidthChar::width(ch).unwrap_or(1) as f64;
        }
        if let Some(style) = run_style
            && !run.is_empty()
        {
            spans.push(Span::styled(run, style));
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

/// A short spinner glyph for the front of the status line, or `None` when the `spinner` flag is
/// off. Frames come from the well-known `throbber-widgets-tui` braille set — or its classic
/// `|/-\` ASCII set in retro mode, where a console font rarely covers braille.
pub fn spinner_prefix(app: &App) -> Option<String> {
    let a = app.animations();
    if !(a.master && a.spinner) {
        return None;
    }
    let syms = if app.retro_mode() {
        throbber_widgets_tui::ASCII.symbols
    } else {
        throbber_widgets_tui::BRAILLE_EIGHT.symbols
    };
    let i = (app.anim_frame() / 2) as usize % syms.len();
    Some(syms[i].to_owned())
}

/// One-cell bar levels: eighth-blocks normally; a CP437 shade ramp in retro mode, where the
/// eighth-blocks (all but ▄/█) are outside a 256-glyph console font — level reads as density
/// instead of height, which is how DOS-era meters drew it anyway.
const BLOCKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
const RETRO_BLOCKS: [char; 8] = ['.', ':', '░', '░', '▒', '▒', '▓', '█'];

fn bar_blocks(app: &App) -> &'static [char; 8] {
    if app.retro_mode() {
        &RETRO_BLOCKS
    } else {
        &BLOCKS
    }
}

/// A row of faux VU bars (`▁▂▃▅▇`) for the status line, or `None` when the `eq_bars` flag is off.
/// Procedural (no real audio tap) — each bar mixes two out-of-phase waves so it looks lively.
pub fn eq_bars(app: &App) -> Option<String> {
    let a = app.animations();
    if !(a.master && a.eq_bars) {
        return None;
    }
    let blocks = bar_blocks(app);
    let f = app.anim_frame();
    let mut s = String::with_capacity(5);
    for i in 0..5u64 {
        let t = wave(f + i * 7, 16 + i * 3) * 0.6 + wave(f * 2 + i * 5, 9 + i) * 0.4;
        let idx = (t * (blocks.len() - 1) as f64).round() as usize;
        s.push(blocks[idx.min(blocks.len() - 1)]);
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

// ── one-shot + ambient UI effects ───────────────────────────────────────────

/// Equal-style run grouper for character-by-character line builders (these run at animation
/// FPS, so they emit one `Span` per style-run instead of one heap `String` per char).
struct RunBuilder {
    spans: Vec<Span<'static>>,
    run: String,
    style: Option<Style>,
}

impl RunBuilder {
    fn new(spans: Vec<Span<'static>>) -> Self {
        Self {
            spans,
            run: String::new(),
            style: None,
        }
    }

    fn push(&mut self, ch: char, style: Style) {
        if self.style != Some(style) {
            self.flush();
            self.style = Some(style);
        }
        self.run.push(ch);
    }

    fn flush(&mut self) {
        if let Some(style) = self.style.take()
            && !self.run.is_empty()
        {
            self.spans
                .push(Span::styled(std::mem::take(&mut self.run), style));
        }
        self.run.clear();
    }

    fn finish(mut self) -> Vec<Span<'static>> {
        self.flush();
        self.spans
    }
}

/// Smooth sub-second seekbar fill: interpolates the gauge between mpv's ~1 Hz `time-pos`
/// reports using the same `time_pos_at` anchor the OS media session interpolates with.
/// Folded under the `seekbar` flag (it is that element's animation); returns `base` untouched
/// when the flag is off, playback is paused, or the clock isn't running — so a parked app
/// never creeps.
pub fn smooth_seek_ratio(app: &App, base: f64) -> f64 {
    let a = app.animations();
    if !(a.master && a.seekbar) || app.playback.paused || !app.animation_active() {
        return base;
    }
    let (Some(pos), Some(at), Some(dur)) = (
        app.playback.time_pos,
        app.playback.time_pos_at,
        app.playback.duration,
    ) else {
        return base;
    };
    if dur <= 0.0 {
        return base;
    }
    let live = pos + at.elapsed().as_secs_f64() * app.playback.speed.max(0.0);
    // Defence-in-depth: a non-finite `live` (a stray NaN/inf position slipping past
    // ingestion) would make `(live/dur).clamp(..)` stay NaN and panic ratatui's
    // `Gauge::ratio`. Coalesce to 0 so the seekbar renders empty instead of crashing.
    (crate::util::finite_or(live, 0.0) / dur).clamp(0.0, 1.0)
}

/// A bright ripple expanding from the seekbar head right after a seek, so the jump target is
/// unmissable. Recolours cell backgrounds only (like the comet), so the time label stays put.
pub fn seek_flash_overlay(frame: &mut Frame, app: &App, area: Rect, ratio: f64) {
    let a = app.animations();
    if !(a.master && a.seek_flash) || area.width == 0 || area.height == 0 {
        return;
    }
    let Some(t) = fx_t(app, app.fx.seek, fx_window::SEEK_MS) else {
        return;
    };
    let spread = ease_out_cubic(t);
    let head = ((f64::from(area.width) * ratio) as u16).min(area.width.saturating_sub(1));
    let radius = 1.0 + spread * 5.0;
    let gauge = app.theme.color(R::GaugeFilled);
    let empty = app.theme.color(R::GaugeEmpty);
    let buf = frame.buffer_mut();
    for dx in -6i32..=6 {
        let d = dx.unsigned_abs() as f64;
        if d > radius {
            continue;
        }
        let x = i32::from(area.x) + i32::from(head) + dx;
        if x < i32::from(area.x) || x >= i32::from(area.right()) {
            continue;
        }
        // Brightest at the head, fading with distance and as the ripple dies out.
        let b = (1.0 - d / radius.max(1.0)) * (1.0 - t);
        if let Some(cell) = buf.cell_mut((x as u16, area.y)) {
            let under = if (x - i32::from(area.x)) as f64 / f64::from(area.width.max(1)) < ratio {
                gauge
            } else {
                empty
            };
            cell.set_bg(lerp_color(under, Color::Rgb(255, 255, 255), b * 0.9));
        }
    }
}

/// A transient volume gauge flashed directly over the transport strip's `vol - 50% +`
/// cluster whenever the volume changes (keys, wheel, or remote) — hold for two thirds of
/// the window, fade out over the last third. `area` is the cluster's own rect (the caller
/// computes it for the `VolumeArea` hit rect anyway) and the gauge spans it exactly, so it
/// reads as the controls momentarily morphing into a level meter. Paint only: the -/+ hit
/// rects registered underneath are untouched, so the buttons keep taking clicks while
/// covered. `█`/`░` cells only, so retro mode shows it verbatim.
pub fn volume_flash_overlay(frame: &mut Frame, app: &App, area: Rect) {
    let a = app.animations();
    if !(a.master && a.volume_flash) || area.width == 0 || area.height == 0 {
        return;
    }
    let Some(t) = fx_t(app, app.fx.volume, fx_window::VOLUME_MS) else {
        return;
    };
    let volume = app.playback.volume.clamp(0, 100);
    let edge_blink = if matches!(volume, 0 | 100) {
        // One low-amplitude brightness breath replaces any endpoint size change. It only
        // changes colour: the glyphs, overlay bounds, and transport hit geometry stay fixed.
        0.82 + 0.18 * (std::f64::consts::PI * t).sin()
    } else {
        1.0
    };
    let alpha = ((1.0 - t) * 3.0).clamp(0.0, 1.0) * edge_blink;
    let vol = volume as f64 / 100.0;
    let filled = (vol * f64::from(area.width)).round() as u16;
    let bg = app.theme.color(R::Background);
    let on = lerp_color(bg, app.theme.color(R::GaugeFilled), alpha);
    let off = lerp_color(bg, app.theme.color(R::GaugeEmpty), alpha * 0.6);
    let buf = frame.buffer_mut();
    for i in 0..area.width {
        let (ch, color) = if i < filled {
            ('█', on)
        } else {
            ('░', off)
        };
        put_char(buf, area.x + i, area.y, ch, Style::default().fg(color));
    }
}

/// The title line's letter-cascade intro right after a track change: characters sweep in
/// left-to-right with a bright head, landing on the plain bold title. Centered by a leading
/// pad so the text sits at its final position from the first frame. Takes precedence over the
/// shimmer while its window runs; `None` otherwise (the caller falls through).
pub fn title_intro_line(
    app: &App,
    title: &str,
    artist: &str,
    liked: bool,
    width: u16,
) -> Option<Line<'static>> {
    let a = app.animations();
    if !(a.master && a.track_intro) {
        return None;
    }
    let t = fx_t(app, app.fx.track_intro, fx_window::TRACK_INTRO_MS)?;
    let heart = if liked { "♥ " } else { "" };
    let body = col_window(&format!("{heart}{title} — {artist}"), 0, width as usize);
    let total = UnicodeWidthStr::width(body.as_str());
    let reveal = ease_out_cubic(t) * (total as f64 + 5.0);
    let pad = (width as usize).saturating_sub(total) / 2;
    let base = app.theme.color(R::TextPrimary);
    let bright = app.theme.color(R::Accent);
    let mut b = RunBuilder::new(vec![Span::raw(" ".repeat(pad))]);
    let mut col = 0f64;
    for ch in body.chars() {
        if col >= reveal {
            break;
        }
        // Brightest just behind the reveal head, settling to the plain title colour.
        let glow = (1.0 - (reveal - col) / 5.0).clamp(0.0, 1.0);
        let style = Style::default()
            .fg(lerp_color(base, bright, glow))
            .add_modifier(Modifier::BOLD);
        b.push(ch, style);
        col += UnicodeWidthChar::width(ch).unwrap_or(1) as f64;
    }
    Some(Line::from(b.finish()).alignment(Alignment::Left))
}

/// The transient status message typing itself in with a bright caret head — shared by every
/// view's status band. Reads the text and kind straight off [`App::status`]; `None` when the
/// flag is off or the window has passed (callers render their plain centered line).
pub fn status_toast_line(app: &App, width: u16) -> Option<Line<'static>> {
    let a = app.animations();
    if !(a.master && a.toast) {
        return None;
    }
    let text = app.status.text.as_str();
    if text.is_empty() {
        return None;
    }
    let cols = UnicodeWidthStr::width(text);
    let window_ms = fx_window::toast_ms(cols);
    let t = fx_t(app, app.fx.toast, window_ms)?;
    // The window is type-time plus a fixed glow tail; typing progress maps onto the type
    // portion so the last characters land just before the tail starts.
    let type_frac = 1.0 - 250.0 / window_ms as f64;
    let progress = (t / type_frac.max(0.05)).min(1.0);
    let shown = col_window(text, 0, width as usize);
    let shown_cols = UnicodeWidthStr::width(shown.as_str());
    let reveal = progress * shown_cols as f64;
    let role = match app.status.kind {
        crate::app::StatusKind::Error => R::Error,
        crate::app::StatusKind::Info => R::Success,
    };
    let style = app.theme.style(role);
    let pad = (width as usize).saturating_sub(shown_cols) / 2;
    let mut b = RunBuilder::new(vec![Span::raw(" ".repeat(pad))]);
    let mut col = 0f64;
    for ch in shown.chars() {
        if col >= reveal {
            break;
        }
        b.push(ch, style);
        col += UnicodeWidthChar::width(ch).unwrap_or(1) as f64;
    }
    let mut spans = b.finish();
    if progress < 1.0 && (col as usize) < width as usize {
        // The typing head: a block caret in the accent colour.
        spans.push(Span::styled("\u{2588}", app.theme.style(R::Accent)));
    }
    Some(Line::from(spans).alignment(Alignment::Left))
}

/// A little burst of hearts and sparks radiating from the title when the current track is
/// liked. Drawn over the title row and the blank gap rows directly above/below it; the row of
/// cells the title text itself occupies (`occupied_cols`, centered) is left untouched so the
/// words never glitch.
pub fn like_burst_overlay(frame: &mut Frame, app: &App, area: Rect, occupied_cols: u16) {
    let a = app.animations();
    if !(a.master && a.like_burst) || area.width < 12 {
        return;
    }
    let Some(t) = fx_t(app, app.fx.like, fx_window::LIKE_MS) else {
        return;
    };
    let spread = ease_out_cubic(t);
    const GLYPHS: [char; 5] = ['♥', '✦', '·', '*', '♥'];
    let color = lerp_color(app.theme.color(R::Error), app.theme.color(R::AccentAlt), t);
    let cx = i32::from(area.x) + i32::from(area.width) / 2;
    let cy = i32::from(area.y);
    let keep_lo = cx - i32::from(occupied_cols) / 2 - 1;
    let keep_hi = cx + i32::from(occupied_cols) / 2 + 1;
    let buf = frame.buffer_mut();
    for i in 0..12u64 {
        // Sparks die off one by one toward the end instead of all vanishing at once.
        if t > 0.55 && hash32(i * 97).is_multiple_of(3) {
            continue;
        }
        let ang = (i as f64 / 12.0 + f64::from(hash32(i) % 100) / 900.0) * std::f64::consts::TAU;
        let x = cx + (ang.cos() * (2.0 + spread * 14.0)).round() as i32;
        let y = cy + (ang.sin() * (0.4 + spread * 1.7)).round() as i32;
        if y < cy - 1 || y > cy + 1 {
            continue;
        }
        if x < i32::from(area.x) || x >= i32::from(area.right()) {
            continue;
        }
        if y == cy && x >= keep_lo && x <= keep_hi {
            continue; // never overwrite the title text itself
        }
        let g = GLYPHS[(hash32(i * 31) as usize) % GLYPHS.len()];
        put_char(buf, x as u16, y as u16, g, Style::default().fg(color));
    }
}

/// The focused list-selection bar, breathing gently toward the accent colour. Identity when
/// the flag is off or the style has no background to breathe.
pub fn selection_style(app: &App, base: Style) -> Style {
    let a = app.animations();
    if !(a.master && a.selection) {
        return base;
    }
    let Some(bg) = base.bg else { return base };
    let t = wave(app.anim_frame(), app.anim_ms_frames(2800)) * 0.35;
    base.bg(lerp_color(bg, app.theme.color(R::Accent), t))
}

/// Cascade restyle for the `vis`-th visible row of `mode`'s list while a list-reveal one-shot
/// runs: rows fade up from the theme background top-to-bottom. Identity outside the window.
pub fn stagger_style(app: &App, mode: Mode, vis: usize, base: Style) -> Style {
    let Some(alpha) = stagger_alpha(app, mode, vis) else {
        return base;
    };
    let bg_theme = app.theme.color(R::Background);
    let mut out = base;
    if let Some(fg) = base.fg {
        out = out.fg(lerp_color(bg_theme, fg, alpha));
    }
    if let Some(bg) = base.bg {
        out = out.bg(lerp_color(bg_theme, bg, alpha));
    }
    out
}

/// The cascade's per-row alpha (0 hidden → 1 landed), or `None` when no cascade applies to
/// `mode` right now. Row delays are staggered across the first 60% of the window (capped at
/// 14 distinct steps so tall lists still finish together); each row then fades in over 25%.
fn stagger_alpha(app: &App, mode: Mode, vis: usize) -> Option<f64> {
    let a = app.animations();
    if !(a.master && a.stagger) {
        return None;
    }
    let (start, m) = app.fx.list?;
    if m != mode {
        return None;
    }
    let t = fx_t(app, Some(start), fx_window::LIST_MS)?;
    let delay = (vis as f64).min(14.0) / 14.0 * 0.6;
    let alpha = ((t - delay) / 0.25).clamp(0.0, 1.0);
    Some(ease_out_cubic(alpha))
}

/// A blinking block caret for text inputs, breathing between its own colour and `bg` (~1.1 s
/// cycle, ~60% on so it reads as a caret, not a strobe). `base` is exactly the style the call
/// site renders its plain solid block with today — returned untouched when the flag is off,
/// so the off-state stays byte-identical.
pub fn caret_span(app: &App, base: Style, bg: Color) -> Span<'static> {
    let a = app.animations();
    if !(a.master && a.caret) {
        return Span::styled("\u{2588}", base);
    }
    let on = base.fg.unwrap_or_else(|| app.theme.color(R::Accent));
    let alpha = (wave(app.anim_frame(), app.anim_ms_frames(1100)) * 1.6).clamp(0.0, 1.0);
    Span::styled("\u{2588}", base.fg(lerp_color(bg, on, alpha)))
}

/// Blink for string-composed carets (the settings text editor's `▏`): the caret char while
/// on, a plain space while off. Same cadence as [`caret_span`]; always one cell.
pub fn caret_char(app: &App) -> char {
    let a = app.animations();
    if !(a.master && a.caret) {
        return '▏';
    }
    let period = app.anim_ms_frames(1100);
    if app.anim_frame() % period < period * 3 / 5 {
        '▏'
    } else {
        ' '
    }
}

/// Which tab strip a pop applies to: the top nav bar (armed by a screen switch) or an
/// in-view tab bar (Library / Settings tabs).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TabPop {
    Nav,
    Inner,
}

/// The active tab's style right after a switch: a bright accent wash that settles onto the
/// normal selection colours. Identity when the flag is off or no pop is running.
pub fn active_tab_style(app: &App, pop: TabPop, base: Style) -> Style {
    let a = app.animations();
    if !(a.master && a.tabs) {
        return base;
    }
    let start = match pop {
        TabPop::Nav => app.fx.switch.map(|(f, _)| f),
        TabPop::Inner => app.fx.tabbar,
    };
    let Some(t) = fx_t(app, start, fx_window::SWITCH_MS) else {
        return base;
    };
    let glow = 1.0 - ease_out_cubic(t);
    let mut out = base;
    if let Some(bg) = base.bg {
        out = out.bg(lerp_color(bg, app.theme.color(R::Accent), glow * 0.8));
    }
    out
}

/// Popup fade-in: while the window runs, every cell of a just-opened popup is blended up from
/// the popup background, so the box materializes instead of appearing at once. Runs as a
/// post-pass from `seal_popup_background` (after Reset backgrounds are sealed, so every cell
/// has a concrete colour to blend from). A no-op the moment the window ends.
pub fn popup_fade_overlay(frame: &mut Frame, app: &App, area: Rect) {
    let a = app.animations();
    if !(a.master && a.popup_fade) {
        return;
    }
    let Some(t) = fx_t(app, app.fx.popup, fx_window::POPUP_MS) else {
        return;
    };
    let alpha = ease_out_cubic(t);
    let from = crate::ui::popup_bg(app);
    let buf = frame.buffer_mut();
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            if let Some(cell) = buf.cell_mut((x, y)) {
                let fg = lerp_color(from, cell.fg, alpha);
                let bg = lerp_color(from, cell.bg, alpha);
                cell.set_fg(fg).set_bg(bg);
            }
        }
    }
}

/// Animated trailing dots for an in-progress label (`Searching`, `…thinking`): cycles
/// `∅ → . → .. → ...` about three times a second, padded to a fixed three cells so the line
/// never shifts. `None` when the flag is off (callers keep their static `…`).
pub fn activity_dots(app: &App) -> Option<String> {
    let a = app.animations();
    if !(a.master && a.activity) {
        return None;
    }
    let step = (app.anim_frame() / app.anim_ms_frames(350).max(1)) % 4;
    let mut s = ".".repeat(step as usize);
    while s.len() < 3 {
        s.push(' ');
    }
    Some(s)
}

/// A spinner glyph standing in for the `⬇` of a *running* download's status tag, so live
/// progress reads as activity at a glance. `None` when the flag is off (the static `⬇` stays).
pub fn download_spinner(app: &App) -> Option<String> {
    let a = app.animations();
    if !(a.master && a.activity) {
        return None;
    }
    let syms = if app.retro_mode() {
        throbber_widgets_tui::ASCII.symbols
    } else {
        throbber_widgets_tui::BRAILLE_EIGHT.symbols
    };
    let i = (app.anim_frame() / 2) as usize % syms.len();
    Some(syms[i].to_owned())
}

/// The current synced-lyric line's style: breathes toward the accent, with a bright flash as
/// a line first becomes current (armed by the central lyric-index diff). Identity when off.
pub fn lyrics_current_style(app: &App, base: Style) -> Style {
    let a = app.animations();
    if !(a.master && a.lyrics) {
        return base;
    }
    let breathe = wave(app.anim_frame(), app.anim_ms_frames(2400)) * 0.35;
    let mut fg = lerp_color(
        app.theme.color(R::LyricsCurrent),
        app.theme.color(R::Accent),
        breathe,
    );
    if let Some(t) = fx_t(app, app.fx.lyric, fx_window::LYRIC_MS) {
        fg = lerp_color(app.theme.color(R::AccentAlt), fg, ease_out_cubic(t));
    }
    base.fg(fg)
}

/// Non-current lyric lines fade slightly with distance from the current one, focusing the eye.
/// Only applied on themes with a concrete RGB background (a transparent background has no
/// colour to fade toward); lines within two rows keep the plain dim style.
pub fn lyrics_dim_style(app: &App, base: Style, distance: usize) -> Style {
    let a = app.animations();
    if !(a.master && a.lyrics) || distance <= 2 {
        return base;
    }
    let Color::Rgb(..) = app.theme.color(R::Background) else {
        return base;
    };
    let fade = ((distance as f64 - 2.0) * 0.14).min(0.55);
    base.fg(lerp_color(
        app.theme.color(R::LyricsDim),
        app.theme.color(R::Background),
        fade,
    ))
}

/// A two-cell mini VU marker for the queue window's now-playing row — the moving-bars version
/// of the static `▸ `. Rides the `eq_bars` flag (it is the same visual language as the
/// status-line VU); `None` when off or paused, so the marker freezes back to `▸ `.
pub fn queue_marker(app: &App) -> Option<String> {
    let a = app.animations();
    if !(a.master && a.eq_bars) || app.playback.paused {
        return None;
    }
    let blocks = bar_blocks(app);
    let f = app.anim_frame();
    let mut s = String::with_capacity(8);
    for i in 0..2u64 {
        let t = wave(f + i * 5, 14 + i * 4) * 0.7 + wave(f * 2 + i * 3, 9) * 0.3;
        let idx = (t * (blocks.len() - 1) as f64).round() as usize;
        s.push(blocks[idx.min(blocks.len() - 1)]);
    }
    Some(s)
}

// ── About-card overlay effects ──────────────────────────────────────────────

const ABOUT_SPARKLE_GLYPHS: [char; 6] = ['✦', '✧', '·', '*', '✦', '·'];

/// Twinkling sparkles on the blank cells around the About card's icon. Positions are hashed
/// (stable) per zone size; each star breathes on its own phase between the subtle and the
/// alt-accent colour. Never writes over a non-blank cell — and never inside `keep_clear`
/// (the icon's rect), whose cells look blank when a native graphics protocol draws the icon.
pub fn about_sparkles(frame: &mut Frame, app: &App, zone: Rect, keep_clear: Rect) {
    let a = app.animations();
    if !(a.master && a.about_fx) || zone.width < 10 || zone.height < 2 {
        return;
    }
    let f = app.anim_frame();
    let dim = app.theme.color(R::TextSubtle);
    let bright = app.theme.color(R::AccentAlt);
    let count = (u32::from(zone.width) / 4).clamp(6, 16);
    let buf = frame.buffer_mut();
    for i in 0..u64::from(count) {
        let x = zone.left() + (hash32(i * 3 + 1) % u32::from(zone.width)) as u16;
        let y = zone.top() + (hash32(i * 7 + 2) % u32::from(zone.height)) as u16;
        if keep_clear.contains(ratatui::layout::Position { x, y }) {
            continue;
        }
        let phase = u64::from(hash32(i * 11 + 3) % 60);
        let tw = wave(f + phase, app.anim_ms_frames(1600));
        if tw < 0.25 {
            continue; // fully dark part of the twinkle — leave the cell alone
        }
        if let Some(cell) = buf.cell_mut((x, y)) {
            if cell.symbol() != " " {
                continue;
            }
            let g = ABOUT_SPARKLE_GLYPHS[(hash32(i * 13) as usize) % ABOUT_SPARKLE_GLYPHS.len()];
            cell.set_char(g).set_fg(lerp_color(dim, bright, tw));
        }
    }
}

/// The About card's `yututui  vX.Y.Z` name line with a gradient band sweeping across the
/// brand. `None` when the flag is off (the caller renders its plain two-span line).
pub fn about_brand_line(app: &App, name: &str, version: &str) -> Option<Line<'static>> {
    let a = app.animations();
    if !(a.master && a.about_fx) {
        return None;
    }
    let f = app.anim_frame();
    let base = app.theme.color(R::TextPrimary);
    let bright = app.theme.color(R::TextSubtle);
    let bg = crate::ui::popup_bg(app);
    let span_len = name.chars().count() as f64 + 6.0;
    let head = (f as f64 / 3.0) % span_len;
    let mut b = RunBuilder::new(Vec::new());
    for (i, ch) in name.chars().enumerate() {
        let d = (i as f64 - head).abs();
        let glow = (1.0 - d / 3.0).clamp(0.0, 1.0);
        b.push(
            ch,
            Style::default()
                .fg(lerp_color(base, bright, glow))
                .bg(bg)
                .add_modifier(Modifier::BOLD),
        );
    }
    let mut spans = b.finish();
    spans.push(Span::styled(
        format!("  v{version}"),
        app.theme.style(R::TextMuted).bg(bg),
    ));
    Some(Line::from(spans))
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
    let blocks = bar_blocks(app);
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
                let idx = (frac * (blocks.len() - 1) as f64).round() as usize;
                put_char(buf, x, y, blocks[idx.min(blocks.len() - 1)], style);
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
    const LABEL: &str = "<yututui>";
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
    use crate::app::{App, Msg, ScrollSurface};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::time::{Duration, Instant};

    fn advance_frames(app: &mut App, frames: u64) {
        for _ in 0..frames {
            app.update(Msg::AnimTick(1));
        }
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<Vec<_>>()
            .join("")
    }

    fn render_with(
        app: &App,
        width: u16,
        height: u16,
        draw: impl FnOnce(&mut Frame, &App, Rect),
    ) -> Buffer {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let area = Rect::new(0, 0, width, height);
        terminal.draw(|f| draw(f, app, area)).unwrap();
        terminal.backend().buffer().clone()
    }

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

    /// Same promise for the new generation of effects: everything off (the default) means
    /// identity styles, `None` builders, the plain solid caret, and an untouched seek ratio.
    #[test]
    fn new_effects_off_are_identity_too() {
        let _guard = crate::i18n::lock_for_test();
        let mut app = App::new(100);
        app.status.text = "Saved: something".to_owned();
        let base = Style::default()
            .fg(Color::Rgb(9, 8, 7))
            .bg(Color::Rgb(3, 2, 1))
            .add_modifier(Modifier::BOLD);

        assert!(title_intro_line(&app, "Title", "Artist", false, 40).is_none());
        assert!(status_toast_line(&app, 40).is_none());
        assert!(activity_dots(&app).is_none());
        assert!(download_spinner(&app).is_none());
        assert!(queue_marker(&app).is_none());
        assert!(about_brand_line(&app, "yututui", "0.0.0").is_none());
        assert_eq!(selection_style(&app, base), base);
        assert_eq!(stagger_style(&app, Mode::Library, 3, base), base);
        assert_eq!(active_tab_style(&app, TabPop::Nav, base), base);
        assert_eq!(lyrics_current_style(&app, base), base);
        assert_eq!(lyrics_dim_style(&app, base, 9), base);
        assert_eq!(smooth_seek_ratio(&app, 0.42), 0.42);
        assert_eq!(caret_char(&app), '▏');
        let caret = caret_span(&app, base, Color::Rgb(0, 0, 0));
        assert_eq!(caret.content, "\u{2588}");
        assert_eq!(caret.style, base);
    }

    /// One-shots are inert until armed: even with master + flags on, the render helpers stay
    /// `None` while no fx window has been triggered; arming a slot produces output for the
    /// window's duration.
    #[test]
    fn one_shots_require_an_armed_window() {
        let _guard = crate::i18n::lock_for_test();
        let mut app = App::new(100);
        app.config.animations.master = true;
        app.config.animations.track_intro = true;
        app.config.animations.toast = true;
        app.status.text = "hello".to_owned();
        // Flags on but nothing armed → still nothing to draw.
        assert!(title_intro_line(&app, "T", "A", false, 40).is_none());
        assert!(status_toast_line(&app, 40).is_none());
        // Armed → the helpers produce output while the window runs.
        app.fx.track_intro = Some(0);
        app.fx.toast = Some(0);
        assert!(title_intro_line(&app, "T", "A", false, 40).is_some());
        assert!(status_toast_line(&app, 40).is_some());
    }

    #[test]
    fn enabled_element_helpers_return_visible_values() {
        let _guard = crate::i18n::lock_for_test();
        let mut app = App::new(100);
        app.config.animations.master = true;
        app.config.animations.title = true;
        app.config.animations.heart = true;
        app.config.animations.spinner = true;
        app.config.animations.eq_bars = true;
        app.config.animations.border = true;
        app.config.animations.controls = true;
        advance_frames(&mut app, 18);

        let line = title_line(&app, "A Very Long Animated Title", "Artist", true, 14)
            .expect("title animation");
        let text = line_text(&line);
        assert!(text.contains('♥') || text.contains("Title") || text.contains("Artist"));

        assert!(spinner_prefix(&app).is_some_and(|s| !s.is_empty()));
        assert_eq!(eq_bars(&app).expect("eq bars").chars().count(), 5);

        let base = Style::default().fg(Color::Rgb(1, 2, 3));
        assert_ne!(border_style(&app, base).fg, base.fg);
        assert_ne!(controls_style(&app, base).fg, base.fg);

        app.config.retro_mode = true;
        assert!(matches!(
            spinner_prefix(&app).as_deref(),
            Some("|" | "/" | "-" | "\\")
        ));
        assert!(
            eq_bars(&app)
                .expect("retro bars")
                .chars()
                .all(|ch| ['.', ':', '░', '▒', '▓', '█'].contains(&ch))
        );
    }

    #[test]
    fn selected_marquee_tracks_overflow_and_cursor_identity() {
        let mut app = App::new(100);
        let text = "A title that is much too wide for the visible row";

        assert_eq!(
            selected_marquee(&app, ScrollSurface::Library, 0, "short", 20),
            "short"
        );

        let first = selected_marquee(&app, ScrollSurface::Library, 0, text, 12);
        assert!(app.bridges.marquee_ran.get());
        assert_eq!(
            app.bridges.marquee_key.get(),
            Some((ScrollSurface::Library, 0))
        );

        advance_frames(&mut app, 60);
        let later = selected_marquee(&app, ScrollSurface::Library, 0, text, 12);
        assert_ne!(first, later);

        let reset = selected_marquee(&app, ScrollSurface::Search, 2, text, 12);
        assert_eq!(
            app.bridges.marquee_key.get(),
            Some((ScrollSurface::Search, 2))
        );
        assert!(reset.starts_with("A title"));
    }

    #[test]
    fn one_shot_and_ui_helpers_produce_expected_output_when_armed() {
        let _guard = crate::i18n::lock_for_test();
        let mut app = App::new(100);
        app.config.animations.master = true;
        app.config.animations.track_intro = true;
        app.config.animations.toast = true;
        app.config.animations.selection = true;
        app.config.animations.stagger = true;
        app.config.animations.caret = true;
        app.config.animations.tabs = true;
        app.config.animations.activity = true;
        app.config.animations.lyrics = true;
        app.config.animations.about_fx = true;
        app.config.animations.eq_bars = true;
        app.fx.track_intro = Some(0);
        app.fx.toast = Some(0);
        app.fx.list = Some((0, Mode::Library));
        app.fx.switch = Some((0, Mode::Search));
        app.fx.tabbar = Some(0);
        app.fx.lyric = Some(0);
        app.status.text = "Saved track".to_owned();
        advance_frames(&mut app, 3);

        assert!(
            line_text(&title_intro_line(&app, "Title", "Artist", true, 40).unwrap()).contains('♥')
        );
        assert!(line_text(&status_toast_line(&app, 40).unwrap()).contains("Sav"));
        assert_eq!(activity_dots(&app).unwrap().len(), 3);
        assert!(download_spinner(&app).is_some_and(|s| !s.is_empty()));
        assert!(queue_marker(&app).is_some_and(|s| s.chars().count() == 2));
        assert!(line_text(&about_brand_line(&app, "yututui", "1.2.3").unwrap()).contains("1.2.3"));

        let base = Style::default()
            .fg(Color::Rgb(200, 200, 200))
            .bg(Color::Rgb(10, 10, 10));
        assert_ne!(selection_style(&app, base).bg, base.bg);
        assert_ne!(stagger_style(&app, Mode::Library, 0, base).fg, base.fg);
        assert_ne!(active_tab_style(&app, TabPop::Nav, base).bg, base.bg);
        assert_ne!(active_tab_style(&app, TabPop::Inner, base).bg, base.bg);
        assert_ne!(lyrics_current_style(&app, base).fg, base.fg);
        assert_eq!(lyrics_dim_style(&app, base, 1).fg, base.fg);
        let _ = lyrics_dim_style(&app, base, 8);
        assert_eq!(
            caret_span(&app, base, Color::Rgb(0, 0, 0)).content,
            "\u{2588}"
        );

        let period = app.anim_ms_frames(1100);
        let current = app.anim_frame();
        advance_frames(&mut app, period.saturating_sub(current));
        assert_eq!(caret_char(&app), '▏');
        let target = period + period * 4 / 5;
        let current = app.anim_frame();
        advance_frames(&mut app, target.saturating_sub(current));
        assert_eq!(caret_char(&app), ' ');
    }

    #[test]
    fn paint_overlays_and_canvas_modify_the_buffer_when_enabled() {
        let _guard = crate::i18n::lock_for_test();
        let mut app = App::new(100);
        app.config.animations.master = true;
        app.config.animations.seekbar = true;
        app.config.animations.seek_flash = true;
        app.config.animations.volume_flash = true;
        app.config.animations.like_burst = true;
        app.config.animations.popup_fade = true;
        app.config.animations.about_fx = true;
        app.config.animations.rain = true;
        app.config.animations.starfield = true;
        app.config.animations.visualizer = true;
        app.config.animations.donut = true;
        app.config.animations.bounce = true;
        app.fx.seek = Some(0);
        app.fx.volume = Some(0);
        app.fx.like = Some(0);
        app.fx.popup = Some(0);
        app.playback.volume = 60;
        advance_frames(&mut app, 5);

        let volume = render_with(&app, 12, 1, |f, app, area| {
            volume_flash_overlay(f, app, area);
        });
        let row = (0..volume.area.width)
            .map(|x| volume[(x, 0)].symbol())
            .collect::<Vec<_>>()
            .join("");
        assert!(
            row.contains('█') && row.contains('░'),
            "volume row: {row:?}"
        );

        let burst = render_with(&app, 40, 3, |f, app, area| {
            like_burst_overlay(f, app, area, 0);
        });
        assert!(
            (0..burst.area.height)
                .flat_map(|y| (0..burst.area.width).map(move |x| (x, y)))
                .any(|(x, y)| burst[(x, y)].symbol() != " "),
            "like burst should paint at least one spark"
        );

        let canvas = render_with(&app, 48, 16, |f, app, area| {
            render_canvas(f, app, area);
        });
        assert!(
            (0..canvas.area.height)
                .flat_map(|y| (0..canvas.area.width).map(move |x| (x, y)))
                .any(|(x, y)| canvas[(x, y)].symbol() != " "),
            "enabled canvas effects should paint nonblank cells"
        );

        let sparkles = render_with(&app, 32, 6, |f, app, area| {
            about_sparkles(f, app, area, Rect::new(12, 2, 8, 2));
        });
        assert!(
            (0..sparkles.area.height)
                .flat_map(|y| (0..sparkles.area.width).map(move |x| (x, y)))
                .any(|(x, y)| sparkles[(x, y)].symbol() != " "),
            "about sparkles should paint around the keep-clear rect"
        );
    }

    #[test]
    fn smooth_seek_ratio_guards_paused_missing_and_nonfinite_state() {
        let mut app = App::new(100);
        app.config.animations.master = true;
        app.config.animations.seekbar = true;

        assert_eq!(smooth_seek_ratio(&app, 0.25), 0.25);

        app.playback.paused = true;
        app.playback.time_pos = Some(10.0);
        app.playback.time_pos_at = Some(Instant::now() - Duration::from_secs(2));
        app.playback.duration = Some(100.0);
        assert_eq!(smooth_seek_ratio(&app, 0.25), 0.25);

        app.playback.paused = false;
        app.playback.time_pos = Some(f64::NAN);
        assert_eq!(smooth_seek_ratio(&app, 0.25), 0.25);
    }
}
