//! The artist detail view: a header (name · subscribers) over two lists — top songs and
//! albums/singles. Reached from a Search artist row; Esc/Back returns to Search.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, HighlightSpacing, List, ListItem, ListState, Paragraph};

use crate::app::{App, ArtistSection, MouseTarget, ScrollSurface, StatusKind};
use crate::t;
use crate::theme::ThemeRole as R;
use crate::ui::buttons;

pub fn render(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(app.theme.style(R::BorderPrimary))
        .style(app.theme.style(R::TextPrimary));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // The nav strip rides the top border line itself, like every other screen.
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

    // The two lists share the leftover space evenly; an empty section folds to its
    // 3-row frame so the populated one gets the room (the anonymous page can lack songs).
    let (songs_c, albums_c) = match app.artist.as_ref() {
        Some(st) if st.page.songs.is_empty() && !st.page.albums.is_empty() => {
            (Constraint::Length(3), Constraint::Fill(1))
        }
        Some(st) if st.page.albums.is_empty() && !st.page.songs.is_empty() => {
            (Constraint::Fill(1), Constraint::Length(3))
        }
        _ => (Constraint::Fill(1), Constraint::Fill(1)),
    };
    let rows = Layout::vertical([
        Constraint::Length(2), // reserved top band (status toast, aligned with Search)
        Constraint::Length(1), // header: artist name · subscribers
        songs_c,               // top songs
        albums_c,              // albums / singles
        Constraint::Length(crate::ui::control_box::docked_rows(app)), // docked player bar
        Constraint::Length(1), // help
    ])
    .split(inner);

    if !app.status.text.is_empty() && !app.control_box_active() {
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
            rows[0],
        );
    }

    let Some(st) = app.artist.as_ref() else {
        // Defensive: the mode is only entered with state present.
        return;
    };

    let header = match &st.page.subscribers {
        Some(subs) if !subs.is_empty() => format!("♪ {} · {subs}", st.page.name),
        _ => format!("♪ {}", st.page.name),
    };
    frame.render_widget(
        Paragraph::new(
            Line::from(header).style(app.theme.style(R::TextPrimary).add_modifier(Modifier::BOLD)),
        ),
        rows[1],
    );

    render_section(
        frame,
        app,
        rows[2],
        ArtistSection::Songs,
        t!(" Top songs ", " 인기곡 ", " 人気曲 "),
    );
    render_section(
        frame,
        app,
        rows[3],
        ArtistSection::Albums,
        t!(
            " Albums · Singles ",
            " 앨범 · 싱글 ",
            " アルバム · シングル "
        ),
    );
    crate::ui::control_box::render_docked(frame, app, rows[4]);
    buttons::render_help_button(frame, app, rows[5]);
}

fn render_section(frame: &mut Frame, app: &App, area: Rect, section: ArtistSection, title: &str) {
    let Some(st) = app.artist.as_ref() else {
        return;
    };
    let focused = st.section == section;
    let (rows, selected, scroll, surface) = match section {
        ArtistSection::Songs => (
            &st.page.songs,
            st.songs_selected,
            &st.songs_scroll,
            ScrollSurface::ArtistSongs,
        ),
        ArtistSection::Albums => (
            &st.page.albums,
            st.albums_selected,
            &st.albums_scroll,
            ScrollSurface::ArtistAlbums,
        ),
    };
    let border = if focused {
        R::BorderFocused
    } else {
        R::BorderMuted
    };
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(app.theme.style(border))
        .style(app.theme.style(R::TextPrimary));
    let list_area = block.inner(area);
    frame.render_widget(block, area);
    if list_area.height == 0 || list_area.width == 0 {
        return;
    }
    if rows.is_empty() {
        frame.render_widget(
            Paragraph::new(t!("(none)", "(없음)", "(なし)")).style(app.theme.style(R::TextMuted)),
            list_area,
        );
        return;
    }

    let len = rows.len();
    let selected = selected.min(len - 1);
    let offset = scroll.resolve(
        selected,
        list_area.height,
        len,
        crate::ui::scroll::SCROLLOFF,
    );
    let visible_sel = (offset..offset + list_area.height as usize).contains(&selected);

    let items: Vec<ListItem> = rows
        .iter()
        .skip(offset)
        .take(list_area.height as usize)
        .map(|song| {
            let is_favorite =
                song.youtube_playlist_id().is_none() && app.library.is_favorite(&song.video_id);
            let heart = if is_favorite { "♥ " } else { "  " };
            let title = app.display_title(song);
            let artist = app.display_artist(song);
            let text = match (artist.is_empty(), song.duration.is_empty()) {
                (true, true) => title.into_owned(),
                (true, false) => format!("{title}  ({})", song.duration),
                (false, true) => format!("{title} — {artist}"),
                (false, false) => format!("{title} — {artist}  ({})", song.duration),
            };
            ListItem::new(format!("{heart}{text}")).style(app.theme.style(R::TextPrimary))
        })
        .collect();
    let highlight = if focused {
        Style::default()
            .fg(app.theme.color(R::SelectionFg))
            .bg(app.theme.color(R::SelectionBg))
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(app.theme.color(R::SelectionInactiveFg))
            .bg(app.theme.color(R::SelectionInactiveBg))
    };
    let list = List::new(items)
        .highlight_style(highlight)
        .style(app.theme.style(R::TextPrimary))
        .highlight_symbol("▶ ")
        .highlight_spacing(HighlightSpacing::Always);
    let mut state = ListState::default();
    if visible_sel {
        state.select(Some(selected - offset));
    }
    frame.render_stateful_widget(list, list_area, &mut state);

    // Each visible row is a click target: single-click selects, double-click activates.
    for vis in 0..list_area.height {
        let item = offset + vis as usize;
        if item >= len {
            break;
        }
        let target = match section {
            ArtistSection::Songs => MouseTarget::ArtistSongRow(item),
            ArtistSection::Albums => MouseTarget::ArtistAlbumRow(item),
        };
        app.register_mouse_button(
            Rect {
                x: list_area.x,
                y: list_area.y + vis,
                width: list_area.width,
                height: 1,
            },
            target,
        );
    }
    buttons::render_list_scrollbar(
        frame,
        app,
        Rect {
            x: list_area.right(),
            y: list_area.y,
            width: 1,
            height: list_area.height,
        },
        surface,
        len,
        offset,
        list_area.height as usize,
    );
}
