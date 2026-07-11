//! The player control box: title, seekbar, transport strip, and status line — the block
//! every screen's "now playing" chrome is built from. Three consumers: the Player view's
//! legacy Top layout (`render_at`, byte-identical rows), the bottom-docked box every
//! screen shows in Bottom mode (`render_docked`), and the miniplayer's rows
//! (`ui::views::mini`).

use std::borrow::Cow;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Gauge, Paragraph};
use unicode_width::UnicodeWidthStr;

use crate::app::{App, DownloadState, Mode, MouseTarget, StatusKind};
use crate::keymap::{Action, KeyContext};
use crate::queue::Repeat;
use crate::t;
use crate::theme::ThemeRole as R;
use crate::ui::buttons::{self, Seg};
use crate::util::format;

/// Render the control block into four caller-provided single-height rows — the Player
/// view's legacy top layout passes its own `rows[1]/[3]/[5]/[7]`, so the output is
/// byte-identical to the pre-extraction rendering.
pub fn render_at(
    frame: &mut Frame,
    app: &App,
    title: Rect,
    seek: Rect,
    controls: Rect,
    status: Rect,
) {
    render_title_row(frame, app, title, true);
    render_seekbar(frame, app, seek, true);
    render_controls(frame, app, controls, true);
    render_status_line(frame, app, status, true);

    // Heart burst around the title when the current track is liked. Skipped while a status
    // message covers the title (nothing to celebrate over) — drawn after the neighbours so the
    // sparks sit on top of the blank gap rows.
    if app.status.text.is_empty()
        && let Some(s) = app.queue.current()
    {
        let title_text = app.display_title(s);
        let artist = app.display_artist(s);
        let occupied =
            UnicodeWidthStr::width(format!("{title_text} — {artist}").as_str()) as u16 + 4;
        crate::ui::anim::like_burst_overlay(frame, app, title, occupied);
    }
}

/// Rows the docked control box occupies (its separator rule plus the four control rows) on
/// the current screen — `0` in the legacy `Top` position, where no screen carries docked
/// chrome and every view's constraints collapse to their pre-dock bytes.
pub fn docked_rows(app: &App) -> u16 {
    if app.control_box_active() {
        DOCKED_BOX_ROWS
    } else {
        0
    }
}

/// Separator rule + title + seekbar + controls + status. The Top layout's gap rows are
/// deliberately absent: they are top-of-screen airiness, and a docked bar reads better dense.
pub const DOCKED_BOX_ROWS: u16 = 5;

/// Render the docked control box above a view's footer row. The player animation clock only
/// ticks on the Player screen, so every other screen gets the static forms — a marquee or
/// VU bar frozen mid-frame reads as a glitch, not a pause. One-shot effects (status-toast
/// typewriter, volume flash) stay: their windows are wall-clock-driven and mode-independent.
pub fn render_docked(frame: &mut Frame, app: &App, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let animated = app.mode == Mode::Player;
    let rows = Layout::vertical([
        Constraint::Length(1), // separator rule
        Constraint::Length(1), // title
        Constraint::Length(1), // seekbar
        Constraint::Length(1), // transport controls
        Constraint::Length(1), // status line
    ])
    .split(area);
    // The rule is drawn from single-cell glyphs chosen at the source (not left to the retro
    // scrubber), like every other retro stand-in.
    let rule = if app.retro_mode() { "-" } else { "─" };
    frame.render_widget(
        Paragraph::new(
            Line::from(rule.repeat(usize::from(area.width)))
                .style(app.theme.style(R::BorderPrimary)),
        ),
        rows[0],
    );
    render_title_row(frame, app, rows[1], animated);
    render_seekbar(frame, app, rows[2], animated);
    render_controls(frame, app, rows[3], animated);
    render_status_line(frame, app, rows[4], animated);
    // No like-burst here: its sparks decorate the blank gap rows the docked box doesn't have.
}

/// Title (or an error, if playback failed). With the title/heart animations off this is the
/// plain bold line exactly as before; with them on, `anim::title_line` returns a shimmering /
/// scrolling line (and a pulsing ♥), which we render in place of it. A just-changed track's
/// intro cascade and a just-set status message's typewriter reveal each take precedence for
/// their short one-shot window, then fall through to these steady-state forms.
pub(in crate::ui) fn render_title_row(frame: &mut Frame, app: &App, area: Rect, animated: bool) {
    if !app.status.text.is_empty() {
        if let Some(line) = crate::ui::anim::status_toast_line(app, area.width) {
            frame.render_widget(Paragraph::new(line), area);
        } else {
            let role = match app.status.kind {
                StatusKind::Error => R::Error,
                StatusKind::Info => R::Success,
            };
            frame.render_widget(
                Paragraph::new(
                    Line::from(app.status.text.as_str())
                        .style(app.theme.style(role))
                        .alignment(Alignment::Center),
                ),
                area,
            );
        }
    } else if let Some(line) = app.queue.current().filter(|_| animated).and_then(|s| {
        let title = app.display_title(s);
        let artist = app.display_artist(s);
        let liked = app.library.is_favorite(&s.video_id);
        crate::ui::anim::title_intro_line(app, title.as_ref(), artist.as_ref(), liked, area.width)
            .or_else(|| {
                crate::ui::anim::title_line(app, title.as_ref(), artist.as_ref(), liked, area.width)
            })
    }) {
        frame.render_widget(Paragraph::new(line), area);
    } else {
        let text = match app.queue.current() {
            Some(s) => {
                let title = app.display_title(s);
                let artist = app.display_artist(s);
                let heart = if app.library.is_favorite(&s.video_id) {
                    "♥ "
                } else {
                    ""
                };
                format!("{heart}{title} — {artist}")
            }
            None => {
                // The search key respects rebinds (the chord bound to `OpenSearch`, not a
                // hardcoded letter).
                let key = app.keymap.label_for_display(
                    KeyContext::Player,
                    Action::OpenSearch,
                    app.retro_mode(),
                );
                if crate::i18n::is_korean() {
                    format!("재생 중인 곡 없음 — {key} 를 눌러 검색")
                } else {
                    format!("Nothing playing — press {key} to search")
                }
            }
        };
        frame.render_widget(
            Paragraph::new(
                Line::from(text.bold())
                    .style(app.theme.style(R::TextPrimary))
                    .alignment(Alignment::Center),
            ),
            area,
        );
    }
}

