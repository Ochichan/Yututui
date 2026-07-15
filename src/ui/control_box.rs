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

mod beginner;
use beginner::{StatusLabelTier, control_label as beginner_control_label, pad_display_state};
mod status_glyphs;
use status_glyphs::*;

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
    render_title_row(frame, app, title, true, false);
    render_seekbar(frame, app, seek, true);
    render_controls(frame, app, controls, true);
    render_status_line(frame, app, status, true);

    // Heart burst around the title when the current track is liked. Skipped while a status
    // message covers the title (nothing to celebrate over) — drawn after the neighbours so the
    // sparks sit on top of the blank gap rows.
    if app.status.text.is_empty()
        && crate::ui::anim::like_burst_active(app, title.width)
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
    render_title_row(frame, app, rows[1], animated, false);
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
///
/// `marquee` opts the plain fallback into `selected_marquee` on overflow. Only the mini
/// tier may pass true: it replaces the whole UI, so no other marquee surface (list cursor
/// rows, the radio card) can render in the same frame and fight over the single-slot
/// `marquee_key` — two same-frame callers would keep resetting each other's origin and
/// freeze both crawls while pinning the animation clock awake.
pub(in crate::ui) fn render_title_row(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    animated: bool,
    marquee: bool,
) {
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
                // Scroll an overflowing title so it stays readable in the mini player.
                // `selected_marquee` is deliberately independent of the animation masters
                // (like the radio card's title) and returns the text unchanged when it
                // already fits, so the fits case stays byte-identical.
                let text = format!("{heart}{title} — {artist}");
                if marquee {
                    crate::ui::anim::selected_marquee(
                        app,
                        crate::app::ScrollSurface::PlayerTitle,
                        0,
                        &text,
                        usize::from(area.width),
                    )
                } else {
                    text
                }
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
        let preview = app.seekbar_preview_target();
        let shown_position = preview.or(app.playback.time_pos);
        let raw = format::seekbar_ratio(shown_position, app.playback.duration);
        let ratio = if animated && preview.is_none() {
            crate::ui::anim::smooth_seek_ratio(app, raw)
        } else {
            raw
        };
        (
            ratio,
            format::seekbar_label(shown_position, app.playback.duration),
        )
    };
    // With the time-glow animation on, the gauge and its label pulse toward the accent for
    // a beat as each playback second lands (identity style/plain label otherwise).
    let gauge_style = Style::default()
        .fg(app.theme.color(R::GaugeFilled))
        .bg(app.theme.color(R::GaugeEmpty));
    let gauge_style = if animated {
        crate::ui::anim::time_glow_gauge_style(app, gauge_style)
    } else {
        gauge_style
    };
    let label = if animated {
        crate::ui::anim::time_glow_label(app, label)
    } else {
        ratatui::text::Span::raw(label)
    };
    let seekbar = Gauge::default()
        .gauge_style(gauge_style)
        .ratio(ratio)
        .label(label);
    frame.render_widget(seekbar, area);
    // A bright comet sweeps the filled portion when the seekbar animation is on (no-op
    // otherwise), a short ripple marks the head right after a seek, and sparks dance on
    // the playhead while the sparkle flag is on. Skipped in the static (off-Player)
    // forms — a frozen comet is a permanent bright smear.
    if animated {
        crate::ui::anim::seekbar_overlay(frame, app, area, ratio);
        crate::ui::anim::seek_flash_overlay(frame, app, area, ratio);
        crate::ui::anim::progress_sparkle_overlay(frame, app, area, ratio);
    }
    // Publish the seekbar's screen rect so a mouse click can be hit-tested for seeking.
    app.hits.set_seekbar_rect(area);
}

