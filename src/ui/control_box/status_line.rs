//! Status-line segment assembly: builds the transport status line as `(target, text)`
//! parts, one helper per control cluster (queue position, rating, shuffle/repeat or the
//! live-radio transport, EQ, streaming mode, download tag). Split from `control_box.rs`
//! so each control's beginner/compact labelling reads on its own; the push order is the
//! render order and must not change — every later segment's hit rect depends on it.

use std::borrow::Cow;

use super::StatusLineParts;
use super::beginner::{
    StatusLabelTier, control_label as beginner_control_label, pad_display_state,
};
use super::status_glyphs::*;
use crate::app::{App, DownloadState, MouseTarget};
use crate::keymap::{Action, KeyContext};
use crate::queue::Repeat;
use crate::t;

pub(super) fn status_line_parts_with_labels_reusing(
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
    push_playback_state(&mut parts, app, labels, minimal, retro);
    push_queue_position(&mut parts, app, labels, gap, retro);
    push_rating(&mut parts, app, labels, gap, retro);
    push_why_gem(&mut parts, app, labels, gap, retro);
    // Shuffle and repeat are both always shown as click toggles, and every state of each keeps
    // one display width, so the line's layout never shifts as they toggle or appear. Each
    // carries its media glyph — `<key>:🔀` shuffle, `<key>:🔁`/`<key>:🔂` repeat — or a padded cross when
    // off, so they read the same in every UI language.
    // On a live radio stream the same two slots (and the same actions/keys behind them)
    // read as the live transport instead: `LIVE:` sync verdict, `SYNC:` re-sync.
    if app.current_is_radio_stream() {
        push_live_transport(&mut parts, app, labels, gap, retro);
    } else {
        push_shuffle_repeat(&mut parts, app, labels, gap, retro);
    }
    if !minimal && (app.playback.speed - 1.0).abs() > f64::EPSILON {
        parts.push((None, Cow::Owned(format!("{gap}{:.1}x", app.playback.speed))));
    }
    push_eq(&mut parts, app, labels, gap, retro);
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
    push_streaming_mode(&mut parts, app, labels, gap, retro);
    push_download_tag(&mut parts, app, gap, minimal);

    parts
}

/// Per-track provenance for the current queue item. The compact chip is deliberately ASCII:
/// ambiguous-width symbols would move every click target that follows it on CJK terminals.
fn push_why_gem(
    parts: &mut StatusLineParts,
    app: &App,
    labels: StatusLabelTier,
    gap: &'static str,
    retro: bool,
) {
    let Some(song) = app.queue.current() else {
        return;
    };
    if app.why_gem_for(&song.video_id).is_none() {
        return;
    }

    parts.push((None, Cow::Borrowed(gap)));
    let text = if labels.beginner() {
        beginner_control_label(
            app,
            labels,
            KeyContext::Global,
            Action::WhyAi,
            if retro {
                "Why"
            } else {
                crate::i18n::why_gem::title()
            },
            None,
        )
    } else {
        "WHY?".to_owned()
    };
    parts.push((Some(MouseTarget::Global(Action::WhyAi)), Cow::Owned(text)));
}

/// The playback state (`▸ playing` / `‖ paused`), shrunk to its one-cell glyph in the
/// minimal tier.
fn push_playback_state(
    parts: &mut StatusLineParts,
    app: &App,
    labels: StatusLabelTier,
    minimal: bool,
    retro: bool,
) {
    // EAW-neutral glyphs (one cell everywhere) — the ⏸/▶ media emoji widen to two
    // cells on some terminals (Windows), which drifts every later segment's hit rect
    // off its rendered text and makes `R:`/`EQ:` unclickable. See `render_controls`.
    if !(minimal && labels.beginner()) {
        let state = match (minimal, app.playback.paused, labels.beginner() && retro) {
            (true, true, _) => "‖",
            (true, false, _) => "▸",
            (false, true, true) => "‖ paused",
            (false, false, true) => "▸ playing",
            (false, true, false) => t!("‖ paused", "‖ 일시정지", "‖ 一時停止"),
            (false, false, false) => t!("▸ playing", "▸ 재생 중", "▸ 再生中"),
        };
        parts.push((None, Cow::Borrowed(state)));
    }
}

/// The `pos/len` queue position, shown while the queue is non-empty.
fn push_queue_position(
    parts: &mut StatusLineParts,
    app: &App,
    labels: StatusLabelTier,
    gap: &'static str,
    retro: bool,
) {
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
                    t!("Queue", "대기열", "キュー")
                },
                Some(value),
            )
        } else {
            value
        };
        parts.push((Some(MouseTarget::QueuePos), Cow::Owned(text)));
    }
}

/// The current track's rating: one tri-state glyph (🤔/👍/👎) that replaces the old separate
/// ♥ favorite + ✗ dislike columns. Same `PlayerLabel` style and 4-space gap as the toggles next
/// to it; click and the `f` key hit the same Action, so it mirrors the keyboard exactly.
fn push_rating(
    parts: &mut StatusLineParts,
    app: &App,
    labels: StatusLabelTier,
    gap: &'static str,
    retro: bool,
) {
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
                        t!("Like", "좋아요", "高く評価")
                    } else if disliked {
                        t!("Dislike", "싫어요", "低く評価")
                    } else {
                        t!("None", "없음", "なし")
                    },
                    [
                        t!("Like", "좋아요", "高く評価"),
                        t!("Dislike", "싫어요", "低く評価"),
                        t!("None", "없음", "なし"),
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
                    t!("Rating", "평가", "評価")
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
}

