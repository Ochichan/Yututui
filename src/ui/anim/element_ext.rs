//! Second-wave element effects for the Now Playing surface: the seekbar's second-tick glow,
//! sparkles riding the playhead, a comet chasing the Player border, and the play/pause light
//! wave. All follow the parent module's contract — flag-gated one-liners at the call site,
//! identity/no-op when off, phase from [`App::anim_frame`], retro-safe glyphs.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::Span;

use super::{ease_out_cubic, fx_t, fx_window, hash32, lerp_color};
use crate::app::App;
use crate::theme::ThemeRole as R;

// ── time glow (second tick) ─────────────────────────────────────────────────

/// The current sub-second glow amount (1 right after a second rolls over, decaying to 0), or
/// `None` when the effect shouldn't run (flag off, paused, parked clock, radio stream — whose
/// gauge doesn't mean elapsed time). Uses the same `time_pos_at` interpolation anchor as
/// [`super::smooth_seek_ratio`], so the tick lands exactly when the label increments.
fn second_glow(app: &App) -> Option<f64> {
    let a = app.animations();
    if !(a.master && a.time_glow)
        || app.playback.paused
        || !app.animation_active()
        || app.current_is_radio_stream()
    {
        return None;
    }
    let (pos, at) = (app.playback.time_pos?, app.playback.time_pos_at?);
    let live = pos + at.elapsed().as_secs_f64() * app.playback.speed.max(0.0);
    if !live.is_finite() || live < 0.0 {
        return None;
    }
    // Fully lit at the rollover, gone ~450 ms later.
    let glow = 1.0 - (live.fract() / 0.45).min(1.0);
    (glow > 0.02).then_some(ease_out_cubic(glow))
}

/// The seekbar gauge style with the second-tick glow blended in; `base` unchanged when the
/// effect is off or idle, so the plain gauge stays byte-identical.
pub fn time_glow_gauge_style(app: &App, base: Style) -> Style {
    let Some(glow) = second_glow(app) else {
        return base;
    };
    let Some(fg) = base.fg else { return base };
    base.fg(lerp_color(fg, app.theme.color(R::Accent), glow * 0.6))
}

/// The seekbar's time label as a span: glows toward the accent for a beat as each second
/// lands, and stays the plain unstyled label the rest of the time (and always, when off).
pub fn time_glow_label(app: &App, label: String) -> Span<'static> {
    let Some(glow) = second_glow(app) else {
        return Span::raw(label);
    };
    Span::styled(
        label,
        Style::default().fg(lerp_color(
            app.theme.color(R::TextPrimary),
            app.theme.color(R::Accent),
            glow,
        )),
    )
}

// ── progress sparkle ────────────────────────────────────────────────────────

const SPARKLE_GLYPHS: [char; 3] = ['✦', '·', '*'];
const SPARKLE_GLYPHS_RETRO: [char; 3] = ['*', '.', '+'];

/// Tiny sparks dancing around the seekbar head while playback runs: up to three glyphs in the
/// ±2 cells around the head, each twinkling on its own hashed phase. Foreground-only writes,
/// so the gauge fill underneath stays put; a paused player draws nothing.
pub fn progress_sparkle_overlay(frame: &mut Frame, app: &App, area: Rect, ratio: f64) {
    let a = app.animations();
    if !(a.master && a.progress_sparkle)
        || app.playback.paused
        || !app.animation_active()
        || area.width < 8
        || area.height == 0
    {
        return;
    }
    let f = app.anim_frame();
    let glyphs = if app.retro_mode() {
        &SPARKLE_GLYPHS_RETRO
    } else {
        &SPARKLE_GLYPHS
    };
    let head = ((f64::from(area.width) * ratio) as u16).min(area.width.saturating_sub(1));
    let bright = Color::Rgb(255, 255, 255);
    let base = app.theme.color(R::Accent);
    let buf = frame.buffer_mut();
    for i in 0..3u64 {
        // Each spark re-picks its offset every ~10 frames and twinkles independently;
        // the dark part of the cycle leaves the underlying cell alone.
        let tw = super::wave(f + i * 13, 22 + i * 5);
        if tw < 0.35 {
            continue;
        }
        let jitter = i64::from(hash32(i * 7 + f / 10) % 5) - 2;
        let x = i64::from(area.x) + i64::from(head) + jitter;
        if x < i64::from(area.x) || x >= i64::from(area.right()) {
            continue;
        }
        if let Some(cell) = buf.cell_mut((x as u16, area.y)) {
            // Only decorate the gauge's own fill/empty cells (spaces and block eighths) —
            // never the time label's digits/punctuation, which must stay readable.
            if !matches!(
                cell.symbol(),
                " " | "▏" | "▎" | "▍" | "▌" | "▋" | "▊" | "▉" | "█" | "░" | "▒" | "▓"
            ) {
                continue;
            }
            cell.set_char(glyphs[(i as usize) % glyphs.len()])
                .set_fg(lerp_color(base, bright, tw));
        }
    }
}

