//! The "what's playing" (지듣노) card: a compact centered popup over the player that
//! shows what the radio identify one-shot made of the current ICY title, with two
//! actions — save the song to the music favorites, or hand off to DJ Gem for the story.
//! Opened with `i` ([`crate::keymap::Action::IdentifyNowPlaying`]); its inner actions are
//! remappable under [`crate::keymap::KeyContext::NowPlaying`]; `i` / Esc / Enter close.
//! Confidence shapes the presentation: high reads as fact, medium as "(probably)", and
//! low/unknown honestly says it couldn't tell — never a guess dressed up as an answer.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::app::{
    App, IdentifiedKind, IdentifiedNowPlaying, IdentifyConfidence, MouseTarget,
    NowPlayingOverlayState,
};
use crate::keymap::{Action, KeyContext};
use crate::t;
use crate::theme::ThemeRole as R;
use crate::ui::buttons::{self, Seg};
use crate::ui::text::truncate_to_width;

/// Render the card as a centered popup over `area`.
pub fn render(frame: &mut Frame, app: &App, area: Rect) {
    let Some(overlay) = app.now_playing_overlay.as_ref() else {
        return;
    };

    let popup = centered_fixed(area, 58, 11);
    crate::ui::render_popup_background(frame, app, popup);

    let block = Block::default()
        .title(t!(" Now playing ", " 지금 듣는 노래 "))
        .borders(Borders::ALL)
        .border_style(crate::ui::popup_style(app, R::BorderPrimary))
        .style(crate::ui::popup_style(app, R::TextPrimary));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let rows = Layout::vertical([
        Constraint::Length(1), // station
        Constraint::Length(1), // gap
        Constraint::Min(3),    // body (identification / state)
        Constraint::Length(1), // gap
        Constraint::Length(1), // action buttons
        Constraint::Length(1), // close hint
    ])
    .split(inner);
    let width = inner.width.saturating_sub(2) as usize;

    frame.render_widget(
        Paragraph::new(truncate_to_width(&overlay.station_label, width))
            .alignment(Alignment::Center)
            .style(crate::ui::popup_style(app, R::TextMuted)),
        rows[0],
    );

    draw_body(frame, app, rows[2], &overlay.state, &overlay.raw_title);

    // Action row: only the actions the state supports, plus Close. Labels carry the live
    // chord (respecting rebinds — the actions are remappable under
    // `KeyContext::NowPlaying`) so the keyboard path is always discoverable.
    let retro = app.retro_mode();
    let fav_key =
        app.keymap
            .label_for_display(KeyContext::NowPlaying, Action::NowPlayingFavorite, retro);
    let ask_key =
        app.keymap
            .label_for_display(KeyContext::NowPlaying, Action::NowPlayingAskAi, retro);
    let fav_label = if overlay.resolving {
        t!("Saving…", "저장 중…").to_owned()
    } else if overlay.resolved.is_some() {
        format!("{} ({fav_key})", t!("♥ Saved", "♥ 저장됨"))
    } else {
        format!("{} ({fav_key})", t!("♥ Favorite", "♥ 즐겨찾기"))
    };
    let ask_label = format!("{} ({ask_key})", t!("Tell me more", "더 알아보기"));
    let mut segs: Vec<Seg> = Vec::with_capacity(5);
    if app.now_playing_can_favorite() {
        segs.push(Seg::button(MouseTarget::NowPlayingFavorite, &fav_label));
        segs.push(Seg::label("   "));
    }
    if app.now_playing_can_ask() {
        segs.push(Seg::button(MouseTarget::NowPlayingAskAi, &ask_label));
        segs.push(Seg::label("   "));
    }
    segs.push(Seg::button(
        MouseTarget::CloseNowPlaying,
        t!("Close", "닫기"),
    ));
    buttons::render_segments(
        frame,
        app,
        rows[4],
        &segs,
        crate::ui::confirm_button_style(app),
        crate::ui::confirm_gap_style(app),
        Alignment::Center,
    );

    let close_key =
        app.keymap
            .label_for_display(KeyContext::Player, Action::IdentifyNowPlaying, retro);
    let hint = if crate::i18n::is_korean() {
        format!("{close_key} / Esc 닫기")
    } else {
        format!("{close_key} / Esc to close")
    };
    frame.render_widget(
        Paragraph::new(Line::from(hint))
            .alignment(Alignment::Center)
            .style(crate::ui::popup_style(app, R::TextMuted)),
        rows[5],
    );

    crate::ui::seal_popup_background(frame, app, popup);
    crate::ui::mark_art_rows_for_popup(frame, app, popup);
}