/// Seekbar. Ratio + label (incl. the None/zero-duration handling) live in `format` so the
/// edge cases are unit-tested without a frame buffer. With the seekbar animation on, the
/// fill also interpolates between mpv's ~1 Hz position reports (the label stays on whole
/// seconds), so the gauge glides instead of stepping. A live radio stream has no duration,
/// so its gauge shows the timeshift state instead (full = live edge, backed off = behind);
/// the position-interpolating smoother would fight that meaning, so radio skips it.
pub(in crate::ui) fn render_seekbar(frame: &mut Frame, app: &App, area: Rect, animated: bool) {
    let (ratio, label) = if app.current_is_radio_stream() {
        format::radio_seekbar(
            app.playback.time_pos,
            app.radio_behind_secs(),
            app.radio_live_synced(),
        )
    } else {
        let raw = format::seekbar_ratio(app.playback.time_pos, app.playback.duration);
        let ratio = if animated {
            crate::ui::anim::smooth_seek_ratio(app, raw)
        } else {
            raw
        };
        (
            ratio,
            format::seekbar_label(app.playback.time_pos, app.playback.duration),
        )
    };
    let seekbar = Gauge::default()
        .gauge_style(
            Style::default()
                .fg(app.theme.color(R::GaugeFilled))
                .bg(app.theme.color(R::GaugeEmpty)),
        )
        .ratio(ratio)
        .label(label);
    frame.render_widget(seekbar, area);
    // A bright comet sweeps the filled portion when the seekbar animation is on (no-op
    // otherwise), and a short ripple marks the head right after a seek. Skipped in the
    // static (off-Player) forms — a frozen comet is a permanent bright smear.
    if animated {
        crate::ui::anim::seekbar_overlay(frame, app, area, ratio);
        crate::ui::anim::seek_flash_overlay(frame, app, area, ratio);
    }
    // Publish the seekbar's screen rect so a mouse click can be hit-tested for seeking.
    app.hits.set_seekbar_rect(area);
}

/// The current track's tri-state rating glyph: 👍 liked, 👎 disliked, 🤔 neither. Language-neutral
/// Unicode — all three are width-2, so the status line never shifts as the state changes — so the
/// row reads identically in every UI language. Retro mode picks the single-cell `+`/`-`/`?`
/// stand-ins at the source (still one width across all states), so a 256-glyph console never
/// sees the emoji at all. `like` is favorite membership; `dislike` is the streaming engine's
/// hard-block flag — mutually exclusive, so one glyph covers both states, cycled by
/// [`Action::CycleRating`].
fn rating_glyph(liked: bool, disliked: bool, retro: bool) -> &'static str {
    match (retro, liked, disliked) {
        (false, true, _) => "👍",
        (false, false, true) => "👎",
        (false, false, false) => "🤔",
        (true, true, _) => "+",
        (true, false, true) => "-",
        (true, false, false) => "?",
    }
}

/// The `S:` shuffle toggle's state glyph. Both states of a mode share one display width, so the
/// centered status line never shifts on toggle: the non-retro cross pads to the 🔀 emoji's two
/// cells, and the retro pair are single-cell CP437 glyphs (`v`/`x`, the same checked/cross
/// convention the retro scrubber maps ✓/✗ to elsewhere).
fn shuffle_glyph(on: bool, retro: bool) -> &'static str {
    match (retro, on) {
        (false, true) => "🔀",
        (false, false) => "✗ ",
        (true, true) => "v",
        (true, false) => "x",
    }
}

/// The `R:` repeat toggle's state glyph — the media repeat-all / repeat-one symbols, or a cross
/// padded to their two cells when off. Retro mode picks single-cell CP437 glyphs at the source
/// instead of trusting the post-pass scrubber: `∞` repeat-all, `1` repeat-one, `x` off — three
/// distinct states where the old scrub collapsed 🔁 and 🔂 into the same letter.
fn repeat_glyph(mode: Repeat, retro: bool) -> &'static str {
    match (retro, mode) {
        (false, Repeat::Off) => "✗ ",
        (false, Repeat::All) => "🔁",
        (false, Repeat::One) => "🔂",
        (true, Repeat::Off) => "x",
        (true, Repeat::All) => "∞",
        (true, Repeat::One) => "1",
    }
}