// ── border chase ────────────────────────────────────────────────────────────

/// A short bright comet running clockwise around a view's outer border. Recolours the
/// foreground of the three cells behind its head — the border glyphs themselves are left
/// exactly as the block drew them, so this composes with (and outshines) the breathing tint.
pub fn border_chase_overlay(frame: &mut Frame, app: &App, area: Rect) {
    let a = app.animations();
    if !(a.master && a.border_chase) || area.width < 4 || area.height < 3 {
        return;
    }
    let w = u64::from(area.width);
    let h = u64::from(area.height);
    let perimeter = 2 * (w + h) - 4;
    let head = (app.anim_frame() * 2) % perimeter; // two cells per frame reads as a glide
    let base = app.theme.color(R::BorderPrimary);
    let bright = app.theme.color(R::AccentAlt);
    let buf = frame.buffer_mut();
    for k in 0..3u64 {
        let pos = (head + perimeter - k) % perimeter;
        // Unroll the perimeter index into (x, y), clockwise from the top-left corner.
        let (dx, dy) = if pos < w {
            (pos, 0)
        } else if pos < w + h - 1 {
            (w - 1, pos - w + 1)
        } else if pos < 2 * w + h - 2 {
            (2 * w + h - 3 - pos, h - 1)
        } else {
            (0, perimeter - pos)
        };
        let t = 1.0 - k as f64 / 3.0;
        if let Some(cell) = buf.cell_mut((area.x + dx as u16, area.y + dy as u16)) {
            cell.set_fg(lerp_color(base, bright, t));
        }
    }
}

// ── pause flash ─────────────────────────────────────────────────────────────

/// A light wave washing outward across the transport-controls row right after play/pause
/// toggles: cells near the expanding ring get their existing foreground blended toward
/// white, then everything settles. Recolour-only — glyphs and click targets are untouched.
pub fn pause_flash_overlay(frame: &mut Frame, app: &App, area: Rect) {
    let a = app.animations();
    if !(a.master && a.pause_flash) || area.width < 8 || area.height == 0 {
        return;
    }
    let Some(t) = fx_t(app, app.fx.pause, fx_window::PAUSE_MS) else {
        return;
    };
    let spread = ease_out_cubic(t);
    let cx = f64::from(area.x) + f64::from(area.width) / 2.0;
    let radius = spread * (f64::from(area.width) / 2.0 + 2.0);
    let fade = 1.0 - t;
    let buf = frame.buffer_mut();
    for i in 0..area.width {
        let x = area.x + i;
        let d = (f64::from(x) - cx).abs();
        // A 3-cell-wide ring, brightest at the wavefront.
        let band = (1.0 - (d - radius).abs() / 3.0).clamp(0.0, 1.0);
        if band <= 0.0 {
            continue;
        }
        if let Some(cell) = buf.cell_mut((x, area.y)) {
            let fg = cell.fg;
            cell.set_fg(lerp_color(fg, Color::Rgb(255, 255, 255), band * fade * 0.9));
        }
    }
}