/// The state-dependent body: spinner text, the identification, or an honest miss.
fn draw_body(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    state: &NowPlayingOverlayState,
    raw_title: &str,
) {
    let width = area.width.saturating_sub(2) as usize;
    let primary = crate::ui::popup_style(app, R::HelpAction).add_modifier(Modifier::BOLD);
    let muted = crate::ui::popup_style(app, R::TextMuted);

    let lines: Vec<Line> = match state {
        NowPlayingOverlayState::Loading => vec![Line::from(Span::styled(
            t!("Identifying…", "확인 중…"),
            muted,
        ))],
        NowPlayingOverlayState::NoMetadata => vec![
            Line::from(Span::styled(
                t!(
                    "This station doesn't expose song info.",
                    "이 방송국은 곡 정보를 제공하지 않아요"
                ),
                primary,
            )),
            Line::from(""),
            Line::from(Span::styled(
                t!(
                    "(no usable stream title right now)",
                    "(지금은 쓸 만한 스트림 제목이 없어요)"
                ),
                muted,
            )),
        ],
        NowPlayingOverlayState::Error(message) => vec![
            Line::from(Span::styled(
                t!(
                    "Couldn't identify this one.",
                    "무슨 곡인지 알아내지 못했어요"
                ),
                primary,
            )),
            Line::from(""),
            Line::from(Span::styled(truncate_to_width(message, width), muted)),
        ],
        NowPlayingOverlayState::Identified(id) => {
            identified_lines(id, raw_title, width, primary, muted)
        }
    };
    frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), area);
}

/// Lines for a completed identification, shaped by kind + confidence.
fn identified_lines(
    id: &IdentifiedNowPlaying,
    raw_title: &str,
    width: usize,
    primary: ratatui::style::Style,
    muted: ratatui::style::Style,
) -> Vec<Line<'static>> {
    match id.kind {
        IdentifiedKind::Ad => vec![
            Line::from(Span::styled(
                t!(
                    "Sounds like station content (ad / jingle), not a song.",
                    "광고나 방송국 징글 같아요 — 노래가 아니에요"
                ),
                primary,
            )),
            Line::from(""),
            Line::from(Span::styled(truncate_to_width(raw_title, width), muted)),
        ],
        IdentifiedKind::Unknown => vec![
            Line::from(Span::styled(
                t!(
                    "Couldn't identify this one.",
                    "무슨 곡인지 알아내지 못했어요"
                ),
                primary,
            )),
            Line::from(""),
            Line::from(Span::styled(truncate_to_width(raw_title, width), muted)),
        ],
        IdentifiedKind::Song => {
            let name = match (&id.title, &id.artist) {
                (Some(title), Some(artist)) => format!("{title} — {artist}"),
                (Some(title), None) => title.clone(),
                (None, Some(artist)) => artist.clone(),
                (None, None) => raw_title.to_owned(),
            };
            let headline = match id.confidence {
                IdentifyConfidence::High => name,
                IdentifyConfidence::Medium => {
                    format!("{}{name}", t!("(probably) ", "(아마도) "))
                }
                IdentifyConfidence::Low => {
                    format!("{}{name}", t!("(uncertain) ", "(불확실) "))
                }
            };
            let mut lines = vec![Line::from(Span::styled(
                truncate_to_width(&headline, width),
                primary,
            ))];
            if let Some(note) = &id.note {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    truncate_to_width(note, width),
                    muted,
                )));
            }
            lines
        }
    }
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
