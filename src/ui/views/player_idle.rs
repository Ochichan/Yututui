//! The empty Player: what to do when nothing is loaded and no canvas, lyrics, or art fills
//! the space above the player bar.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use crate::app::{App, MouseTarget};
use crate::t;
use crate::theme::ThemeRole as R;

/// The empty Player: a heading and one row per way in (search, library, DJ Gem), each naming
/// its current key and clickable. Drawn only when it fits whole.
pub(super) fn render_idle_card(frame: &mut Frame, app: &App, area: Rect) {
    use crate::keymap::{Action, KeyContext};
    let retro = app.retro_mode();
    let rows: Vec<(Action, String, &str)> = [
        (
            Action::OpenSearch,
            t!("Search for music", "음악 검색", "音楽を検索"),
        ),
        (
            Action::OpenLibrary,
            t!("Open your library", "라이브러리 열기", "ライブラリを開く"),
        ),
        (
            Action::OpenAi,
            t!("Ask DJ Gem", "DJ Gem에게 요청", "DJ Gem に頼む"),
        ),
    ]
    .into_iter()
    .map(|(action, label)| {
        let key = app
            .keymap
            .label_for_display(KeyContext::Player, action, retro);
        (action, key, label)
    })
    .collect();
    // The docked bar's title already says nothing is playing; this names what to do.
    let heading = t!("♪  Start listening", "♪  음악 듣기 시작", "♪  聴きはじめる");
    let key_width = rows
        .iter()
        .map(|(_, key, _)| UnicodeWidthStr::width(key.as_str()))
        .max()
        .unwrap_or(1);
    let width = rows
        .iter()
        .map(|(_, _, label)| key_width + 2 + UnicodeWidthStr::width(*label))
        .chain(std::iter::once(UnicodeWidthStr::width(heading)))
        .max()
        .unwrap_or(0) as u16;
    // Heading, a blank row, then one row per action.
    let height = rows.len() as u16 + 2;
    if area.width < width + 2 || area.height < height {
        return;
    }
    let x = area.x + (area.width - width) / 2;
    let y = area.y + (area.height - height) / 2;
    let row = |offset: u16| Rect {
        x,
        y: y + offset,
        width,
        height: 1,
    };
    frame.render_widget(
        Paragraph::new(heading).style(app.theme.style(R::TextPrimary).add_modifier(Modifier::BOLD)),
        row(0),
    );
    for (index, (action, key, label)) in rows.into_iter().enumerate() {
        let rect = row(index as u16 + 2);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                ratatui::text::Span::styled(
                    crate::ui::text::pad_to_width(&key, key_width + 2),
                    app.theme.style(R::Accent),
                ),
                ratatui::text::Span::styled(label.to_owned(), app.theme.style(R::TextMuted)),
            ])),
            rect,
        );
        app.register_mouse_button(rect, MouseTarget::Player(action));
    }
}