/// The live-radio transport occupying the shuffle/repeat slots (and the same actions/keys):
/// `LIVE:` sync verdict, `SYNC:` re-sync, the `ID?` identify card, and the `REC` chip while
/// the radio recorder is writing a track.
fn push_live_transport(
    parts: &mut StatusLineParts,
    app: &App,
    labels: StatusLabelTier,
    gap: &'static str,
    retro: bool,
) {
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
                    Some(true) => t!("Synced", "동기화", "同期済み"),
                    Some(false) => t!("Behind", "뒤처짐", "遅延"),
                    None => t!("Unknown", "알 수 없음", "不明"),
                },
                [
                    t!("Synced", "동기화", "同期済み"),
                    t!("Behind", "뒤처짐", "遅延"),
                    t!("Unknown", "알 수 없음", "不明"),
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
                t!("Live", "라이브", "ライブ")
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
                t!("Resync", "다시 동기화", "再同期")
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
                t!("Identify", "곡 찾기", "曲を特定")
            },
            None,
        )
    } else {
        t!("ID?", "지듣노", "この曲?").to_owned()
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
                    t!("Recordings", "녹음 목록", "録音一覧")
                },
                None,
            )
        } else {
            t!("REC", "녹음", "録音").to_owned()
        };
        parts.push((
            Some(MouseTarget::Player(Action::ToggleRecordings)),
            Cow::Owned(recordings),
        ));
    }
}

/// The shuffle and repeat click toggles (the non-radio form of the two transport slots).
fn push_shuffle_repeat(
    parts: &mut StatusLineParts,
    app: &App,
    labels: StatusLabelTier,
    gap: &'static str,
    retro: bool,
) {
    parts.push((None, Cow::Borrowed(gap)));
    let shuffle = if labels.beginner() {
        let (state, states) = if retro {
            (if app.queue.shuffle { "On" } else { "Off" }, ["On", "Off"])
        } else {
            (
                if app.queue.shuffle {
                    t!("On", "켬", "オン")
                } else {
                    t!("Off", "끔", "オフ")
                },
                [t!("On", "켬", "オン"), t!("Off", "끔", "オフ")],
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
                t!("Shuffle", "셔플", "シャッフル")
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
                    Repeat::Off => t!("Off", "끔", "オフ"),
                    Repeat::All => t!("All", "전체", "すべて"),
                    Repeat::One => t!("One", "한 곡", "1曲"),
                },
                [
                    t!("Off", "끔", "オフ"),
                    t!("All", "전체", "すべて"),
                    t!("One", "한 곡", "1曲"),
                ],
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
                t!("Repeat", "반복", "リピート")
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

/// The `EQ:` preset label — a click target that opens the EQ menu.
fn push_eq(
    parts: &mut StatusLineParts,
    app: &App,
    labels: StatusLabelTier,
    gap: &'static str,
    retro: bool,
) {
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
                t!("Equalizer", "이퀄라이저", "イコライザー")
            },
            Some(pad_display_state(
                app.audio.preset.label(),
                &["Flat", "Bass", "Treble", "Vocal", "Rock", "Jazz", "Custom"],
            )),
        )
    } else {
        format!("EQ:{}", app.audio.preset.label())
    };
    parts.push((Some(MouseTarget::EqMenu), Cow::Owned(eq)));
}

/// The streaming-autoplay mode label while a station is active — or, in Local Deck, the
/// `off(saved:on)` form that keeps the suppressed preference visible.
fn push_streaming_mode(
    parts: &mut StatusLineParts,
    app: &App,
    labels: StatusLabelTier,
    gap: &'static str,
    retro: bool,
) {
    if app.streaming_active() {
        // Show the station's mode (Focused/Balanced/Discovery) as a click target that opens the
        // mode dropdown — same affordance as the `EQ:` label next to it.
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
                    t!(
                        "Streaming autoplay",
                        "스트리밍 자동재생",
                        "ストリーミング自動再生"
                    )
                },
                Some(format!(
                    "{} · {}",
                    if retro {
                        "On"
                    } else {
                        t!("On", "켬", "オン")
                    },
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
                    t!(
                        "Streaming autoplay",
                        "스트리밍 자동재생",
                        "ストリーミング自動再生"
                    )
                },
                Some(if retro {
                    "Off (saved On)".to_owned()
                } else {
                    t!("Off (saved On)", "끔 (저장: 켬)", "オフ (保存: オン)").to_owned()
                }),
            )
        } else {
            "streaming:off(saved:on)".to_owned()
        };
        parts.push((Some(MouseTarget::StreamingMenu), Cow::Owned(streaming)));
    }
}

/// Download indicator for the current track, if one is in flight or finished. While one is
/// actually running and the activity animation is on, a spinner stands in for the `⬇` so
/// live progress reads as motion (same single cell, so the line never shifts).
fn push_download_tag(parts: &mut StatusLineParts, app: &App, gap: &'static str, minimal: bool) {
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
}