/// The shuffle slot's stand-in on a live radio stream: the `LIVE:` sync verdict. Same
/// width discipline as [`shuffle_glyph`] — every state pads to the emoji pair's two cells
/// non-retro, and retro picks single-cell CP437 glyphs (the `v`/`x` check/cross convention,
/// `-` for unknown) at the source.
fn live_sync_glyph(synced: Option<bool>, retro: bool) -> &'static str {
    match (retro, synced) {
        (false, Some(true)) => "✓ ",
        (false, Some(false)) => "✗ ",
        (false, None) => "· ",
        (true, Some(true)) => "v",
        (true, Some(false)) => "x",
        (true, None) => "-",
    }
}

/// The repeat slot's stand-in on a live radio stream: the `SYNC:` re-sync action. One
/// state only, so the width invariant is trivial — but it still matches its non-radio
/// sibling's cells (two non-retro, one CP437 retro).
fn resync_glyph(retro: bool) -> &'static str {
    if retro { "»" } else { "🔄" }
}

/// The transport status line: state, queue position, rating, shuffle, repeat, speed, EQ, etc.
///
/// Rendered as click segments rather than one string so `R:` (toggles repeat) and
/// `eq:` (opens the preset dropdown) are mouse targets — but every segment shares the same
/// cyan style, so the line looks exactly like the plain status text it replaced. `eq:` is
/// always shown now (so the dropdown is always reachable); the rest stay conditional.
pub(in crate::ui) fn render_status_line(frame: &mut Frame, app: &App, area: Rect, animated: bool) {
    // Roomy separators normally; progressively tighter when the line wouldn't fit (narrow
    // terminals, or text zoom shrinking the virtual grid). Content always wins over air —
    // the `eq:` / `R:` toggles at the tail must stay visible and clickable.
    let fits = |parts: &Vec<(Option<MouseTarget>, Cow<'static, str>)>| {
        parts
            .iter()
            .map(|(_, text)| buttons::text_width(text))
            .sum::<u16>()
            <= area.width
    };
    // The last tier also sheds the non-clickable decorations (state word, speed, VU,
    // download tag), keeping every *control* reachable on even the tiniest grid.
    let (gap_w, parts) = [("    ", false), ("  ", false), (" ", false), (" ", true)]
        .iter()
        .map(|(gap, minimal)| {
            (
                buttons::text_width(gap),
                status_line_parts(app, gap, *minimal, animated),
            )
        })
        .find(|(_, parts)| fits(parts))
        .unwrap_or_else(|| (1, status_line_parts(app, " ", true, animated)));
    // The identify chip (`ID?` / `지듣노`) is the smallest control on the line, and its
    // trailing `?` sits right on its rect's edge — fold up to two cells of the flanking
    // gaps into its hit rect (never more than the gap itself, so it can't annex a
    // neighbouring control's cells).
    let id_pad = gap_w.min(2);
    let segments: Vec<Seg> = parts
        .iter()
        .map(|(target, text)| match target {
            Some(t @ MouseTarget::Player(Action::IdentifyNowPlaying)) => {
                Seg::padded_button(t.clone(), text.as_ref(), id_pad)
            }
            Some(t) => Seg::button(t.clone(), text.as_ref()),
            None => Seg::label(text.as_ref()),
        })
        .collect();
    // Same style for buttons and labels so the clickable parts are visually indistinguishable.
    let style = app.theme.style(R::PlayerLabel);
    buttons::render_segments(frame, app, area, &segments, style, style, Alignment::Center);
}

