//! Radio-recording settings and recordings-browser overlays.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph};

use super::{bar, centered_fixed, slider_str};
use crate::app::{App, MouseTarget};
use crate::t;
use crate::theme::ThemeRole as R;
use crate::ui::buttons;
use crate::ui::text::pad_to_width;

/// The radio-recording settings popup (opened from the Playback tab's one radio item). Rows:
/// mode · min · max · output folder · keep-recent · notifications · browse.
pub fn render_recording_settings(frame: &mut Frame, app: &App, area: Rect) {
    use crate::config::{
        RECORDING_MAX_SECONDS_MAX, RECORDING_MAX_SECONDS_MIN, RECORDING_MIN_SECONDS_MAX,
        RECORDING_MIN_SECONDS_MIN, RECORDING_PAST_TRACKS_MAX, RECORDING_PAST_TRACKS_MIN,
    };
    let Some(popup) = app.overlays.recording_settings.as_ref() else {
        return;
    };
    let Some(st) = app.settings.as_deref() else {
        return;
    };
    let d = &st.draft;
    let popup_rect = centered_fixed(area, 60, 13);
    crate::ui::render_popup_background(frame, app, popup_rect);

    let block = Block::default()
        .title(t!(" Radio recording ", " 라디오 녹음 "))
        .borders(Borders::ALL)
        .border_style(crate::ui::confirm_border_style(app))
        .style(crate::ui::popup_style(app, R::TextPrimary));
    let inner = block.inner(popup_rect);
    frame.render_widget(block, popup_rect);
    let rows = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);

    let label_w = 16usize;
    let value_w = usize::from(rows[0].width).saturating_sub(label_w + 2);
    let dir_display = if popup.editing_dir {
        crate::ui::text::editable_value(
            &d.recording_dir,
            popup.dir_cursor.byte_index(&d.recording_dir),
            value_w,
            crate::ui::anim::caret_char(app),
            false,
        )
    } else if d.recording_dir.trim().is_empty() {
        t!("(default)", "(기본값)").to_owned()
    } else {
        d.recording_dir.clone()
    };
    let entries: Vec<(String, String)> = vec![
        (
            t!("Mode", "모드").to_owned(),
            format!("< {} >", d.recording_mode.label()),
        ),
        (
            t!("Min duration", "최소 길이").to_owned(),
            slider_str(
                &bar(
                    d.recording_min_seconds as f64,
                    RECORDING_MIN_SECONDS_MIN as f64,
                    RECORDING_MIN_SECONDS_MAX as f64,
                ),
                &format!("{}s", d.recording_min_seconds),
            ),
        ),
        (
            t!("Max duration", "최대 길이").to_owned(),
            slider_str(
                &bar(
                    d.recording_max_seconds as f64,
                    RECORDING_MAX_SECONDS_MIN as f64,
                    RECORDING_MAX_SECONDS_MAX as f64,
                ),
                &format!("{} min", d.recording_max_seconds / 60),
            ),
        ),
        (t!("Output folder", "저장 폴더").to_owned(), dir_display),
        (
            t!("Keep recent", "최근 보관").to_owned(),
            slider_str(
                &bar(
                    d.recording_past_tracks as f64,
                    RECORDING_PAST_TRACKS_MIN as f64,
                    RECORDING_PAST_TRACKS_MAX as f64,
                ),
                &format!("{}", d.recording_past_tracks),
            ),
        ),
        (
            t!("Notifications", "알림").to_owned(),
            if d.recording_notify {
                "[x]".to_owned()
            } else {
                "[ ]".to_owned()
            },
        ),
        (
            t!("Browse recordings…", "녹음 목록 보기…").to_owned(),
            "\u{21b5}".to_owned(),
        ),
    ];

    let lines: Vec<Line> = entries
        .iter()
        .enumerate()
        .map(|(i, (label, value))| {
            let marker = if i == popup.row { "\u{25b8} " } else { "  " };
            let style = if i == popup.row {
                crate::ui::popup_style(app, R::TextPrimary).add_modifier(Modifier::BOLD)
            } else {
                crate::ui::popup_style(app, R::TextMuted)
            };
            Line::styled(
                format!("{marker}{}{value}", pad_to_width(label, label_w)),
                style,
            )
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), rows[0]);
    // Publish whole-row targets first; finer arrow/slider targets registered later win hit tests.
    popup.rect.set(Some(popup_rect));
    let right = rows[0].right();
    for (i, (label, value)) in entries.iter().enumerate() {
        let y = rows[0].y + i as u16;
        if y >= rows[0].bottom() {
            break;
        }
        app.register_mouse_button(
            Rect {
                x: rows[0].x,
                y,
                width: rows[0].width,
                height: 1,
            },
            MouseTarget::RecordingRow(i),
        );
        // Measure the same display-width-padded prefix that was rendered above.
        let marker = if i == popup.row { "\u{25b8} " } else { "  " };
        let vx = rows[0].x
            + buttons::text_width(marker)
            + buttons::text_width(&pad_to_width(label, label_w));
        let vw = buttons::text_width(value);
        let put = |x: u16, w: u16, target: MouseTarget| {
            if w == 0 || x >= right {
                return;
            }
            app.register_mouse_button(
                Rect {
                    x,
                    y,
                    width: w.min(right - x),
                    height: 1,
                },
                target,
            );
        };
        // The adjustable rows expose arrow targets; numeric rows also expose their bar.
        if matches!(i, 0 | 1 | 2 | 4) {
            put(vx, 1, MouseTarget::RecordingChange { row: i, delta: -1 });
            let last = vx.saturating_add(vw.saturating_sub(1));
            put(last, 1, MouseTarget::RecordingChange { row: i, delta: 1 });
            if i != 0 {
                put(vx + 2, 11, MouseTarget::RecordingSlider(i));
            }
        }
    }

    let help = if popup.editing_dir {
        t!(
            "Type a folder \u{b7} Enter done \u{b7} Esc cancel",
            "폴더 입력 \u{b7} Enter 완료 \u{b7} Esc 취소"
        )
    } else {
        t!(
            "\u{2191}/\u{2193} \u{b7} \u{2190}/\u{2192} change \u{b7} Enter \u{b7} Esc close",
            "\u{2191}/\u{2193} \u{b7} \u{2190}/\u{2192} 변경 \u{b7} Enter \u{b7} Esc 닫기"
        )
    };
    frame.render_widget(
        Paragraph::new(help)
            .alignment(Alignment::Center)
            .style(crate::ui::popup_style(app, R::TextMuted)),
        rows[1],
    );
    crate::ui::seal_popup_background(frame, app, popup_rect);
    crate::ui::mark_art_rows_for_popup(frame, app, popup_rect);
}