/// The transport status line: state, queue position, rating, shuffle, repeat, speed, EQ, etc.
///
/// Rendered as click segments rather than one string so repeat and
/// `eq:` (opens the preset dropdown) are mouse targets — but every segment shares the same
/// cyan style, so the line looks exactly like the plain status text it replaced. `eq:` is
/// always shown now (so the dropdown is always reachable); the rest stay conditional.
pub(in crate::ui) fn render_status_line(frame: &mut Frame, app: &App, area: Rect, animated: bool) {
    // Roomy separators normally; progressively tighter when the line wouldn't fit (narrow
    // terminals, or text zoom shrinking the virtual grid). Content always wins over air —
    // the `eq:` / repeat toggles at the tail must stay visible and clickable.
    let (gap_w, parts) = fitted_status_line_parts(app, area.width, animated);
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
    let label_style = app.theme.style(R::PlayerLabel);
    let button_style = if app.beginner_labels_enabled() {
        app.theme.style(R::Accent).add_modifier(Modifier::BOLD)
    } else {
        // Beginner mode off preserves the legacy status-line styling exactly.
        label_style
    };
    buttons::render_segments(
        frame,
        app,
        area,
        &segments,
        button_style,
        label_style,
        Alignment::Center,
    );
}

type StatusLinePart = (Option<MouseTarget>, Cow<'static, str>);
type StatusLineParts = Vec<StatusLinePart>;

fn fitted_status_line_parts(app: &App, width: u16, animated: bool) -> (u16, StatusLineParts) {
    let fits = |parts: &[StatusLinePart]| {
        parts
            .iter()
            .map(|(_, text)| buttons::text_width(text))
            .sum::<u16>()
            <= width
    };
    if app.beginner_labels_enabled() {
        // Shed keycaps and decoration before names, then use compact labels only when necessary.
        let mut parts = StatusLineParts::with_capacity(16);
        for (gap, minimal, labels) in [
            ("    ", false, StatusLabelTier::BeginnerKeys),
            ("  ", false, StatusLabelTier::BeginnerKeys),
            (" ", false, StatusLabelTier::BeginnerKeys),
            (" ", true, StatusLabelTier::BeginnerKeys),
            ("  ", false, StatusLabelTier::BeginnerNames),
            (" ", false, StatusLabelTier::BeginnerNames),
            (" ", true, StatusLabelTier::BeginnerNames),
            (" ", true, StatusLabelTier::Compact),
        ] {
            parts =
                status_line_parts_with_labels_reusing(app, gap, minimal, animated, labels, parts);
            if fits(&parts) {
                return (buttons::text_width(gap), parts);
            }
        }
        (1, parts)
    } else {
        let mut parts = StatusLineParts::with_capacity(16);
        for (gap, minimal) in [("    ", false), ("  ", false), (" ", false), (" ", true)] {
            parts = status_line_parts_with_labels_reusing(
                app,
                gap,
                minimal,
                animated,
                StatusLabelTier::Compact,
                parts,
            );
            if fits(&parts) {
                return (buttons::text_width(gap), parts);
            }
        }
        (1, parts)
    }
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
#[cfg(test)]
fn status_line_parts(
    app: &App,
    gap: &'static str,
    minimal: bool,
    animated: bool,
) -> StatusLineParts {
    status_line_parts_with_labels(app, gap, minimal, animated, StatusLabelTier::Compact)
}
#[cfg(test)]
fn status_line_parts_with_labels(
    app: &App,
    gap: &'static str,
    minimal: bool,
    animated: bool,
    labels: StatusLabelTier,
) -> StatusLineParts {
    status_line_parts_with_labels_reusing(
        app,
        gap,
        minimal,
        animated,
        labels,
        StatusLineParts::with_capacity(16),
    )
}

fn status_line_parts_with_labels_reusing(
    app: &App,
    gap: &'static str,
    minimal: bool,
    animated: bool,
    labels: StatusLabelTier,
    mut parts: StatusLineParts,
) -> StatusLineParts {
    let retro = app.retro_mode();
    parts.clear();
    // A braille throbber leads the line when the spinner animation is on (no-op otherwise). It's a
    // plain label, so `render_segments` keeps every later hit rect aligned to its rendered text.
    if (!minimal || !labels.beginner())
        && animated
        && let Some(spin) = crate::ui::anim::spinner_prefix(app)
    {
        parts.push((None, Cow::Owned(format!("{spin} "))));
    }
    // EAW-neutral glyphs (one cell everywhere) — the ⏸/▶ media emoji widen to two
    // cells on some terminals (Windows), which drifts every later segment's hit rect
    // off its rendered text and makes `R:`/`eq:` unclickable. See `render_controls`.
    if !(minimal && labels.beginner()) {
        let state = match (minimal, app.playback.paused, labels.beginner() && retro) {
            (true, true, _) => "‖",
            (true, false, _) => "▸",
            (false, true, true) => "‖ paused",
            (false, false, true) => "▸ playing",
            (false, true, false) => t!("‖ paused", "‖ 일시정지"),
            (false, false, false) => t!("▸ playing", "▸ 재생 중"),
        };
        parts.push((None, Cow::Borrowed(state)));
    }
    if !app.queue.is_empty() {
        let (pos, _) = app.queue.position();
        // Clickable (opens the queue window) but styled exactly like the labels around it,
        // so the line looks unchanged — same as the shuffle/repeat toggles next to it.
        parts.push((None, Cow::Borrowed(gap)));
        let value = format!("{pos}/{}", app.queue.len());
        let text = if labels.beginner() {
            beginner_control_label(
                app,
                labels,
                KeyContext::Player,
                Action::OpenQueue,
                if retro {
                    "Queue"
                } else {
                    t!("Queue", "대기열")
                },
                Some(value),
            )
        } else {
            value
        };
        parts.push((Some(MouseTarget::QueuePos), Cow::Owned(text)));
    }
    // The current track's rating: one tri-state glyph (🤔/👍/👎) that replaces the old separate
    // ♥ favorite + ✗ dislike columns. Same `PlayerLabel` style and 4-space gap as the toggles next
    // to it; click and the `f` key hit the same Action, so it mirrors the keyboard exactly.
    if let Some(cur) = app.queue.current() {
        let liked = app.library.is_favorite(&cur.video_id);
        let disliked = app.signals.is_disliked(&cur.video_id);
        parts.push((None, Cow::Borrowed(gap)));
        let text = if labels.beginner() {
            let (state, states) = if retro {
                (
                    if liked {
                        "Like"
                    } else if disliked {
                        "Dislike"
                    } else {
                        "None"
                    },
                    ["Like", "Dislike", "None"],
                )
            } else {
                (
                    if liked {
                        t!("Like", "좋아요")
                    } else if disliked {
                        t!("Dislike", "싫어요")
                    } else {
                        t!("None", "없음")
                    },
                    [
                        t!("Like", "좋아요"),
                        t!("Dislike", "싫어요"),
                        t!("None", "없음"),
                    ],
                )
            };
            beginner_control_label(
                app,
                labels,
                KeyContext::Player,
                Action::CycleRating,
                if retro {
                    "Rating"
                } else {
                    t!("Rating", "평가")
                },
                Some(pad_display_state(state, &states)),
            )
        } else {
            rating_glyph(liked, disliked, retro).to_owned()
        };
        parts.push((
            Some(MouseTarget::Player(Action::CycleRating)),
            Cow::Owned(text),
        ));
    }
    // Shuffle and repeat are both always shown as click toggles, and every state of each keeps
    // one display width, so the line's layout never shifts as they toggle or appear. Each
    // carries its media glyph — `<key>:🔀` shuffle, `<key>:🔁`/`<key>:🔂` repeat — or a padded cross when
    // off, so they read the same in every UI language.
    // On a live radio stream the same two slots (and the same actions/keys behind them)
    // read as the live transport instead: `LIVE:` sync verdict, `SYNC:` re-sync.
    if app.current_is_radio_stream() {
        parts.push((None, Cow::Borrowed(gap)));
        let live = if labels.beginner() {
            let synced = app.radio_live_synced();
            let (state, states) = if retro {
                (
                    match synced {
                        Some(true) => "Synced",
                        Some(false) => "Behind",
                        None => "Unknown",
                    },
                    ["Synced", "Behind", "Unknown"],
                )
            } else {
                (
                    match synced {
                        Some(true) => t!("Synced", "동기화"),
                        Some(false) => t!("Behind", "뒤처짐"),
                        None => t!("Unknown", "알 수 없음"),
                    },
                    [
                        t!("Synced", "동기화"),
                        t!("Behind", "뒤처짐"),
                        t!("Unknown", "알 수 없음"),
                    ],
                )
            };
            beginner_control_label(
                app,
                labels,
                KeyContext::Player,
                Action::ToggleShuffle,
                if retro {
                    "Live"
                } else {
                    t!("Live", "라이브")
                },
                Some(pad_display_state(state, &states)),
            )
        } else {
            format!("LIVE:{}", live_sync_glyph(app.radio_live_synced(), retro))
        };
        parts.push((
            Some(MouseTarget::Player(Action::ToggleShuffle)),
            Cow::Owned(live),
        ));
        parts.push((None, Cow::Borrowed(gap)));
        let resync = if labels.beginner() {
            beginner_control_label(
                app,
                labels,
                KeyContext::Player,
                Action::CycleRepeat,
                if retro {
                    "Resync"
                } else {
                    t!("Resync", "다시 동기화")
                },
                None,
            )
        } else {
            format!("SYNC:{}", resync_glyph(retro))
        };
        parts.push((
            Some(MouseTarget::Player(Action::CycleRepeat)),
            Cow::Owned(resync),
        ));
        // The "what's playing" card — now ICY-metadata-backed, so always offered on a live
        // radio stream, DJ Gem or not. A text label, not a glyph: EAW-ambiguous symbols
        // (♪ …) render wide on some CJK terminals and would drift every later hit rect.
        parts.push((None, Cow::Borrowed(gap)));
        let identify = if labels.beginner() {
            beginner_control_label(
                app,
                labels,
                KeyContext::Player,
                Action::IdentifyNowPlaying,
                if retro {
                    "Identify"
                } else {
                    t!("Identify", "곡 찾기")
                },
                None,
            )
        } else {
            t!("ID?", "지듣노").to_owned()
        };
        parts.push((
            Some(MouseTarget::Player(Action::IdentifyNowPlaying)),
            Cow::Owned(identify),
        ));
        // While the radio recorder is writing a track, a click-to-open `REC` chip. A plain
        // text label (not a glyph) for the same EAW-neutral reason as `ID?` above — an
        // ambiguous-width mark would drift later hit rects on some CJK terminals.
        if app.recorder.is_recording() {
            parts.push((None, Cow::Borrowed(gap)));
            let recordings = if labels.beginner() {
                beginner_control_label(
                    app,
                    labels,
                    KeyContext::Player,
                    Action::ToggleRecordings,
                    if retro {
                        "Recordings"
                    } else {
                        t!("Recordings", "녹음 목록")
                    },
                    None,
                )
            } else {
                t!("REC", "녹음").to_owned()
            };
            parts.push((
                Some(MouseTarget::Player(Action::ToggleRecordings)),
                Cow::Owned(recordings),
            ));
        }
    } else {
        parts.push((None, Cow::Borrowed(gap)));
        let shuffle = if labels.beginner() {
            let (state, states) = if retro {
                (if app.queue.shuffle { "On" } else { "Off" }, ["On", "Off"])
            } else {
                (
                    if app.queue.shuffle {
                        t!("On", "켬")
                    } else {
                        t!("Off", "끔")
                    },
                    [t!("On", "켬"), t!("Off", "끔")],
                )
            };
            beginner_control_label(
                app,
                labels,
                KeyContext::Player,
                Action::ToggleShuffle,
                if retro {
                    "Shuffle"
                } else {
                    t!("Shuffle", "셔플")
                },
                Some(pad_display_state(state, &states)),
            )
        } else {
            format!(
                "{}:{}",
                player_action_key_label(app, Action::ToggleShuffle, retro),
                shuffle_glyph(app.queue.shuffle, retro)
            )
        };
        parts.push((
            Some(MouseTarget::Player(Action::ToggleShuffle)),
            Cow::Owned(shuffle),
        ));
        parts.push((None, Cow::Borrowed(gap)));
        let repeat = if labels.beginner() {
            let (state, states) = if retro {
                (
                    match app.queue.repeat {
                        Repeat::Off => "Off",
                        Repeat::All => "All",
                        Repeat::One => "One",
                    },
                    ["Off", "All", "One"],
                )
            } else {
                (
                    match app.queue.repeat {
                        Repeat::Off => t!("Off", "끔"),
                        Repeat::All => t!("All", "전체"),
                        Repeat::One => t!("One", "한 곡"),
                    },
                    [t!("Off", "끔"), t!("All", "전체"), t!("One", "한 곡")],
                )
            };
            beginner_control_label(
                app,
                labels,
                KeyContext::Player,
                Action::CycleRepeat,
                if retro {
                    "Repeat"
                } else {
                    t!("Repeat", "반복")
                },
                Some(pad_display_state(state, &states)),
            )
        } else {
            format!(
                "{}:{}",
                player_action_key_label(app, Action::CycleRepeat, retro),
                repeat_glyph(app.queue.repeat, retro)
            )
        };
        parts.push((
            Some(MouseTarget::Player(Action::CycleRepeat)),
            Cow::Owned(repeat),
        ));
    }
    if !minimal && (app.playback.speed - 1.0).abs() > f64::EPSILON {
        parts.push((None, Cow::Owned(format!("{gap}{:.1}x", app.playback.speed))));
    }
    parts.push((None, Cow::Borrowed(gap)));
    let eq = if labels.beginner() {
        beginner_control_label(
            app,
            labels,
            KeyContext::Player,
            Action::CycleEq,
            if retro {
                "Equalizer"
            } else {
                t!("Equalizer", "이퀄라이저")
            },
            Some(pad_display_state(
                app.audio.preset.label(),
                &["Flat", "Bass", "Treble", "Vocal", "Rock", "Jazz", "Custom"],
            )),
        )
    } else {
        format!("eq:{}", app.audio.preset.label())
    };
    parts.push((Some(MouseTarget::EqMenu), Cow::Owned(eq)));
    // Faux VU bars trail the EQ label when the EQ-bars animation is on (no-op otherwise).
    if !minimal
        && animated
        && let Some(mut bars) = crate::ui::anim::eq_bars(app)
    {
        // `eq_bars` already owns this frame's five-glyph buffer. Prefix the separator in
        // place instead of formatting it into a second allocation and immediately dropping
        // the original. This keeps the single label segment (and therefore its exact cells)
        // unchanged.
        bars.insert_str(0, gap);
        parts.push((None, Cow::Owned(bars)));
    }
    if app.streaming_active() {
        // Show the station's mode (Focused/Balanced/Discovery) as a click target that opens the
        // mode dropdown — same affordance as the `eq:` label next to it.
        parts.push((None, Cow::Borrowed(gap)));
        let streaming = if labels.beginner() {
            let states = if retro {
                ["Focused", "Balanced", "Discovery"]
            } else {
                crate::streaming::StreamingMode::CYCLE.map(|mode| mode.label())
            };
            let mode = if retro {
                match app.config.streaming.mode {
                    crate::streaming::StreamingMode::Focused => "Focused",
                    crate::streaming::StreamingMode::Balanced => "Balanced",
                    crate::streaming::StreamingMode::Discovery => "Discovery",
                }
            } else {
                app.config.streaming.mode.label()
            };
            beginner_control_label(
                app,
                labels,
                KeyContext::Global,
                Action::ToggleStreaming,
                if retro {
                    "Streaming autoplay"
                } else {
                    t!("Streaming autoplay", "스트리밍 자동재생")
                },
                Some(format!(
                    "{} · {}",
                    if retro { "On" } else { t!("On", "켬") },
                    pad_display_state(mode, &states)
                )),
            )
        } else {
            format!(
                "streaming:{}",
                app.config.streaming.mode.label().to_lowercase()
            )
        };
        parts.push((Some(MouseTarget::StreamingMenu), Cow::Owned(streaming)));
    } else if app.local_dedicated_mode && app.autoplay_streaming {
        // Local Deck suppresses the effective online top-up without rewriting the user's normal
        // preference. Keep that distinction visible instead of making the control disappear and
        // leaving a saved `On` setting looking like it was lost.
        parts.push((None, Cow::Borrowed(gap)));
        let streaming = if labels.beginner() {
            beginner_control_label(
                app,
                labels,
                KeyContext::Global,
                Action::ToggleStreaming,
                if retro {
                    "Streaming autoplay"
                } else {
                    t!("Streaming autoplay", "스트리밍 자동재생")
                },
                Some(if retro {
                    "Off (saved On)".to_owned()
                } else {
                    t!("Off (saved On)", "끔 (저장: 켬)").to_owned()
                }),
            )
        } else {
            "streaming:off(saved:on)".to_owned()
        };
        parts.push((Some(MouseTarget::StreamingMenu), Cow::Owned(streaming)));
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
                let head = crate::ui::anim::download_spinner(app).unwrap_or("⬇");
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
    // text zoom shrinking the virtual grid). Beginner mode first shortens Volume to `vol`,
    // then sheds its keycaps, while the legacy path retains exactly the old segment bytes.
    let compact_volume_label = t!("vol ", "볼륨 ");
    let beginner_volume_label = if app.retro_mode() {
        "Volume "
    } else {
        t!("Volume ", "볼륨 ")
    };
    let plain_down = " - ";
    let plain_up = " + ";
    // Resolving display keycaps allocates two strings. The legacy/non-beginner path never
    // displays them, so do not build them on every animated frame just to discard them.
    let beginner_buttons = app
        .beginner_labels_enabled()
        .then(|| beginner::volume_buttons(app));
    let tight_transport_width = 13u16;
    let cluster_width = |label: &str, down: &str, up: &str| {
        buttons::text_width(label)
            .saturating_add(buttons::text_width(down))
            .saturating_add(buttons::text_width(&vol))
            .saturating_add(buttons::text_width(up))
    };
    let fits_tight = |label: &str, down: &str, up: &str| {
        tight_transport_width.saturating_add(cluster_width(label, down, up)) <= area.width
    };
    let (volume_label, volume_down, volume_up) = match beginner_buttons.as_ref() {
        Some((down, up)) if fits_tight(beginner_volume_label, down, up) => {
            (beginner_volume_label, down.as_str(), up.as_str())
        }
        Some((down, up)) if fits_tight(compact_volume_label, down, up) => {
            (compact_volume_label, down.as_str(), up.as_str())
        }
        _ => (compact_volume_label, plain_down, plain_up),
    };
    let full = 21u16.saturating_add(cluster_width(volume_label, volume_down, volume_up));
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
        Seg::label(volume_label),
        Seg::button(MouseTarget::Player(Action::VolDown), volume_down),
        Seg::label(&vol),
        Seg::button(MouseTarget::Player(Action::VolUp), volume_up),
    ];
    let widths: [u16; 10] = std::array::from_fn(|index| buttons::text_width(segments[index].text));
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
    // And right after a play/pause toggle, a light wave washes across the row (recolour
    // only — glyphs and hit rects untouched; no-op outside its window).
    crate::ui::anim::pause_flash_overlay(frame, app, area);
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
        // rewrote enabled/disabled glyphs to unequal widths and nudged the line on every toggle.
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
        // scrub mapped both 🔁 and 🔂 to the same letter), and every retro state glyph must be a plain
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
        let _guard = crate::i18n::lock_for_test();
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
                .any(|(_, s)| s.starts_with("x:") || s.starts_with("r:")),
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

        let beginner =
            status_line_parts_with_labels(&app, " ", true, false, StatusLabelTier::BeginnerNames);
        assert!(
            text_for(&beginner, &MouseTarget::Player(Action::ToggleShuffle)).starts_with("Live: ")
        );
        assert_eq!(
            text_for(&beginner, &MouseTarget::Player(Action::CycleRepeat)),
            "Resync"
        );
        assert_eq!(
            text_for(&beginner, &MouseTarget::Player(Action::IdentifyNowPlaying)),
            "Identify"
        );
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
            // in radio mode is the only width change (LIVE:/SYNC: vs key labels).
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

    fn text_for<'a>(parts: &'a [StatusLinePart], target: &MouseTarget) -> &'a str {
        parts
            .iter()
            .find(|(candidate, _)| candidate.as_ref() == Some(target))
            .map(|(_, text)| text.as_ref())
            .unwrap_or_else(|| panic!("missing {target:?}"))
    }

    #[test]
    fn beginner_status_labels_expand_then_fall_back_by_width() {
        let _guard = crate::i18n::lock_for_test();
        let mut app = App::new(100);
        app.config.beginner_mode = true;
        app.queue.set(vec![Song::remote("a", "A", "x", "1:00")], 0);

        let (_, roomy) = fitted_status_line_parts(&app, 100, false);
        assert_eq!(
            text_for(&roomy, &MouseTarget::Player(Action::ToggleShuffle)).trim_end(),
            "[x] Shuffle: Off"
        );
        assert_eq!(
            text_for(&roomy, &MouseTarget::Player(Action::CycleRepeat)).trim_end(),
            "[r] Repeat: Off"
        );
        assert_eq!(
            text_for(&roomy, &MouseTarget::EqMenu).trim_end(),
            "[e] Equalizer: Flat"
        );

        let (_, standard) = fitted_status_line_parts(&app, 80, false);
        let shuffle = text_for(&standard, &MouseTarget::Player(Action::ToggleShuffle));
        assert!(shuffle.starts_with("Shuffle: "), "got {shuffle:?}");
        assert!(!shuffle.contains('['), "keycaps should shed before names");

        let (_, narrow) = fitted_status_line_parts(&app, 32, false);
        assert!(text_for(&narrow, &MouseTarget::Player(Action::ToggleShuffle)).starts_with("x:"));
        assert!(text_for(&narrow, &MouseTarget::EqMenu).starts_with("eq:"));
    }

    #[test]
    fn beginner_off_uses_live_default_key_labels() {
        let _guard = crate::i18n::lock_for_test();
        let mut app = App::new(100);
        app.queue.set(vec![Song::remote("a", "A", "x", "1:00")], 0);
        let parts = status_line_parts(&app, "    ", false, false);

        assert_eq!(text_for(&parts, &MouseTarget::QueuePos), "1/1");
        assert_eq!(
            text_for(&parts, &MouseTarget::Player(Action::CycleRating)),
            "🤔"
        );
        assert_eq!(
            text_for(&parts, &MouseTarget::Player(Action::ToggleShuffle)),
            "x:✗ "
        );
        assert_eq!(
            text_for(&parts, &MouseTarget::Player(Action::CycleRepeat)),
            "r:✗ "
        );
        assert_eq!(text_for(&parts, &MouseTarget::EqMenu), "eq:Flat");
    }

    #[test]
    fn compact_status_uses_remapped_and_unbound_key_labels() {
        let mut app = App::new(100);
        app.queue.set(vec![Song::remote("a", "A", "x", "1:00")], 0);
        app.keymap
            .rebind(
                KeyContext::Player,
                Action::ToggleShuffle,
                crate::keymap::parse_chord("f8").unwrap(),
            )
            .unwrap();
        app.keymap.unbind(KeyContext::Player, Action::CycleRepeat);

        let parts = status_line_parts(&app, "    ", false, false);
        assert_eq!(
            text_for(&parts, &MouseTarget::Player(Action::ToggleShuffle)),
            "F8:✗ "
        );
        assert_eq!(
            text_for(&parts, &MouseTarget::Player(Action::CycleRepeat)),
            "—:✗ "
        );
    }

    #[test]
    fn beginner_toggle_states_keep_the_status_line_width_stable() {
        let _guard = crate::i18n::lock_for_test();
        let mut app = App::new(100);
        app.queue.set(vec![Song::remote("a", "A", "x", "1:00")], 0);
        let mut widths = Vec::new();
        for shuffle in [false, true] {
            for repeat in [Repeat::Off, Repeat::All, Repeat::One] {
                app.queue.shuffle = shuffle;
                app.queue.repeat = repeat;
                widths.push(
                    status_line_parts_with_labels(
                        &app,
                        " ",
                        true,
                        false,
                        StatusLabelTier::BeginnerNames,
                    )
                    .iter()
                    .map(|(_, text)| UnicodeWidthStr::width(text.as_ref()))
                    .sum::<usize>(),
                );
            }
        }
        assert!(widths.windows(2).all(|pair| pair[0] == pair[1]));
    }
}