/// Build the transport status-line as `(target, text)` segments from app state — split out
/// from [`render_status_line`] so the conditional assembly (queue position, rating, shuffle /
/// repeat, speed, EQ, streaming mode, download tag) is unit-testable without a frame
/// buffer. A `None` target is a static label/spacing; spacing is its own label so a clickable
/// segment's hit rect hugs just its text.
/// The `minimal` variant is the last responsive tier: the playback state shrinks to its
/// one-cell glyph and purely informational tags (speed, VU bars, download progress)
/// drop away, so the clickable toggles never fall off the right edge.
/// `animated` is false in the docked box's static (off-Player) forms: the clock-driven
/// decorations (spinner, VU bars) drop instead of freezing mid-frame.
fn status_line_parts(
    app: &App,
    gap: &'static str,
    minimal: bool,
    animated: bool,
) -> Vec<(Option<MouseTarget>, Cow<'static, str>)> {
    let retro = app.retro_mode();
    let mut parts: Vec<(Option<MouseTarget>, Cow<'static, str>)> = Vec::with_capacity(16);
    // A braille throbber leads the line when the spinner animation is on (no-op otherwise). It's a
    // plain label, so `render_segments` keeps every later hit rect aligned to its rendered text.
    if animated && let Some(spin) = crate::ui::anim::spinner_prefix(app) {
        parts.push((None, Cow::Owned(format!("{spin} "))));
    }
    // EAW-neutral glyphs (one cell everywhere) — the ⏸/▶ media emoji widen to two
    // cells on some terminals (Windows), which drifts every later segment's hit rect
    // off its rendered text and makes `R:`/`eq:` unclickable. See `render_controls`.
    let state = match (minimal, app.playback.paused) {
        (true, true) => "‖",
        (true, false) => "▸",
        (false, true) => t!("‖ paused", "‖ 일시정지"),
        (false, false) => t!("▸ playing", "▸ 재생 중"),
    };
    parts.push((None, Cow::Borrowed(state)));
    if !app.queue.is_empty() {
        let (pos, _) = app.queue.position();
        // Clickable (opens the queue window) but styled exactly like the labels around it,
        // so the line looks unchanged — same as the `S:`/`R:` toggles next to it.
        parts.push((None, Cow::Borrowed(gap)));
        parts.push((
            Some(MouseTarget::QueuePos),
            Cow::Owned(format!("{pos}/{}", app.queue.len())),
        ));
    }
    // The current track's rating: one tri-state glyph (🤔/👍/👎) that replaces the old separate
    // ♥ favorite + ✗ dislike columns. Same `PlayerLabel` style and 4-space gap as `S:`/`R:` next
    // to it; click and the `f` key hit the same Action, so it mirrors the keyboard exactly.
    if let Some(cur) = app.queue.current() {
        let liked = app.library.is_favorite(&cur.video_id);
        let disliked = app.signals.is_disliked(&cur.video_id);
        parts.push((None, Cow::Borrowed(gap)));
        parts.push((
            Some(MouseTarget::Player(Action::CycleRating)),
            Cow::Borrowed(rating_glyph(liked, disliked, retro)),
        ));
    }
    // Shuffle and repeat are both always shown as click toggles, and every state of each keeps
    // one display width, so the line's layout never shifts as they toggle or appear. Each
    // carries its media glyph — `S:🔀` shuffle, `R:🔁`/`R:🔂` repeat — or a padded cross when
    // off, so they read the same in every UI language.
    // On a live radio stream the same two slots (and the same actions/keys behind them)
    // read as the live transport instead: `LIVE:` sync verdict, `SYNC:` re-sync.
    if app.current_is_radio_stream() {
        parts.push((None, Cow::Borrowed(gap)));
        parts.push((
            Some(MouseTarget::Player(Action::ToggleShuffle)),
            Cow::Owned(format!(
                "LIVE:{}",
                live_sync_glyph(app.radio_live_synced(), retro)
            )),
        ));
        parts.push((None, Cow::Borrowed(gap)));
        parts.push((
            Some(MouseTarget::Player(Action::CycleRepeat)),
            Cow::Owned(format!("SYNC:{}", resync_glyph(retro))),
        ));
        // The "what's playing" card — now ICY-metadata-backed, so always offered on a live
        // radio stream, DJ Gem or not. A text label, not a glyph: EAW-ambiguous symbols
        // (♪ …) render wide on some CJK terminals and would drift every later hit rect.
        parts.push((None, Cow::Borrowed(gap)));
        parts.push((
            Some(MouseTarget::Player(Action::IdentifyNowPlaying)),
            Cow::Borrowed(t!("ID?", "지듣노")),
        ));
        // While the radio recorder is writing a track, a click-to-open `REC` chip. A plain
        // text label (not a glyph) for the same EAW-neutral reason as `ID?` above — an
        // ambiguous-width mark would drift later hit rects on some CJK terminals.
        if app.recorder.is_recording() {
            parts.push((None, Cow::Borrowed(gap)));
            parts.push((
                Some(MouseTarget::Player(Action::ToggleRecordings)),
                Cow::Borrowed(t!("REC", "녹음")),
            ));
        }
    } else {
        parts.push((None, Cow::Borrowed(gap)));
        parts.push((
            Some(MouseTarget::Player(Action::ToggleShuffle)),
            Cow::Owned(format!("S:{}", shuffle_glyph(app.queue.shuffle, retro))),
        ));
        parts.push((None, Cow::Borrowed(gap)));
        parts.push((
            Some(MouseTarget::Player(Action::CycleRepeat)),
            Cow::Owned(format!("R:{}", repeat_glyph(app.queue.repeat, retro))),
        ));
    }
    if !minimal && (app.playback.speed - 1.0).abs() > f64::EPSILON {
        parts.push((None, Cow::Owned(format!("{gap}{:.1}x", app.playback.speed))));
    }
    parts.push((None, Cow::Borrowed(gap)));
    parts.push((
        Some(MouseTarget::EqMenu),
        Cow::Owned(format!("eq:{}", app.audio.preset.label())),
    ));
    // Faux VU bars trail the EQ label when the EQ-bars animation is on (no-op otherwise).
    if !minimal
        && animated
        && let Some(bars) = crate::ui::anim::eq_bars(app)
    {
        parts.push((None, Cow::Owned(format!("{gap}{bars}"))));
    }
    if app.streaming_active() {
        // Show the station's mode (Focused/Balanced/Discovery) as a click target that opens the
        // mode dropdown — same affordance as the `eq:` label next to it.
        parts.push((None, Cow::Borrowed(gap)));
        parts.push((
            Some(MouseTarget::StreamingMenu),
            Cow::Owned(format!(
                "streaming:{}",
                app.config.streaming.mode.label().to_lowercase()
            )),
        ));
    }
    // Download indicator for the current track, if one is in flight or finished. While one is
    // actually running and the activity animation is on, a spinner stands in for the `⬇` so
    // live progress reads as motion (same single cell, so the line never shifts).
    if !minimal
        && let Some(s) = app.queue.current()
        && let Some(state) = app.downloads.active.get(&s.video_id)
    {
        let tag = match state {
            DownloadState::Running(p) => {
                let head = crate::ui::anim::download_spinner(app).unwrap_or_else(|| "⬇".to_owned());
                format!("{head} {p}%")
            }
            DownloadState::Done => "⬇ ✓".to_owned(),
            DownloadState::Failed => "⬇ ✗".to_owned(),
        };
        parts.push((None, Cow::Owned(format!("{gap}{tag}"))));
    }

    parts
}

/// The transport strip under the seekbar: skip-back / play-pause / skip-forward, then a
/// volume nudge cluster (`- 50% +`). No boxes — the glyphs *are* the buttons, so it
/// reads like a real now-playing bar rather than a row of GUI buttons. Each control is
/// padded to a few cells so it's an easy click target. The toggle shows the action
/// (`▸` play when paused, `‖` pause when playing) and both glyphs are one cell, so the
/// strip never reflows. Volume lives here (not the status line) to anchor the `-`/`+`
/// controls to the number they change.
///
/// Every glyph is a plain non-emoji symbol (EAW-neutral, one cell everywhere) — unlike
/// the ⏮/⏯ media emoji, which some terminals widen to two cells and so drift the click
/// rects off the rendered glyph.
pub(in crate::ui) fn render_controls(frame: &mut Frame, app: &App, area: Rect, animated: bool) {
    let toggle = if app.playback.paused {
        " ▸ "
    } else {
        " ‖ "
    };
    let animations = app.animations();
    let vol = if animations.master && animations.volume_flash {
        // The flash morphs this cluster into a gauge, so keep its numeric slot fixed while the
        // effect is enabled. The disabled branch deliberately retains the legacy bytes.
        format!("{:>3}%", app.playback.volume.clamp(0, 100))
    } else {
        format!("{}%", app.playback.volume)
    };
    // Roomy gaps normally; tighter ones when the strip wouldn't fit (a narrow terminal or
    // text zoom shrinking the virtual grid). Buttons keep their one-cell inner padding, so
    // hit targets stay comfortable — only the dead space between them compresses.
    let full: u16 = 3 * 3
        + 3
        + 3
        + 6
        + buttons::text_width(t!("vol ", "볼륨 "))
        + 3
        + 3
        + buttons::text_width(&vol);
    let (gap, vol_gap) = if full <= area.width {
        ("   ", "      ")
    } else {
        (" ", "  ")
    };
    let segments = [
        Seg::button(MouseTarget::Player(Action::PrevTrack), " ⇤ "),
        Seg::label(gap),
        Seg::button(MouseTarget::Player(Action::TogglePause), toggle),
        Seg::label(gap),
        Seg::button(MouseTarget::Player(Action::NextTrack), " ⇥ "),
        Seg::label(vol_gap),
        Seg::label(t!("vol ", "볼륨 ")),
        Seg::button(MouseTarget::Player(Action::VolDown), " - "),
        Seg::label(&vol),
        Seg::button(MouseTarget::Player(Action::VolUp), " + "),
    ];
    let widths: Vec<u16> = segments
        .iter()
        .map(|s| buttons::text_width(s.text))
        .collect();
    let total = widths.iter().copied().sum::<u16>();
    let volume_offset = widths[..6].iter().copied().sum::<u16>();
    let volume_width = widths[6..].iter().copied().sum::<u16>();
    let x = area.x + area.width.saturating_sub(total) / 2 + volume_offset;
    let width = if x < area.right() {
        volume_width.min(area.right().saturating_sub(x))
    } else {
        0
    };
    let vol_rect = Rect {
        x,
        y: area.y,
        width,
        height: area.height.min(1),
    };
    app.register_mouse_button(vol_rect, MouseTarget::VolumeArea);
    let controls_base = app
        .theme
        .style(R::PlayerControl)
        .add_modifier(Modifier::BOLD);
    // The pulse lerps by the animation clock, which only ticks on the Player screen — the
    // static forms keep the plain control colour instead of a frozen mid-pulse tint.
    let controls = if animated {
        crate::ui::anim::controls_style(app, controls_base)
    } else {
        controls_base
    };
    let labels = app.theme.style(R::PlayerLabel);
    buttons::render_segments(
        frame,
        app,
        area,
        &segments,
        controls,
        labels,
        Alignment::Center,
    );
    // Right after a volume nudge, the transient gauge paints directly over the
    // `vol - 50% +` cells (no-op outside its one-shot window). Paint only — the -/+ and
    // wheel hit rects registered above stay live, so the buttons keep taking clicks
    // while the gauge covers their glyphs (repeated +/+/+ nudges land blind).
    crate::ui::anim::volume_flash_overlay(frame, app, vol_rect);
}

/// The EQ preset dropdown, anchored under the `eq:` label and listing the built-in presets
/// (the active one highlighted). Each row is a click target that selects that preset.
pub fn render_eq_dropdown(frame: &mut Frame, app: &App, area: Rect) {
    let rows: Vec<(String, bool, MouseTarget)> = crate::eq::EqPreset::CYCLE
        .iter()
        .map(|p| {
            (
                p.label().to_owned(),
                *p == app.audio.preset,
                MouseTarget::EqSelect(*p),
            )
        })
        .collect();
    render_dropdown(frame, app, area, MouseTarget::EqMenu, " EQ ", &rows);
}

/// The streaming-mode dropdown, anchored under the `streaming:` label and listing the station modes
/// (the active one highlighted). Each row is a click target that switches the mode. Mirrors
/// the `eq:` dropdown exactly.
pub fn render_streaming_dropdown(frame: &mut Frame, app: &App, area: Rect) {
    let cur = app.config.streaming.mode;
    let rows: Vec<(String, bool, MouseTarget)> = crate::streaming::StreamingMode::CYCLE
        .iter()
        .map(|m| {
            (
                m.label().to_owned(),
                *m == cur,
                MouseTarget::StreamingSelect(*m),
            )
        })
        .collect();
    render_dropdown(
        frame,
        app,
        area,
        MouseTarget::StreamingMenu,
        t!(" Streaming ", " 스트리밍 "),
        &rows,
    );
}

/// Compact dropdown box width: the widest label + a one-cell left inset + two border columns.
fn dropdown_width<'a>(labels: impl Iterator<Item = &'a str>) -> u16 {
    labels
        .map(|l| UnicodeWidthStr::width(l) as u16)
        .max()
        .unwrap_or(0)
        + 1
        + 2
}

