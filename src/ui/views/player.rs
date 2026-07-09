//! The now-playing view: current track, a seekbar with `M:SS / M:SS`, transport state,
//! and queue indicators (position, shuffle, repeat).

use std::borrow::Cow;

use image::imageops::FilterType;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Gauge, Paragraph};
use ratatui_image::{Resize, StatefulImage};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{App, DownloadState, MouseTarget, RadioModeConfirm, ScrollSurface, StatusKind};
use crate::keymap::{Action, KeyContext};
use crate::lyrics;
use crate::queue::Repeat;
use crate::t;
use crate::theme::ThemeRole as R;
use crate::ui::buttons::{self, Seg};
use crate::util::format;

pub fn render(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(crate::ui::anim::border_style(
            app,
            app.theme.style(R::BorderPrimary),
        ))
        .style(app.theme.style(R::TextPrimary));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // The nav strip rides the top border line itself; `render_nav` overlays only the cells
    // its text covers, so the border keeps drawing on either side of it.
    buttons::render_nav(
        frame,
        app,
        Rect {
            x: inner.x,
            y: area.y,
            width: inner.width,
            height: 1,
        },
    );

    let rows = Layout::vertical([
        Constraint::Length(1), // gap (border → title)
        Constraint::Length(1), // title
        Constraint::Length(1), // gap (title → seekbar)
        Constraint::Length(1), // seekbar
        Constraint::Length(1), // gap (seekbar → controls)
        Constraint::Length(1), // mouse controls
        Constraint::Length(1), // gap (controls → status)
        Constraint::Length(1), // transport status
        Constraint::Min(0),    // filler
        Constraint::Length(1), // help
    ])
    .split(inner);

    // Title (or an error, if playback failed). With the title/heart animations off this is the
    // plain bold line exactly as before; with them on, `anim::title_line` returns a shimmering /
    // scrolling line (and a pulsing ♥), which we render in place of it. A just-changed track's
    // intro cascade and a just-set status message's typewriter reveal each take precedence for
    // their short one-shot window, then fall through to these steady-state forms.
    if !app.status.text.is_empty() {
        if let Some(line) = crate::ui::anim::status_toast_line(app, rows[1].width) {
            frame.render_widget(Paragraph::new(line), rows[1]);
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
                rows[1],
            );
        }
    } else if let Some(line) = app.queue.current().and_then(|s| {
        let title = app.display_title(s);
        let artist = app.display_artist(s);
        let liked = app.library.is_favorite(&s.video_id);
        crate::ui::anim::title_intro_line(
            app,
            title.as_ref(),
            artist.as_ref(),
            liked,
            rows[1].width,
        )
        .or_else(|| {
            crate::ui::anim::title_line(app, title.as_ref(), artist.as_ref(), liked, rows[1].width)
        })
    }) {
        frame.render_widget(Paragraph::new(line), rows[1]);
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
            rows[1],
        );
    }

    // Seekbar. Ratio + label (incl. the None/zero-duration handling) live in `format` so the
    // edge cases are unit-tested without a frame buffer. With the seekbar animation on, the
    // fill also interpolates between mpv's ~1 Hz position reports (the label stays on whole
    // seconds), so the gauge glides instead of stepping. A live radio stream has no duration,
    // so its gauge shows the timeshift state instead (full = live edge, backed off = behind);
    // the position-interpolating smoother would fight that meaning, so radio skips it.
    let (ratio, label) = if app.current_is_radio_stream() {
        format::radio_seekbar(
            app.playback.time_pos,
            app.radio_behind_secs(),
            app.radio_live_synced(),
        )
    } else {
        let ratio = crate::ui::anim::smooth_seek_ratio(
            app,
            format::seekbar_ratio(app.playback.time_pos, app.playback.duration),
        );
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
    frame.render_widget(seekbar, rows[3]);
    // A bright comet sweeps the filled portion when the seekbar animation is on (no-op
    // otherwise), and a short ripple marks the head right after a seek.
    crate::ui::anim::seekbar_overlay(frame, app, rows[3], ratio);
    crate::ui::anim::seek_flash_overlay(frame, app, rows[3], ratio);
    // Publish the seekbar's screen rect so a mouse click can be hit-tested for seeking.
    app.hits.set_seekbar_rect(rows[3]);

    // The transient volume-nudge gauge rides the strip's own `vol - 50% +` cells now (see
    // `render_controls`), so the blank row below stays blank.
    render_controls(frame, app, rows[5]);

    render_status_line(frame, app, rows[7]);

    // Heart burst around the title when the current track is liked. Skipped while a status
    // message covers the title (nothing to celebrate over) — drawn after the neighbours so the
    // sparks sit on top of the blank gap rows.
    if app.status.text.is_empty()
        && let Some(s) = app.queue.current()
    {
        let title = app.display_title(s);
        let artist = app.display_artist(s);
        let occupied = unicode_width::UnicodeWidthStr::width(format!("{title} — {artist}").as_str())
            as u16
            + 4;
        crate::ui::anim::like_burst_overlay(frame, app, rows[1], occupied);
    }

    // Central filler: album art (top) and/or the lyrics panel (below). With album art off
    // this is exactly the old behaviour — lyrics fill the whole area, nothing else draws.
    render_filler(frame, app, rows[8]);

    // The full key list lives in the `?` cheat-sheet now; the footer just points to it
    // (chord pulled live from the keymap, so a remap of "toggle help" updates it).
    buttons::render_help_button(frame, app, rows[9]);

    // The status-line dropdowns draw over the screen so their rows win hit-testing.
    if app.dropdowns.eq_open {
        render_eq_dropdown(frame, app, inner);
    }
    if app.dropdowns.streaming_open {
        render_streaming_dropdown(frame, app, inner);
    }
    // The queue window draws last of all so it sits on top and its rects win.
    app.queue_popup.rect.set(None);
    if app.queue_popup.open {
        render_queue_popup(frame, app, inner);
    }
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
fn render_status_line(frame: &mut Frame, app: &App, area: Rect) {
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
                status_line_parts_at(app, gap, *minimal),
            )
        })
        .find(|(_, parts)| fits(parts))
        .unwrap_or_else(|| (1, status_line_parts_at(app, " ", true)));
    // The identify chip (`ID?` / `지듣노`) is the smallest control on the line, and its
    // trailing `?` sits right on its rect's edge — fold up to two cells of the flanking
    // gaps into its hit rect (never more than the gap itself, so it can't annex a
    // neighbouring control's cells).
    let id_pad = gap_w.min(2);
    let segments: Vec<Seg> = parts
        .iter()
        .map(|(target, text)| match target {
            Some(t @ MouseTarget::Player(Action::IdentifyNowPlaying)) => {
                Seg::padded_button(*t, text.as_ref(), id_pad)
            }
            Some(t) => Seg::button(*t, text.as_ref()),
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
fn status_line_parts_at(
    app: &App,
    gap: &'static str,
    minimal: bool,
) -> Vec<(Option<MouseTarget>, Cow<'static, str>)> {
    let retro = app.retro_mode();
    let mut parts: Vec<(Option<MouseTarget>, Cow<'static, str>)> = Vec::with_capacity(16);
    // A braille throbber leads the line when the spinner animation is on (no-op otherwise). It's a
    // plain label, so `render_segments` keeps every later hit rect aligned to its rendered text.
    if let Some(spin) = crate::ui::anim::spinner_prefix(app) {
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
    if !minimal && let Some(bars) = crate::ui::anim::eq_bars(app) {
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

/// The queue window: a themed popup listing the whole play queue (current track marked),
/// opened by clicking the `N/M` position label. Rows are click targets — single-click
/// selects (drag to extend the selection), double-click jumps playback there — and each
/// row's trailing `✗` removes that track. Keyboard nav / Delete / Enter operate it while
/// open (see `App::on_key_queue`). Drawn last over everything so its rects win.
fn render_queue_popup(frame: &mut Frame, app: &App, area: Rect) {
    let total_len = app.queue.len();
    if total_len == 0 {
        return;
    }
    let (pos, total) = app.queue.position();
    let current = app.queue.cursor_pos();

    // ~60% wide, tall enough for the list (+2 for the border) but capped to ~70% of the screen.
    let popup = crate::ui::centered_list_popup(area, total_len, 2, 24);
    if popup.is_empty() {
        return;
    }
    app.queue_popup.rect.set(Some(popup));

    crate::ui::render_popup_background(frame, app, popup);
    let block = Block::default()
        .title(format!(" {} {pos}/{total} ", t!("Queue", "대기열")))
        .borders(Borders::ALL)
        .border_style(crate::ui::popup_style(app, R::BorderPrimary))
        .style(crate::ui::popup_style(app, R::TextPrimary));
    let list = block.inner(popup);
    frame.render_widget(block, popup);
    if list.height == 0 || list.width < 4 {
        crate::ui::seal_popup_background(frame, app, popup);
        crate::ui::mark_art_rows_for_popup(frame, app, popup);
        return;
    }

    // The wheel scrolls this viewport freely; the render only nudges it to keep a
    // keyboard-moved cursor on-screen with a margin (see `ui::scroll`).
    let visible = list.height as usize;
    let cursor = app.queue_popup.cursor.min(total_len - 1);
    let start = app.queue_popup.scroll.resolve(
        cursor,
        list.height,
        total_len,
        crate::ui::scroll::SCROLLOFF,
    );
    let sel_lo = app.queue_popup.cursor.min(app.queue_popup.anchor);
    let sel_hi = app.queue_popup.cursor.max(app.queue_popup.anchor);

    const DEL_W: u16 = 2; // "✗ " click target on the right edge
    let body_w = list.width.saturating_sub(DEL_W) as usize;
    for (vis, (i, song)) in app
        .queue
        .ordered_iter()
        .enumerate()
        .skip(start)
        .take(visible)
        .enumerate()
    {
        let y = list.y + vis as u16;
        let selected = i >= sel_lo && i <= sel_hi;
        let is_current = i == current;
        // The now-playing marker becomes a two-cell mini VU while the EQ-bars animation is on
        // and something is actually playing (same width as `▸ `, so nothing shifts).
        let vu = if is_current {
            crate::ui::anim::queue_marker(app)
        } else {
            None
        };
        let marker = match &vu {
            Some(bars) => bars.as_str(),
            None if is_current => "▸ ",
            None => "  ",
        };
        let title = app.display_title(song);
        let artist = app.display_artist(song);
        let text = crate::ui::text::truncate_owned_to_width(
            format!("{marker}{:>3} {title} — {artist}", i + 1),
            body_w.saturating_sub(1),
        );

        let mut base = if selected {
            crate::ui::anim::selection_style(
                app,
                Style::default()
                    .fg(app.theme.color(R::SelectionFg))
                    .bg(app.theme.color(R::SelectionBg)),
            )
        } else if is_current {
            app.theme.style(R::Accent)
        } else {
            app.theme.style(R::TextPrimary)
        };
        if is_current {
            base = base.add_modifier(Modifier::BOLD);
        }
        let row = Rect {
            x: list.x,
            y,
            width: list.width,
            height: 1,
        };
        frame.render_widget(Paragraph::new(Line::from(text).style(base)), row);

        // Trailing ✗ delete button, kept on the row's highlight when selected.
        let del_x = row.x + row.width.saturating_sub(DEL_W);
        let del_rect = Rect {
            x: del_x,
            y,
            width: DEL_W,
            height: 1,
        };
        let mut del_style = app.theme.style(R::Error);
        if selected {
            del_style = del_style.bg(app.theme.color(R::SelectionBg));
        }
        frame.render_widget(Paragraph::new(Line::from("✗").style(del_style)), del_rect);
        app.register_mouse_button(del_rect, MouseTarget::QueueDel(i));

        // The row body (everything left of the ✗) selects / jumps to the track.
        let body_rect = Rect {
            x: row.x,
            y,
            width: row.width.saturating_sub(DEL_W),
            height: 1,
        };
        app.register_mouse_button(body_rect, MouseTarget::QueueRow(i));
    }

    // Scrollbar on the right border, tracking the viewport position; hidden when it all fits.
    buttons::render_list_scrollbar(
        frame,
        app,
        Rect {
            x: list.right(),
            y: list.y,
            width: 1,
            height: list.height,
        },
        ScrollSurface::Queue,
        total_len,
        start,
        visible,
    );
    crate::ui::seal_popup_background(frame, app, popup);
    crate::ui::mark_art_rows_for_popup(frame, app, popup);
}

/// The EQ preset dropdown, anchored under the `eq:` label and listing the built-in presets
/// (the active one highlighted). Each row is a click target that selects that preset.
fn render_eq_dropdown(frame: &mut Frame, app: &App, area: Rect) {
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
fn render_streaming_dropdown(frame: &mut Frame, app: &App, area: Rect) {
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
    // Drop below the label; clamp against the right/bottom edges so the box stays on screen.
    let x = anchor.x.min(area.right().saturating_sub(box_w));
    let y = (anchor.y + 1).min(area.bottom().saturating_sub(box_h));
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
        app.register_mouse_button(row, *target);
    }
    crate::ui::seal_popup_background(frame, app, popup);
    crate::ui::mark_art_rows_for_popup(frame, app, popup);
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
fn render_controls(frame: &mut Frame, app: &App, area: Rect) {
    let toggle = if app.playback.paused {
        " ▸ "
    } else {
        " ‖ "
    };
    let vol = format!("{}%", app.playback.volume);
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
    let controls = crate::ui::anim::controls_style(
        app,
        app.theme
            .style(R::PlayerControl)
            .add_modifier(Modifier::BOLD),
    );
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

/// Blank rows framing the album art: one between the transport status line and the art,
/// one between the art's bottom edge and the lyrics block.
const ART_TOP_GAP: u16 = 1;
const ART_LYRICS_GAP: u16 = 1;
/// Breathing room between the radio set piece's bottom edge and the one-line art. A
/// luxury, not a reservation: it collapses row-by-row on short terminals before the
/// separator or the lyrics window would lose space (see [`render_radio_filler`]).
const RADIO_ART_SEP_GAP: u16 = 2;
/// Smallest lyrics window (rows) we keep below the art when both are shown.
const MIN_LYRICS_ROWS: u16 = 3;

/// The central area below the transport strip. Album art (when enabled and ready) sits at
/// the top, just under the status line; the lyrics panel (when toggled) starts right below
/// the art's real bottom edge. When album art is off the layout is unchanged — lyrics get
/// the whole area, and an empty area draws nothing.
fn render_filler(frame: &mut Frame, app: &App, area: Rect) {
    app.art.rect.set(None);
    // The radio set piece rides the album-art toggle: with it off, radio mode falls
    // through to the plain music-mode arms below (`art_active()` is hard-false in radio
    // mode, so those resolve to the no-art layout — lyrics, or the bare animation canvas).
    if app.radio_dedicated_mode && app.config.effective_album_art() {
        render_radio_filler(frame, app, area);
        return;
    }
    match (app.art_active(), app.lyrics.visible) {
        // Art on top, lyrics right under it. The art is capped so a readable lyrics window
        // always remains, then lyrics start one row below the art's actual bottom (not at a
        // fixed split), so there's no dead gap between them.
        (true, true) => {
            let after_gap = area.height.saturating_sub(ART_TOP_GAP);
            // ~50% of the filler, but never so tall that lyrics drop below MIN_LYRICS_ROWS.
            let cap = (after_gap / 2).min(after_gap.saturating_sub(MIN_LYRICS_ROWS));
            let band = art_band(area, ART_TOP_GAP, cap);
            match draw_art(frame, app, band) {
                Some(art) => {
                    let lyrics_y = art.bottom().saturating_add(ART_LYRICS_GAP);
                    let lyrics_area = Rect {
                        x: area.x,
                        y: lyrics_y,
                        width: area.width,
                        height: area.bottom().saturating_sub(lyrics_y),
                    };
                    render_lyrics(frame, app, lyrics_area);
                }
                None => render_lyrics(frame, app, area),
            }
        }
        // Art only: medium size, capped to ~55% of the filler height, top-anchored. Canvas
        // animations fill the blank region below the art's real bottom edge (never over the art).
        (true, false) => {
            let cap = (u32::from(area.height) * 11 / 20) as u16;
            let band = art_band(area, ART_TOP_GAP, cap);
            let art = draw_art(frame, app, band);
            let below = art
                .map_or(area.y, |r| r.bottom())
                .saturating_add(ART_LYRICS_GAP);
            if below < area.bottom() {
                let zone = Rect {
                    x: area.x,
                    y: below,
                    width: area.width,
                    height: area.bottom() - below,
                };
                crate::ui::anim::render_canvas(frame, app, zone);
            }
        }
        (false, true) => render_lyrics(frame, app, area),
        // No art, no lyrics: the whole filler is blank — the maximum canvas. With every animation
        // off this stays empty (drawing nothing), exactly as before.
        (false, false) => crate::ui::anim::render_canvas(frame, app, area),
    }
}

fn render_art_animation_separator(frame: &mut Frame, app: &App, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    // The trailing space is deliberate: the motif is tiled edge-to-edge, and it keeps
    // each repetition from butting straight into the next one's leading note.
    const MOTIF: &str = "♫♪.ılılıll|̲̅●̲̅|̲̅=̲̅|̲̅●̲̅|llılılı.♫♪ ";
    let width = usize::from(area.width);
    let offset = if radio_art_animation_on(app) {
        (app.anim_frame() / 6) as usize
    } else {
        0
    };
    let line = repeated_motif_line(MOTIF, width, offset);
    frame.render_widget(
        Paragraph::new(
            Line::from(line)
                .style(app.theme.style(R::Accent).add_modifier(Modifier::BOLD))
                .alignment(Alignment::Center),
        ),
        area,
    );
}

fn repeated_motif_line(motif: &str, width: usize, offset: usize) -> String {
    let clusters = display_clusters(motif);
    if clusters.is_empty() {
        return " ".repeat(width);
    }
    let mut line = String::new();
    let mut i = offset % clusters.len();
    while UnicodeWidthStr::width(line.as_str()) < width {
        line.push_str(&clusters[i]);
        i = (i + 1) % clusters.len();
    }
    crate::ui::text::pad_to_width(
        &crate::ui::text::truncate_owned_to_width(line, width),
        width,
    )
}

fn display_clusters(s: &str) -> Vec<String> {
    let mut clusters = Vec::new();
    let mut current = String::new();
    for ch in s.chars() {
        if UnicodeWidthChar::width(ch).unwrap_or(0) > 0 && !current.is_empty() {
            clusters.push(std::mem::take(&mut current));
        }
        current.push(ch);
    }
    if !current.is_empty() {
        clusters.push(current);
    }
    clusters
}

fn radio_art_animation_on(app: &App) -> bool {
    app.radio_dedicated_mode
        && app.animations().master
        && !app.playback.paused
        && app.queue.current().is_some()
}

fn render_radio_filler(frame: &mut Frame, app: &App, area: Rect) {
    let after_gap = area.height.saturating_sub(ART_TOP_GAP);
    let cap = if app.lyrics.visible {
        after_gap.saturating_sub(MIN_LYRICS_ROWS).min(12)
    } else {
        after_gap.saturating_sub(ART_LYRICS_GAP).min(15)
    };
    let band = art_band(area, ART_TOP_GAP, cap);
    match draw_radio_ascii(frame, app, band) {
        Some(radio) => {
            // Give the one-line art breathing room below the set piece, shrinking the
            // gap first when the terminal is short: the separator row itself and (with
            // lyrics on) a readable lyrics window always win over the luxury rows.
            let reserved_below = 1 + if app.lyrics.visible {
                ART_LYRICS_GAP + MIN_LYRICS_ROWS
            } else {
                0
            };
            let slack = area
                .bottom()
                .saturating_sub(radio.bottom())
                .saturating_sub(reserved_below);
            let separator_y = radio.bottom() + RADIO_ART_SEP_GAP.min(slack);
            if separator_y < area.bottom() {
                render_art_animation_separator(
                    frame,
                    app,
                    Rect {
                        x: area.x,
                        y: separator_y,
                        width: area.width,
                        height: 1,
                    },
                );
            }
            if !app.lyrics.visible {
                // Music-mode filler animations run in radio mode too: the canvas gets
                // the blank band below the one-line art, mirroring the album-art arm
                // of `render_filler`.
                let below = separator_y.saturating_add(ART_LYRICS_GAP);
                if below < area.bottom() {
                    crate::ui::anim::render_canvas(
                        frame,
                        app,
                        Rect {
                            x: area.x,
                            y: below,
                            width: area.width,
                            height: area.bottom() - below,
                        },
                    );
                }
                return;
            }
            let lyrics_y = separator_y.saturating_add(ART_LYRICS_GAP);
            let lyrics_area = Rect {
                x: area.x,
                y: lyrics_y,
                width: area.width,
                height: area.bottom().saturating_sub(lyrics_y),
            };
            render_lyrics(frame, app, lyrics_area);
        }
        None if app.lyrics.visible => render_lyrics(frame, app, area),
        // Too small for the set piece and no lyrics: the whole filler is canvas, like
        // the music-mode no-art arm.
        None => crate::ui::anim::render_canvas(frame, app, area),
    }
}

fn draw_radio_ascii(frame: &mut Frame, app: &App, area: Rect) -> Option<Rect> {
    if area.width < 12 || area.height < 3 {
        return None;
    }
    const LARGE: &[&str] = &[
        "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⢀⠀⠀⠀⠀⠀⠀⠀⠀",
        "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⢰⡟⠷⣶⣤⠀⠀⠀⠀⠀",
        "⠀⠀⠀⢸⠟⠛⠃⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⣠⣼⡇⢀⣸⡇⠀⠀⠀⠀⠀",
        "⠀⠀⠀⠘⣧⡀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠸⠿⠿⠻⣿⣿⡿⠀⠀⠀⠀⠀",
        "⠀⠀⠀⣾⣿⡿⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
        "⠀⠀⠀⠈⠉⠀⠀⠀⠀⣿⠛⠛⠛⠛⠛⠛⠛⠛⠛⠛⣿⠀⠀⠀⠀⠀⠀⠀⠀⠀",
        "⠀⢀⣀⣀⣀⣀⣀⣀⣀⣉⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣉⣀⣀⣀⣀⣀⣀⣀⡀⠀",
        "⠀⢸⣿⣿⣉⣉⣉⣹⣿⣿⣏⣉⣉⣉⣉⣉⣉⣉⣉⣹⣿⣿⣏⣉⣉⣉⣿⣿⡇⠀",
        "⠀⢸⣿⣉⣉⣉⣉⣉⣉⣉⣉⣉⣉⣉⣉⣉⣉⣉⣉⣉⣉⣉⣉⣉⣉⣉⣉⣿⡇⠀",
        "⠀⢸⣿⡿⠋⣉⣭⣍⡙⢿⣿⡏⢉⣭⣭⣭⣭⡉⢹⣿⡿⢋⣩⣭⣉⠙⢿⣿⡇⠀",
        "⠀⢸⡟⢠⣾⠟⠛⠻⣿⣆⠹⡇⢸⣿⣿⣿⣿⡇⢸⠏⣰⣿⠟⠛⠻⣷⡄⢻⡇⠀",
        "⠀⢸⡇⢸⣿⡀⠛⢀⣿⡿⢀⣧⣤⣤⣤⣤⣤⣤⣼⡀⢿⣿⡀⠛⢀⣿⡇⢸⡇⠀",
        "⠀⢸⣿⣄⠙⠿⣿⡿⠟⣡⣾⣿⡏⢹⡏⢹⡏⢹⣿⣷⣌⠻⢿⣿⠿⠋⣠⣿⡇⠀",
        "⠀⠸⠿⠿⠷⠶⠶⠶⠾⠿⠿⠿⠿⠾⠿⠿⠷⠿⠿⠿⠿⠷⠶⠶⠶⠾⠿⠿⠇⠀",
        "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
    ];
    const SMALL: &[&str] = &[
        "⠀⢀⣀⣀⣀⣀⣀⣀⣀⣉⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣉⣀⣀⣀⣀⣀⣀⣀⡀⠀",
        "⠀⢸⣿⣿⣉⣉⣉⣹⣿⣿⣏⣉⣉⣉⣉⣉⣉⣉⣉⣹⣿⣿⣏⣉⣉⣉⣿⣿⡇⠀",
        "⠀⢸⣿⣉⣉⣉⣉⣉⣉⣉⣉⣉⣉⣉⣉⣉⣉⣉⣉⣉⣉⣉⣉⣉⣉⣉⣉⣿⡇⠀",
        "⠀⢸⣿⡿⠋⣉⣭⣍⡙⢿⣿⡏⢉⣭⣭⣭⣭⡉⢹⣿⡿⢋⣩⣭⣉⠙⢿⣿⡇⠀",
        "⠀⢸⡟⢠⣾⠟⠛⠻⣿⣆⠹⡇⢸⣿⣿⣿⣿⡇⢸⠏⣰⣿⠟⠛⠻⣷⡄⢻⡇⠀",
        "⠀⢸⡇⢸⣿⡀⠛⢀⣿⡿⢀⣧⣤⣤⣤⣤⣤⣤⣼⡀⢿⣿⡀⠛⢀⣿⡇⢸⡇⠀",
        "⠀⢸⣿⣄⠙⠿⣿⡿⠟⣡⣾⣿⡏⢹⡏⢹⡏⢹⣿⣷⣌⠻⢿⣿⠿⠋⣠⣿⡇⠀",
        "⠀⠸⠿⠿⠷⠶⠶⠶⠾⠿⠿⠿⠿⠾⠿⠿⠷⠿⠿⠿⠿⠷⠶⠶⠶⠾⠿⠿⠇⠀",
    ];
    let art = if area.width >= 34 && area.height >= LARGE.len() as u16 {
        LARGE
    } else {
        SMALL
    };
    let height = (art.len() as u16).min(area.height);
    if height == 0 {
        return None;
    }
    let y = area.y + area.height.saturating_sub(height) / 2;
    let rect = Rect {
        x: area.x,
        y,
        width: area.width,
        height,
    };
    let style = app.theme.style(R::Accent).add_modifier(Modifier::BOLD);
    let rendered = radio_art_lines(art, height as usize, radio_art_animation_on(app), app);
    let lines: Vec<Line> = rendered
        .into_iter()
        .map(|line| Line::from(line).style(style).alignment(Alignment::Center))
        .collect();
    frame.render_widget(Paragraph::new(lines), rect);
    Some(rect)
}

fn radio_art_lines(art: &[&str], height: usize, animated: bool, app: &App) -> Vec<String> {
    let mut lines: Vec<String> = art
        .iter()
        .take(height)
        .map(|line| (*line).to_owned())
        .collect();
    if !animated || lines.is_empty() {
        return lines;
    }

    let slow = app.anim_frame() / 2;
    let width = art
        .iter()
        .map(|line| UnicodeWidthStr::width(*line))
        .max()
        .unwrap_or(0);
    let note_sway = [-1, 0, 1, 1, 0, -1, -1, 0][((slow / 8) as usize) % 8];
    let body_sway = [0, 1, 0, -1][((slow / 12) as usize) % 4];

    for (i, line) in lines.iter_mut().enumerate() {
        if art.len() > 8 && i == 5 {
            *line = compose_radio_handle_line(line, note_sway, body_sway, width);
            continue;
        }
        let delta = if art.len() > 8 && i < 6 {
            note_sway
        } else if i >= 3 {
            body_sway
        } else {
            0
        };
        if delta != 0 {
            *line = shift_display_line(line, delta, width);
        }
    }

    if ((slow / 10) % 2) == 1 {
        for line in &mut lines {
            *line = line.replace("⣿⣿⣿⣿", "⣿⣿⣶⣿");
        }
    }
    lines
}

fn compose_radio_handle_line(line: &str, note_sway: i32, body_sway: i32, width: usize) -> String {
    let Some(handle_start_byte) = line.find('⣿') else {
        return shift_display_line(line, body_sway, width);
    };
    let (note, handle) = line.split_at(handle_start_byte);
    let handle_start = UnicodeWidthStr::width(note);
    let mut cells = vec!['⠀'; width];
    overlay_radio_segment(&mut cells, 0, note, note_sway);
    overlay_radio_segment(&mut cells, handle_start, handle, body_sway);
    cells.into_iter().collect()
}

fn overlay_radio_segment(cells: &mut [char], start: usize, segment: &str, delta: i32) {
    let mut col = 0i32;
    let start = start as i32 + delta;
    for ch in segment.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0).max(1);
        let target = start + col;
        if ch != '⠀'
            && ch != ' '
            && target >= 0
            && let Some(cell) = cells.get_mut(target as usize)
        {
            *cell = ch;
        }
        col += ch_width as i32;
    }
}

fn shift_display_line(line: &str, delta: i32, width: usize) -> String {
    if delta > 0 {
        let shifted = format!("{}{}", "⠀".repeat(delta as usize), line);
        return crate::ui::text::pad_to_width(
            &crate::ui::text::truncate_owned_to_width(shifted, width),
            width,
        );
    }

    let mut skipped = 0usize;
    let mut out = String::new();
    for ch in line.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if skipped < delta.unsigned_abs() as usize {
            skipped = skipped.saturating_add(ch_width);
            continue;
        }
        out.push(ch);
    }
    crate::ui::text::pad_to_width(&crate::ui::text::truncate_owned_to_width(out, width), width)
}

/// The top slice of `area` the art may occupy: skip `gap` rows under the status line, then
/// cap the height to `max_h` (clamped to whatever space is left).
fn art_band(area: Rect, gap: u16, max_h: u16) -> Rect {
    let avail = area.height.saturating_sub(gap);
    Rect {
        x: area.x,
        y: area.y + gap,
        width: area.width,
        height: max_h.min(avail),
    }
}

/// Draw the current track's album art / thumbnail top-anchored within `band` and centered
/// horizontally by its true aspect ratio (square covers stay square, 16:9 thumbnails stay
/// wide — the picker knows the terminal's font cell size). Returns the rect the art
/// actually occupies so the caller can start the lyrics right below it, or `None` when the
/// band is too small to render anything legible. Only called when `app.art_active()`, so
/// the protocol is present; with no graphics protocol this renders as unicode half-blocks.
/// `Resize::Scale` lets a small source thumbnail grow to fill the rect (the default `Fit`
/// never upscales).
fn draw_art(frame: &mut Frame, app: &App, band: Rect) -> Option<Rect> {
    // Below a few cells there's nothing but mush; skip so a tiny terminal stays clean.
    if band.width < 6 || band.height < 3 {
        return None;
    }
    let mut rect = app.art_fit_rect(band);
    rect.y = band.y; // art_fit_rect centers vertically; re-anchor to the top of the band.
    app.art.rect.set(Some(rect));
    // Retro mode draws the cover itself as luminance-ramp ASCII art: a basic console has no
    // graphics protocol, and the half-block fallback needs truecolor cells the scrubber
    // would flatten into `#` mush anyway.
    if app.retro_mode() {
        if let Some((id, img)) = app.art_source_image() {
            crate::ui::ascii_art::render_image(
                frame,
                crate::ui::ascii_art::Slot::AlbumArt,
                id,
                img,
                rect,
            );
        }
        return Some(rect);
    }
    if let Some(proto) = app.art.protocol.borrow_mut().as_mut() {
        frame.render_stateful_widget(
            StatefulImage::new().resize(Resize::Scale(Some(FilterType::Lanczos3))),
            rect,
            proto,
        );
    }
    Some(rect)
}

/// The synced-lyrics panel: a window of lines centered on the current one, which is
/// highlighted. Auto-scrolls as `time-pos` advances.
fn render_lyrics(frame: &mut Frame, app: &App, area: Rect) {
    let centered = |s: &str, style: Style| {
        Line::from(s.to_owned())
            .style(style)
            .alignment(Alignment::Center)
    };
    let dim = app.theme.style(R::LyricsDim);

    let lines = match &app.lyrics.track {
        Some(t) if !t.lines.is_empty() => &t.lines,
        _ => {
            let base = if app.lyrics.loading {
                t!("Searching lyrics", "가사 검색 중")
            } else if app.lyrics.track.is_some() {
                t!("No synced lyrics found.", "동기화된 가사가 없어요.")
            } else {
                t!("Fetching lyrics", "가사 가져오는 중")
            };
            // In-flight messages carry animated dots when the activity flag is on; the
            // static ellipsis otherwise (and always for the terminal "not found" state).
            let msg = if app.lyrics.track.is_some() && !app.lyrics.loading {
                base.to_owned()
            } else if let Some(dots) = crate::ui::anim::activity_dots(app) {
                format!("{base}{dots}")
            } else {
                format!("{base}…")
            };
            frame.render_widget(Paragraph::new(centered(&msg, dim)), area);
            return;
        }
    };

    let height = area.height as usize;
    if height == 0 {
        return;
    }
    let pos = app.playback.time_pos.unwrap_or(0.0);
    let cur = lyrics::current_index(lines, pos);
    // Keep the current line vertically centered.
    let start = cur.unwrap_or(0).saturating_sub(height / 2);

    // With the lyrics animation on, the current line breathes toward the accent (flashing as
    // it first becomes current) and far lines fade slightly with distance; identity when off.
    let current_style = crate::ui::anim::lyrics_current_style(
        app,
        app.theme
            .style(R::LyricsCurrent)
            .add_modifier(Modifier::BOLD),
    );
    let rendered: Vec<Line> = lines
        .iter()
        .enumerate()
        .skip(start)
        .take(height)
        .map(|(i, l)| {
            let style = if Some(i) == cur {
                current_style
            } else {
                // No current line yet (intro silence) → no distance fade, plain dim.
                let distance = cur.map_or(0, |c| c.abs_diff(i));
                crate::ui::anim::lyrics_dim_style(app, dim, distance)
            };
            centered(&l.text, style)
        })
        .collect();
    frame.render_widget(Paragraph::new(rendered), area);
}

/// A modal confirmation for entering/leaving dedicated Radio mode. It intentionally mirrors the
/// Settings reset confirmation shape so broad UI-mode switches use the same interaction pattern.
pub fn render_radio_mode_confirm(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    confirm: RadioModeConfirm,
) {
    let popup = centered_fixed(area, 60, 9);
    crate::ui::render_popup_background(frame, app, popup);

    let block = Block::default()
        .title(confirm.title())
        .borders(Borders::ALL)
        .border_style(crate::ui::confirm_border_style(app))
        .style(crate::ui::popup_style(app, R::TextPrimary));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
    ])
    .split(inner);
    frame.render_widget(
        Paragraph::new(confirm.prompt())
            .alignment(Alignment::Center)
            .style(crate::ui::popup_style(app, R::TextPrimary)),
        rows[1],
    );
    frame.render_widget(
        Paragraph::new(confirm.detail())
            .alignment(Alignment::Center)
            .style(crate::ui::popup_style(app, R::TextMuted)),
        rows[2],
    );

    let segs = [
        buttons::Seg::button(
            MouseTarget::ConfirmRadioMode,
            t!(" Confirm (Enter) ", " 확인 (Enter) "),
        ),
        buttons::Seg::label("    "),
        buttons::Seg::button(
            MouseTarget::CancelRadioMode,
            t!(" Cancel (Esc) ", " 취소 (Esc) "),
        ),
    ];
    buttons::render_segments(
        frame,
        app,
        rows[4],
        &segs,
        crate::ui::confirm_button_style(app),
        crate::ui::confirm_gap_style(app),
        Alignment::Center,
    );
    crate::ui::seal_popup_background(frame, app, popup);
    crate::ui::mark_art_rows_for_popup(frame, app, popup);
}

/// A `w`×`h` rect centered in `area`, clamped so it never exceeds the available space.
fn centered_fixed(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect {
        x: area.x + area.width.saturating_sub(w) / 2,
        y: area.y + area.height.saturating_sub(h) / 2,
        width: w,
        height: h,
    }
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
        let parts = status_line_parts_at(&app, "    ", false);
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
                    let total: usize = status_line_parts_at(&app, "    ", false)
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
        let parts = status_line_parts_at(&app, "    ", false);
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
        let parts = status_line_parts_at(&app, " ", true);
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
            let parts = status_line_parts_at(&app, " ", minimal);
            assert!(has_target(&parts, |t| matches!(
                t,
                MouseTarget::Player(Action::IdentifyNowPlaying)
            )));
        }
        // DJ Gem up changes nothing on the radio path — still offered.
        app.ai.available = true;
        let parts = status_line_parts_at(&app, "    ", false);
        assert!(has_target(&parts, |t| matches!(
            t,
            MouseTarget::Player(Action::IdentifyNowPlaying)
        )));
        // Music mode never offers it, DJ Gem or not.
        let mut app = App::new(100);
        app.ai.available = true;
        app.queue.set(vec![Song::remote("a", "A", "x", "1:00")], 0);
        let parts = status_line_parts_at(&app, "    ", false);
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
                let total: usize = status_line_parts_at(&app, "    ", false)
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
        let parts = status_line_parts_at(&app, "    ", false);
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
