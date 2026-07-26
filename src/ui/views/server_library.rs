//! Music-server library renderer with explicit playlist access markers.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::app::{App, LibrarySource, MouseTarget, ScrollSurface};
use crate::open_subsonic::model::{
    LibraryWarning, ServerLibraryDetail, ServerLibraryRow, ServerLibrarySection,
    ServerPlaylistAccess,
};
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
        Constraint::Length(1),
        Constraint::Length(2),
        Constraint::Length(2),
        Constraint::Min(0),
        Constraint::Length(crate::ui::control_box::docked_rows(app)),
        Constraint::Length(1),
    ])
    .split(inner);
    render_source_selector(frame, app, rows[0]);
    render_sections(frame, app, rows[1]);
    render_context(frame, app, rows[2]);
    render_rows(frame, app, rows[3]);
    crate::ui::control_box::render_docked(frame, app, rows[4]);
    buttons::render_help_button(frame, app, rows[5]);
}

pub(super) fn render_source_selector(frame: &mut Frame, app: &App, area: Rect) {
    if area.is_empty() {
        return;
    }
    let server_name = app.server.settings.summary.display_name();
    let local = " YuTuTui ";
    let server = format!(" {} ", server_name);
    let separator = "  |  ";
    let compact = buttons::text_width(local)
        .saturating_add(buttons::text_width(&server))
        .saturating_add(buttons::text_width(separator))
        > area.width;
    let server = if compact {
        format!(" {} ", t!("Server", "서버", "サーバー"))
    } else {
        server
    };

    let mut spans = Vec::new();
    let mut x = area.x;
    for (source, label) in [
        (LibrarySource::Yututui, local.to_owned()),
        (LibrarySource::OpenSubsonic, server),
    ] {
        if !spans.is_empty() {
            spans.push(Span::styled(separator, app.theme.style(R::TextMuted)));
            x = x.saturating_add(buttons::text_width(separator));
        }
        let width = buttons::text_width(&label).min(area.right().saturating_sub(x));
        let selected = app.server.library.source == source;
        let style = if selected {
            Style::default()
                .fg(app.theme.color(R::SelectionFg))
                .bg(app.theme.color(R::SelectionBg))
                .add_modifier(Modifier::BOLD)
        } else {
            app.theme.style(R::TextMuted)
        };
        spans.push(Span::styled(label, style));
        if width > 0 {
            app.register_mouse_button(
                Rect {
                    x,
                    y: area.y,
                    width,
                    height: 1,
                },
                MouseTarget::LibrarySource(source),
            );
        }
        x = x.saturating_add(width);
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_sections(frame: &mut Frame, app: &App, area: Rect) {
    let section_rows = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(area);
    let sections = [
        ServerLibrarySection::RecentlyPlayed,
        ServerLibrarySection::Albums,
        ServerLibrarySection::Artists,
        ServerLibrarySection::Songs,
        ServerLibrarySection::Playlists,
    ];
    render_section_line(frame, app, section_rows[0], &sections[..3]);
    render_section_line(frame, app, section_rows[1], &sections[3..]);
}

fn render_section_line(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    sections: &[ServerLibrarySection],
) {
    let full_width = sections
        .iter()
        .copied()
        .enumerate()
        .map(|(index, section)| {
            buttons::text_width(section_label(section, false))
                .saturating_add(2)
                .saturating_add((index > 0) as u16 * 2)
        })
        .sum::<u16>();
    // A 30-column terminal leaves 28 inner cells. Use one consistent compact vocabulary for
    // both section rows at that tier instead of mixing a compact first row with a full second.
    let compact = area.width <= 28 || full_width > area.width;
    let gap = if compact { " " } else { "  " };
    let mut spans = Vec::new();
    let mut x = area.x;
    for (index, section) in sections.iter().copied().enumerate() {
        if index > 0 {
            spans.push(Span::styled(gap, app.theme.style(R::TextMuted)));
            x = x.saturating_add(buttons::text_width(gap));
        }
        let label = if compact {
            section_label(section, true).to_owned()
        } else {
            format!(" {} ", section_label(section, false))
        };
        let width = buttons::text_width(&label).min(area.right().saturating_sub(x));
        let active = app.server.library.section == section;
        let style = if active {
            Style::default()
                .fg(app.theme.color(R::SelectionFg))
                .bg(app.theme.color(R::SelectionBg))
                .add_modifier(Modifier::BOLD)
        } else {
            app.theme.style(R::TextMuted)
        };
        spans.push(Span::styled(label, style));
        if width > 0 {
            app.register_mouse_button(
                Rect {
                    x,
                    y: area.y,
                    width,
                    height: 1,
                },
                MouseTarget::ServerLibrarySection(section),
            );
        }
        x = x.saturating_add(width);
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn section_label(section: ServerLibrarySection, compact: bool) -> &'static str {
    match (section, compact) {
        (ServerLibrarySection::RecentlyPlayed, false) => {
            t!("Recently played", "최근 재생", "最近再生")
        }
        (ServerLibrarySection::RecentlyPlayed, true) => t!("Recent", "최근", "最近"),
        (ServerLibrarySection::Albums, _) => t!("Albums", "앨범", "アルバム"),
        (ServerLibrarySection::Artists, false) => t!("Artists", "아티스트", "アーティスト"),
        (ServerLibrarySection::Artists, true) => t!("Artists", "아티스트", "歌手"),
        (ServerLibrarySection::Songs, _) => t!("Songs", "노래", "曲"),
        (ServerLibrarySection::Playlists, false) => {
            t!("Playlists", "플레이리스트", "プレイリスト")
        }
        (ServerLibrarySection::Playlists, true) => t!("Lists", "목록", "リスト"),
    }
}

fn render_context(frame: &mut Frame, app: &App, area: Rect) {
    if area.is_empty() {
        return;
    }
    let generation = app.server.library.generation;
    if let Some(failure) = app.server.library.failure {
        frame.render_widget(
            Paragraph::new(failure.label())
                .style(app.theme.style(R::Error))
                .wrap(ratatui::widgets::Wrap { trim: true }),
            area,
        );
        return;
    }
    if app.server.library.busy.is_some() {
        frame.render_widget(
            Paragraph::new(t!("Loading…", "불러오는 중…", "読み込み中…"))
                .style(app.theme.style(R::Accent)),
            area,
        );
        return;
    }
    if let Some(detail) = app.server.library.detail.as_ref() {
        let title = match detail {
            ServerLibraryDetail::AlbumSongs { album, .. } => {
                format!("← {}", album.name)
            }
            ServerLibraryDetail::ArtistAlbums { artist, .. } => {
                format!("← {}", artist.name)
            }
            ServerLibraryDetail::PlaylistEntries(playlist) => {
                format!("← {}", playlist.summary.name)
            }
        };
        let text = crate::ui::text::truncate_owned_to_width(title, area.width as usize);
        frame.render_widget(
            Paragraph::new(text).style(app.theme.style(R::SettingsGroup)),
            Rect { height: 1, ..area },
        );
        app.register_mouse_button(
            Rect {
                height: 1,
                width: area.width,
                ..area
            },
            MouseTarget::ServerLibraryBack { generation },
        );
        return;
    }

    let previous = if app.server.library.previous_offsets.is_empty() {
        t!("Previous", "이전", "前へ")
    } else {
        t!("← Previous", "← 이전", "← 前へ")
    };
    let next = if app.server.library.next_offset().is_some() {
        t!("Next →", "다음 →", "次へ →")
    } else {
        t!("Next", "다음", "次へ")
    };
    let page = app.server.library.offset / crate::app::SERVER_LIBRARY_PAGE_LIMIT + 1;
    let line = format!("{previous}  ·  {page}  ·  {next}");
    frame.render_widget(
        Paragraph::new(line)
            .style(app.theme.style(R::TextMuted))
            .alignment(Alignment::Center),
        Rect { height: 1, ..area },
    );
    let half = area.width / 2;
    app.register_mouse_button(
        Rect {
            width: half,
            height: 1,
            ..area
        },
        MouseTarget::ServerLibraryPreviousPage { generation },
    );
    app.register_mouse_button(
        Rect {
            x: area.x.saturating_add(half),
            width: area.width.saturating_sub(half),
            height: 1,
            ..area
        },
        MouseTarget::ServerLibraryNextPage { generation },
    );
    if let Some(warning) = app
        .server
        .library
        .page
        .as_ref()
        .and_then(|page| page.warning)
    {
        let text = match warning {
            LibraryWarning::FeatureUnsupported => t!(
                "Some server features are unavailable.",
                "일부 서버 기능을 사용할 수 없어요.",
                "一部のサーバー機能を利用できません。"
            ),
            LibraryWarning::PartialResults => t!(
                "Showing partial results.",
                "일부 결과만 표시하고 있어요.",
                "一部の結果を表示しています。"
            ),
        };
        frame.render_widget(
            Paragraph::new(text).style(app.theme.style(R::Warning)),
            Rect {
                y: area.y.saturating_add(1),
                height: 1,
                ..area
            },
        );
    }
}

fn render_rows(frame: &mut Frame, app: &App, area: Rect) {
    app.bridges.list_viewport_rows.set(area.height);
    let len = app.server.library.rows_len();
    if area.is_empty() {
        return;
    }
    if len == 0 {
        frame.render_widget(
            Paragraph::new(t!(
                "Nothing to show in this section.",
                "이 섹션에 표시할 항목이 없어요.",
                "このセクションに表示する項目はありません。"
            ))
            .style(app.theme.style(R::TextMuted)),
            area,
        );
        return;
    }

    let cursor = app.server.library.selected.min(len - 1);
    let visible = area.height as usize;
    let start =
        app.bridges
            .library_scroll
            .resolve(cursor, area.height, len, crate::ui::scroll::SCROLLOFF);
    for visible_index in 0..visible {
        let index = start + visible_index;
        if index >= len {
            break;
        }
        let selected = index == cursor;
        let marker = if selected { "▶ " } else { "  " };
        let text = server_row_text(app, index);
        let text = crate::ui::text::truncate_owned_to_width(
            format!("{marker}{text}"),
            area.width.saturating_sub(1) as usize,
        );
        let style = if selected {
            Style::default()
                .fg(app.theme.color(R::SelectionFg))
                .bg(app.theme.color(R::SelectionBg))
                .add_modifier(Modifier::BOLD)
        } else {
            app.theme.style(R::TextPrimary)
        };
        let rect = Rect {
            x: area.x,
            y: area.y + visible_index as u16,
            width: area.width,
            height: 1,
        };
        frame.render_widget(Paragraph::new(text).style(style), rect);
        app.register_mouse_button(
            rect,
            MouseTarget::ServerLibraryRow {
                generation: app.server.library.generation,
                index,
            },
        );
    }
    buttons::render_list_scrollbar(
        frame,
        app,
        Rect {
            x: area.right(),
            y: area.y,
            width: 1,
            height: area.height,
        },
        ScrollSurface::Library,
        len,
        start,
        visible,
    );
}

fn server_row_text(app: &App, index: usize) -> String {
    match app.server.library.detail.as_ref() {
        Some(ServerLibraryDetail::AlbumSongs { songs, .. }) => {
            songs.get(index).map_or_else(String::new, song_text)
        }
        Some(ServerLibraryDetail::ArtistAlbums { albums, .. }) => {
            albums.get(index).map_or_else(String::new, album_text)
        }
        Some(ServerLibraryDetail::PlaylistEntries(playlist)) => playlist
            .entries
            .get(index)
            .map_or_else(String::new, song_text),
        None => app
            .server
            .library
            .page
            .as_ref()
            .and_then(|page| page.rows.get(index))
            .map_or_else(String::new, |row| match row {
                ServerLibraryRow::Song(song) => song_text(song),
                ServerLibraryRow::Album(album) => album_text(album),
                ServerLibraryRow::Artist(artist) => {
                    let count = artist.album_count.map_or_else(String::new, |count| {
                        format!("  ·  {count} {}", t!("albums", "앨범", "アルバム"))
                    });
                    format!("♬ {}{count}", artist.name)
                }
                ServerLibraryRow::Playlist(playlist) => {
                    let count = playlist.song_count.map_or_else(String::new, |count| {
                        format!("  ·  {count} {}", t!("songs", "곡", "曲"))
                    });
                    let linked = match playlist.link.as_ref().map(|link| link.health) {
                        Some(crate::open_subsonic::ServerPlaylistLinkHealth::NeedsAttention) => {
                            format!(
                                "  ·  {}",
                                t!(
                                    "Linked playlist needs attention",
                                    "연결된 목록 확인 필요",
                                    "連携リストを確認"
                                )
                            )
                        }
                        Some(crate::open_subsonic::ServerPlaylistLinkHealth::ServerMissing) => {
                            format!(
                                "  ·  {}",
                                t!("Server copy missing", "서버 복사본 없음", "サーバー側なし")
                            )
                        }
                        Some(crate::open_subsonic::ServerPlaylistLinkHealth::UpToDate) | None
                            if playlist.access == ServerPlaylistAccess::Linked =>
                        {
                            format!("  ·  {}", t!("Linked", "연결됨", "リンク済み"))
                        }
                        _ => String::new(),
                    };
                    format!("☷ {}{count}{linked}", playlist.name)
                }
            }),
    }
}

fn song_text(song: &crate::open_subsonic::ServerSong) -> String {
    let duration = song.duration_secs.map_or_else(String::new, |seconds| {
        format!("  ({}:{:02})", seconds / 60, seconds % 60)
    });
    format!("♪ {} — {}{duration}", song.title, song.artist)
}

fn album_text(album: &crate::open_subsonic::ServerAlbum) -> String {
    let count = album.song_count.map_or_else(String::new, |count| {
        format!("  ·  {count} {}", t!("songs", "곡", "曲"))
    });
    format!("▣ {} — {}{count}", album.name, album.artist)
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::open_subsonic::{ServerLibraryPage, ServerPlaylistId, ServerPlaylistSummary};

    fn draw_server_library(app: &App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, app, frame.area()))
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn section_labels_exist_in_all_languages() {
        let _guard = crate::i18n::lock_for_test();
        let original = crate::i18n::current();
        for language in [
            crate::i18n::Language::English,
            crate::i18n::Language::Korean,
            crate::i18n::Language::Japanese,
        ] {
            crate::i18n::set_language(language);
            for section in [
                ServerLibrarySection::RecentlyPlayed,
                ServerLibrarySection::Albums,
                ServerLibrarySection::Artists,
                ServerLibrarySection::Songs,
                ServerLibrarySection::Playlists,
            ] {
                assert!(!section_label(section, false).is_empty());
                assert!(!section_label(section, true).is_empty());
            }
        }
        crate::i18n::set_language(original);
    }

    #[test]
    fn thirty_column_selector_shows_every_section_in_all_languages() {
        let _guard = crate::i18n::lock_for_test();
        let original = crate::i18n::current();
        let mut app = App::new(50);
        app.mode = crate::app::Mode::Library;
        app.server.library.source = LibrarySource::OpenSubsonic;

        for (language, labels) in [
            (
                crate::i18n::Language::English,
                ["Recent", "Albums", "Artists", "Songs", "Lists"],
            ),
            (
                crate::i18n::Language::Korean,
                ["최근", "앨범", "아티스트", "노래", "목록"],
            ),
            (
                crate::i18n::Language::Japanese,
                ["最近", "アルバム", "歌手", "曲", "リスト"],
            ),
        ] {
            crate::i18n::set_language(language);
            let text = draw_server_library(&app, 30, 30);
            // TestBackend retains the continuation cell after each double-width glyph as a
            // space. Compare whitespace-free text so CJK labels are checked as words.
            let comparable: String = text.chars().filter(|ch| !ch.is_whitespace()).collect();
            for label in labels {
                let label: String = label.chars().filter(|ch| !ch.is_whitespace()).collect();
                assert!(
                    comparable.contains(&label),
                    "{language:?} selector should contain {label:?}: {text:?}"
                );
            }
        }
        crate::i18n::set_language(original);
    }

    #[test]
    fn compact_section_rows_fit_the_twenty_eight_cell_inner_width() {
        let _guard = crate::i18n::lock_for_test();
        let original = crate::i18n::current();
        for language in [
            crate::i18n::Language::English,
            crate::i18n::Language::Korean,
            crate::i18n::Language::Japanese,
        ] {
            crate::i18n::set_language(language);
            for sections in [
                &[
                    ServerLibrarySection::RecentlyPlayed,
                    ServerLibrarySection::Albums,
                    ServerLibrarySection::Artists,
                ][..],
                &[ServerLibrarySection::Songs, ServerLibrarySection::Playlists][..],
            ] {
                let width = sections
                    .iter()
                    .copied()
                    .map(|section| buttons::text_width(section_label(section, true)))
                    .sum::<u16>()
                    .saturating_add(sections.len().saturating_sub(1) as u16);
                assert!(width <= 28, "{language:?} compact row is {width} cells");
            }
        }
        crate::i18n::set_language(original);
    }

    #[test]
    fn linked_playlist_marker_is_localized_and_absent_for_unlinked_rows() {
        let _guard = crate::i18n::lock_for_test();
        let original = crate::i18n::current();
        let mut app = App::new(50);
        app.server.library.page = Some(ServerLibraryPage {
            section: ServerLibrarySection::Playlists,
            rows: vec![ServerLibraryRow::Playlist(ServerPlaylistSummary {
                id: ServerPlaylistId::new("playlist").unwrap(),
                name: "Road Trip".to_owned(),
                owner: Some("owner".to_owned()),
                song_count: Some(2),
                duration_secs: None,
                public: Some(false),
                cover_art_id: None,
                access: ServerPlaylistAccess::Linked,
                link: None,
                readonly_evidence: Some(false),
                owner_evidence: Some("owner".to_owned()),
            })],
            next_offset: None,
            warning: None,
        });
        for (language, marker) in [
            (crate::i18n::Language::English, "Linked"),
            (crate::i18n::Language::Korean, "연결됨"),
            (crate::i18n::Language::Japanese, "リンク済み"),
        ] {
            crate::i18n::set_language(language);
            assert!(server_row_text(&app, 0).contains(marker));
        }
        if let Some(ServerLibraryRow::Playlist(playlist)) = app
            .server
            .library
            .page
            .as_mut()
            .and_then(|page| page.rows.first_mut())
        {
            playlist.access = ServerPlaylistAccess::Server;
        }
        assert!(!server_row_text(&app, 0).contains("リンク済み"));
        crate::i18n::set_language(original);
    }
}