/// A compact status-line dropdown shared by the `eq:` and `streaming:` labels. Anchored under the
/// label whose hit rect matches `anchor_target`; titled `title`; lists `rows` of
/// `(label, is_active, click-target)`. The active row is a full-width highlight bar (no arrow
/// gutter) so the box stays exactly as wide as the longest label. Drawn over whatever is
/// beneath it with an opaque text-cell background; dismissed by picking a row or clicking elsewhere.
fn render_dropdown(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    anchor_target: MouseTarget,
    title: &str,
    rows: &[(String, bool, MouseTarget)],
) {
    // Anchor under the label, whose hit rect the status line just published.
    let Some(anchor) = app.hits.rect_of_target(anchor_target) else {
        return;
    };

    let box_w = dropdown_width(rows.iter().map(|(l, _, _)| l.as_str()));
    let box_h = rows.len() as u16 + 2;
    // Drop below the label; when the anchor sits low (the docked box's status line) and
    // there's room above, drop up instead — bottom-clamping would smother the very line
    // the menu was opened from. Clamp against the screen edges either way.
    let x = anchor.x.min(area.right().saturating_sub(box_w));
    let below_room = area.bottom().saturating_sub(anchor.y + 1);
    let y = if below_room >= box_h || anchor.y.saturating_sub(area.y) < box_h {
        (anchor.y + 1).min(area.bottom().saturating_sub(box_h))
    } else {
        anchor.y - box_h
    };
    let popup = Rect {
        x,
        y,
        width: box_w,
        height: box_h,
    }
    .intersection(area);
    if popup.is_empty() {
        return;
    }

    crate::ui::render_popup_background(frame, app, popup);
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(crate::ui::popup_style(app, R::BorderPrimary))
        .style(crate::ui::popup_style(app, R::TextPrimary));
    let list = block.inner(popup);
    frame.render_widget(block, popup);

    for (i, (label, active, target)) in rows.iter().enumerate() {
        let i = i as u16;
        if i >= list.height {
            break; // out of room (tiny terminal) — don't register off-box rows
        }
        let row = Rect {
            x: list.x,
            y: list.y + i,
            width: list.width,
            height: 1,
        };
        // One-cell inset, then pad to the full row width so the active row's highlight bar
        // spans it (the trailing cells read as the selection bar, not as empty box).
        let mut text = format!(" {label}");
        let pad = (list.width as usize).saturating_sub(UnicodeWidthStr::width(text.as_str()));
        text.push_str(&" ".repeat(pad));
        let style = if *active {
            crate::ui::anim::selection_style(
                app,
                Style::default()
                    .fg(app.theme.color(R::SelectionFg))
                    .bg(app.theme.color(R::SelectionBg))
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            app.theme.style(R::TextPrimary)
        };
        frame.render_widget(Paragraph::new(Line::from(text).style(style)), row);
        app.register_mouse_button(row, target.clone());
    }
    crate::ui::seal_popup_background(frame, app, popup);
    crate::ui::mark_art_rows_for_popup(frame, app, popup);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Song;

    fn has_target(
        parts: &[(Option<MouseTarget>, Cow<'static, str>)],
        pred: impl Fn(&MouseTarget) -> bool,
    ) -> bool {
        parts.iter().any(|(t, _)| t.as_ref().is_some_and(&pred))
    }

    #[test]
    fn status_line_always_offers_shuffle_repeat_and_eq() {
        let app = App::new(100);
        let parts = status_line_parts(&app, "    ", false, true);
        // Shuffle, repeat and the EQ menu are always present so the line never reflows.
        assert!(has_target(&parts, |t| matches!(
            t,
            MouseTarget::Player(Action::ToggleShuffle)
        )));
        assert!(has_target(&parts, |t| matches!(
            t,
            MouseTarget::Player(Action::CycleRepeat)
        )));
        assert!(has_target(&parts, |t| matches!(t, MouseTarget::EqMenu)));
        // Idle queue: no position label and no rating glyph (nothing is current).
        assert!(!has_target(&parts, |t| matches!(t, MouseTarget::QueuePos)));
        assert!(!has_target(&parts, |t| matches!(
            t,
            MouseTarget::Player(Action::CycleRating)
        )));
    }

    #[test]
    fn toggle_glyphs_share_one_width_per_mode() {
        // The status line is centered, so a width change in any always-present segment
        // shifts the whole line. Every state of shuffle / repeat / rating must therefore
        // measure the same in its mode — including retro, where the old post-pass scrub
        // turned `S:🔀`→`S:S ` but `S:✗`→`S:x` and nudged the line on every toggle.
        let w = UnicodeWidthStr::width;
        for retro in [false, true] {
            assert_eq!(
                w(shuffle_glyph(true, retro)),
                w(shuffle_glyph(false, retro))
            );
            assert_eq!(
                w(repeat_glyph(Repeat::All, retro)),
                w(repeat_glyph(Repeat::One, retro))
            );
            assert_eq!(
                w(repeat_glyph(Repeat::All, retro)),
                w(repeat_glyph(Repeat::Off, retro))
            );
            assert_eq!(
                w(rating_glyph(true, false, retro)),
                w(rating_glyph(false, true, retro))
            );
            assert_eq!(
                w(rating_glyph(true, false, retro)),
                w(rating_glyph(false, false, retro))
            );
        }
    }

    #[test]
    fn retro_toggle_glyphs_are_distinct_single_cp437_cells() {
        // Repeat-all and repeat-one must stay tellable apart on a basic console (the old
        // scrub mapped both 🔁 and 🔂 to `R`), and every retro state glyph must be a plain
        // single-cell symbol so the scrubber has nothing left to rewrite.
        let states = [
            shuffle_glyph(true, true),
            shuffle_glyph(false, true),
            repeat_glyph(Repeat::Off, true),
            repeat_glyph(Repeat::All, true),
            repeat_glyph(Repeat::One, true),
            rating_glyph(true, false, true),
            rating_glyph(false, true, true),
            rating_glyph(false, false, true),
        ];
        for s in states {
            assert_eq!(UnicodeWidthStr::width(s), 1, "{s:?} must be one cell");
        }
        assert_ne!(
            repeat_glyph(Repeat::All, true),
            repeat_glyph(Repeat::One, true)
        );
        assert_ne!(
            repeat_glyph(Repeat::All, true),
            repeat_glyph(Repeat::Off, true)
        );
        assert_ne!(
            repeat_glyph(Repeat::One, true),
            repeat_glyph(Repeat::Off, true)
        );
        assert_ne!(shuffle_glyph(true, true), shuffle_glyph(false, true));
    }

    #[test]
    fn status_line_width_is_invariant_across_shuffle_and_repeat_states() {
        let mut app = App::new(100);
        app.queue.set(vec![Song::remote("a", "A", "x", "1:00")], 0);
        for retro in [false, true] {
            app.config.retro_mode = retro;
            let mut widths = Vec::new();
            for shuffle in [false, true] {
                for repeat in [Repeat::Off, Repeat::All, Repeat::One] {
                    app.queue.shuffle = shuffle;
                    app.queue.repeat = repeat;
                    let total: usize = status_line_parts(&app, "    ", false, true)
                        .iter()
                        .map(|(_, s)| UnicodeWidthStr::width(s.as_ref()))
                        .sum();
                    widths.push(total);
                }
            }
            assert!(
                widths.windows(2).all(|w| w[0] == w[1]),
                "retro={retro}: line width changed across toggle states: {widths:?}"
            );
        }
    }

    fn radio_song() -> Song {
        Song::from_source(
            crate::search_source::SearchSource::RadioBrowser,
            "groove",
            "Groove FM",
            "KR / MP3",
            "",
            crate::api::PlayableRef::RadioStream {
                url: "https://example.com/groove.mp3".to_owned(),
            },
        )
    }

    #[test]
    fn radio_stream_swaps_shuffle_repeat_for_live_transport_controls() {
        let mut app = App::new(100);
        app.queue.set(vec![radio_song()], 0);
        let parts = status_line_parts(&app, "    ", false, true);
        // The same two actions stay clickable, but read as the live transport.
        let live = parts
            .iter()
            .find(|(t, _)| matches!(t, Some(MouseTarget::Player(Action::ToggleShuffle))))
            .expect("live-sync segment");
        assert!(live.1.starts_with("LIVE:"), "got {:?}", live.1);
        let sync = parts
            .iter()
            .find(|(t, _)| matches!(t, Some(MouseTarget::Player(Action::CycleRepeat))))
            .expect("re-sync segment");
        assert!(sync.1.starts_with("SYNC:"), "got {:?}", sync.1);
        assert!(
            !parts
                .iter()
                .any(|(_, s)| s.starts_with("S:") || s.starts_with("R:")),
            "music-mode shuffle/repeat labels must not appear on radio"
        );
        // The minimal responsive tier keeps both controls reachable.
        let parts = status_line_parts(&app, " ", true, true);
        assert!(has_target(&parts, |t| matches!(
            t,
            MouseTarget::Player(Action::ToggleShuffle)
        )));
        assert!(has_target(&parts, |t| matches!(
            t,
            MouseTarget::Player(Action::CycleRepeat)
        )));
    }

    #[test]
    fn identify_segment_is_offered_on_any_radio_stream() {
        let mut app = App::new(100);
        app.queue.set(vec![radio_song()], 0);
        // Radio with DJ Gem down: still offered — 지듣노 is ICY-metadata-backed now — and it
        // survives the minimal responsive tier because it's a control.
        for minimal in [false, true] {
            let parts = status_line_parts(&app, " ", minimal, true);
            assert!(has_target(&parts, |t| matches!(
                t,
                MouseTarget::Player(Action::IdentifyNowPlaying)
            )));
        }
        // DJ Gem up changes nothing on the radio path — still offered.
        app.ai.available = true;
        let parts = status_line_parts(&app, "    ", false, true);
        assert!(has_target(&parts, |t| matches!(
            t,
            MouseTarget::Player(Action::IdentifyNowPlaying)
        )));
        // Music mode never offers it, DJ Gem or not.
        let mut app = App::new(100);
        app.ai.available = true;
        app.queue.set(vec![Song::remote("a", "A", "x", "1:00")], 0);
        let parts = status_line_parts(&app, "    ", false, true);
        assert!(!has_target(&parts, |t| matches!(
            t,
            MouseTarget::Player(Action::IdentifyNowPlaying)
        )));
    }

    #[test]
    fn live_transport_glyphs_share_one_width_per_mode() {
        let w = UnicodeWidthStr::width;
        for retro in [false, true] {
            let widths: Vec<usize> = [Some(true), Some(false), None]
                .iter()
                .map(|s| w(live_sync_glyph(*s, retro)))
                .collect();
            assert!(
                widths.windows(2).all(|p| p[0] == p[1]),
                "retro={retro}: live-sync widths differ: {widths:?}"
            );
            // The stand-ins must occupy their slot's exact cells so swapping the labels
            // in radio mode is the only width change (LIVE:/SYNC: vs S:/R:).
            assert_eq!(
                w(live_sync_glyph(Some(true), retro)),
                w(shuffle_glyph(true, retro))
            );
            assert_eq!(w(resync_glyph(retro)), w(repeat_glyph(Repeat::All, retro)));
        }
        // Retro stand-ins are single-cell CP437 glyphs, distinct per state.
        assert_eq!(UnicodeWidthStr::width(resync_glyph(true)), 1);
        for s in [Some(true), Some(false), None] {
            assert_eq!(UnicodeWidthStr::width(live_sync_glyph(s, true)), 1);
        }
        assert_ne!(
            live_sync_glyph(Some(true), true),
            live_sync_glyph(Some(false), true)
        );
        assert_ne!(
            live_sync_glyph(Some(true), true),
            live_sync_glyph(None, true)
        );
        assert_ne!(
            live_sync_glyph(Some(false), true),
            live_sync_glyph(None, true)
        );
    }

    #[test]
    fn status_line_width_is_invariant_across_radio_sync_states() {
        let mut app = App::new(100);
        app.queue.set(vec![radio_song()], 0);
        for retro in [false, true] {
            app.config.retro_mode = retro;
            let mut widths = Vec::new();
            // Synced, behind, and unknown must all measure the same.
            for (pos, edge) in [
                (Some(100.0), Some(103.0)),
                (Some(100.0), Some(300.0)),
                (None, None),
            ] {
                app.playback.time_pos = pos;
                app.playback.cache_time = edge;
                app.playback.cache_time_at = edge.map(|_| std::time::Instant::now());
                let total: usize = status_line_parts(&app, "    ", false, true)
                    .iter()
                    .map(|(_, s)| UnicodeWidthStr::width(s.as_ref()))
                    .sum();
                widths.push(total);
            }
            assert!(
                widths.windows(2).all(|w| w[0] == w[1]),
                "retro={retro}: line width changed across sync states: {widths:?}"
            );
        }
    }

    #[test]
    fn status_line_shows_position_and_rating_once_a_track_is_current() {
        let mut app = App::new(100);
        app.queue.set(vec![Song::remote("a", "A", "x", "1:00")], 0);
        let parts = status_line_parts(&app, "    ", false, true);
        // The clickable N/M position label appears, reading "1/1".
        assert!(
            parts
                .iter()
                .any(|(t, s)| matches!(t, Some(MouseTarget::QueuePos)) && s == "1/1")
        );
        // A current track means the tri-state rating glyph is offered.
        assert!(has_target(&parts, |t| matches!(
            t,
            MouseTarget::Player(Action::CycleRating)
        )));
    }
}