/// The recordings browser: the in-progress track (if any) plus recent finished tracks, with
/// per-row save/discard/play. Reads `app.recorder`.
pub fn render_recordings_browser(frame: &mut Frame, app: &App, area: Rect) {
    let Some(browser) = app.overlays.recordings_browser.as_ref() else {
        return;
    };
    let mut items: Vec<(String, String)> = Vec::new();
    if let Some(seg) = app.recorder.current.as_ref() {
        items.push((
            format!("\u{25cf} {}", seg_display(seg)),
            t!("Recording\u{2026}", "녹음 중\u{2026}").to_owned(),
        ));
    }
    for t in app.recorder.history.iter() {
        let glyph = if matches!(t.state, crate::recorder::RecordingState::Saved) {
            "\u{25a3}"
        } else if t.state.is_recorded() {
            "\u{2713}"
        } else {
            "\u{b7}"
        };
        items.push((
            format!("{glyph} {}", t.display()),
            format!("{}  {}", fmt_dur(t.duration_secs), t.state.label()),
        ));
    }

    let n = items.len();
    let h = ((n as u16) + 4).clamp(7, 20);
    let popup = centered_fixed(area, 64, h);
    // Publish the browser rect so a click outside closes it (mirrors the queue window).
    browser.rect.set(Some(popup));
    crate::ui::render_popup_background(frame, app, popup);
    let block = Block::default()
        .title(t!(" Radio recordings ", " 라디오 녹음 목록 "))
        .borders(Borders::ALL)
        .border_style(crate::ui::confirm_border_style(app))
        .style(crate::ui::popup_style(app, R::TextPrimary));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let rows = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);
    let visible = rows[0].height as usize;

    if items.is_empty() {
        frame.render_widget(
            Paragraph::new(t!("No recordings yet.", "아직 녹음이 없어요."))
                .alignment(Alignment::Center)
                .style(crate::ui::popup_style(app, R::TextMuted)),
            rows[0],
        );
    } else {
        let sel = browser.selected.min(n.saturating_sub(1));
        let first = sel
            .saturating_sub(visible.saturating_sub(1))
            .min(n.saturating_sub(visible.max(1)));
        let lines: Vec<Line> = items
            .iter()
            .enumerate()
            .skip(first)
            .take(visible)
            .map(|(i, (title, sub))| {
                let marker = if i == sel { "\u{25b8} " } else { "  " };
                let style = if i == sel {
                    crate::ui::popup_style(app, R::TextPrimary).add_modifier(Modifier::BOLD)
                } else {
                    crate::ui::popup_style(app, R::TextMuted)
                };
                Line::styled(format!("{marker}{title}   {sub}"), style)
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), rows[0]);
        // One clickable rect per visible row → single-click selects that item.
        for (vis_idx, i) in (first..n).take(visible).enumerate() {
            app.register_mouse_button(
                Rect {
                    x: rows[0].x,
                    y: rows[0].y + vis_idx as u16,
                    width: rows[0].width,
                    height: 1,
                },
                MouseTarget::RecordingBrowseRow(i),
            );
        }
    }
    frame.render_widget(
        Paragraph::new(t!(
            "\u{2191}/\u{2193} \u{b7} s save \u{b7} d discard \u{b7} Enter play \u{b7} Esc close",
            "\u{2191}/\u{2193} \u{b7} s 저장 \u{b7} d 삭제 \u{b7} Enter 재생 \u{b7} Esc 닫기"
        ))
        .alignment(Alignment::Center)
        .style(crate::ui::popup_style(app, R::TextMuted)),
        rows[1],
    );
    crate::ui::seal_popup_background(frame, app, popup);
    crate::ui::mark_art_rows_for_popup(frame, app, popup);
}

fn seg_display(seg: &crate::recorder::OpenSegment) -> String {
    let base = seg.raw.trim();
    if base.is_empty() {
        seg.title.clone().unwrap_or_else(|| "\u{2014}".to_owned())
    } else {
        base.to_owned()
    }
}

fn fmt_dur(secs: u32) -> String {
    format!("{}:{:02}", secs / 60, secs % 60)
}
