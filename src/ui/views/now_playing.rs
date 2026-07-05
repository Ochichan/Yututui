//! The "what's playing" (지듣노) card: a compact centered popup over the player that shows
//! the current radio song straight from the stream's ICY metadata, with two actions — save
//! the song to the music favorites, or (when DJ Gem is connected) hand off to DJ Gem for
//! the story. Opened with `i` ([`crate::keymap::Action::IdentifyNowPlaying`]); its inner
//! actions are remappable under [`crate::keymap::KeyContext::NowPlaying`]; `i` / Esc /
//! Enter close.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::app::{App, MouseTarget, NowPlayingOverlayState, ScrollSurface};
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
    // Empty heart until it's actually in the favorites; filled once saved. The fill is driven by
    // real library state (not merely a resolved YouTube match), so pressing favorite flips ♡ → ♥.
    let fav_label = if overlay.resolving {
        t!("Saving…", "저장 중…").to_owned()
    } else if app.now_playing_is_favorited() {
        format!("{} ({fav_key})", t!("♥ Saved", "♥ 저장됨"))
    } else {
        format!("{} ({fav_key})", t!("♡ Favorite", "♡ 즐겨찾기"))
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

/// The state-dependent body: the playing song, a station-content notice, or an honest miss.
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
        NowPlayingOverlayState::Playing { artist, title } => {
            let headline = match artist {
                Some(artist) => format!("{title} — {artist}"),
                None => title.clone(),
            };
            // Scroll a long "title — artist" so the whole thing is readable. `selected_marquee`
            // runs with the animation masters OFF too, so a clipped title crawls regardless of
            // the animation toggle; it returns the text unchanged when it already fits.
            vec![Line::from(Span::styled(
                crate::ui::anim::selected_marquee(app, ScrollSurface::NowPlaying, 0, &headline, width),
                primary,
            ))]
        }
        NowPlayingOverlayState::StationContent => vec![
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
    };
    frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), area);
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
