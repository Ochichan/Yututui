//! The library view: saved tracks, recent history, and local download-folder audio.
//!
//! Rows are formatted per frame. The underlying collections are bounded and only the
//! visible window is laid out by ratatui, so there's no need to pre-format into state.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::app::{App, LibraryTab, MouseTarget, ScrollSurface};
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
        Constraint::Length(1), // tab bar
        Constraint::Length(1), // spacer
        Constraint::Min(0),    // list
        Constraint::Length(1), // help
    ])
    .split(inner);

    let playlists_root = app.playlists_root();
    let library_rows = app.library_rows();

    render_tabs(frame, app, rows[0]);
    // The filter prompt rides the spacer row when active, so opening it never reflows the
    // list. At the Playlists root the match count is playlists, not songs.
    if app.library_ui.filter_editing || !app.library_ui.filter_query.is_empty() {
        let matches = if playlists_root {
            app.filtered_playlists().len()
        } else {
            library_rows.len()
        };
        render_filter(frame, app, rows[1], matches);
    } else if !app.status.text.is_empty() {
        // A transient status ("Added 2 tracks to …", the create-a-playlist nudge) rides the
        // spacer row so Library actions are visible without leaving the screen. It
        // auto-clears after STATUS_TTL via the global StatusTick, like the Search band — and
        // types itself in while the toast animation's window runs.
        if let Some(line) = crate::ui::anim::status_toast_line(app, rows[1].width) {
            frame.render_widget(Paragraph::new(line), rows[1]);
        } else {
            let role = match app.status.kind {
                crate::app::StatusKind::Error => R::Error,
                crate::app::StatusKind::Info => R::Success,
            };
            frame.render_widget(
                Paragraph::new(
                    Line::from(app.status.text.clone())
                        .style(app.theme.style(role))
                        .alignment(Alignment::Center),
                ),
                rows[1],
            );
        }
    } else if app.effective_library_tab() == LibraryTab::Playlists
        && app.library_ui.open_playlist.is_some()
    {
        // Inside an opened playlist the spacer row carries a clickable breadcrumb back
        // to the playlist list.
        render_playlist_breadcrumb(frame, app, rows[1]);
    }
    if playlists_root {
        render_playlist_list(frame, app, rows[2]);
    } else {
        render_list(frame, app, rows[2], &library_rows);
    }

    buttons::render_help_button(frame, app, rows[3]);
}

fn render_tabs(frame: &mut Frame, app: &App, area: Rect) {
    // Each tab is a click target. Walk left to right tracking the x cursor with the same
    // cell-width math ratatui lays the spans out with, so the hit rects line up exactly.
    let mut spans = Vec::new();
    let mut x = area.x;
    let tabs = app.library_tabs();
    let active_tab = if app.library_tab_available(app.library_ui.tab) {
        app.library_ui.tab
    } else {
        tabs[0]
    };
    let full_width: u16 = tabs
        .iter()
        .copied()
        .enumerate()
        .map(|(i, t)| {
            let label = format!(" {} ({}) ", t.label(), app.library_count_for(t));
            buttons::text_width(&label).saturating_add((i > 0) as u16 * 2)
        })
        .sum();
    let compact = full_width > area.width;
    for (i, t) in tabs.iter().copied().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
            x = x.saturating_add(2);
        }
        let tab_label = if compact {
            t.compact_label()
        } else {
            t.label()
        };
        let label = format!(" {} ({}) ", tab_label, app.library_count_for(t));
        let w = buttons::text_width(&label);
        let area_right = area.x.saturating_add(area.width);
        if x < area_right {
            app.register_mouse_button(
                Rect {
                    x,
                    y: area.y,
                    width: w.min(area_right.saturating_sub(x)),
                    height: 1,
                },
                MouseTarget::LibraryTab(t),
            );
        }
        x = x.saturating_add(w);
        let style = if active_tab == t {
            // A brief accent wash right after a tab/screen switch (identity when off).
            crate::ui::anim::active_tab_style(
                app,
                crate::ui::anim::TabPop::Inner,
                Style::default()
                    .fg(app.theme.color(R::SelectionFg))
                    .bg(app.theme.color(R::SelectionBg))
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            app.theme.style(R::TextMuted)
        };
        spans.push(Span::styled(label, style));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// The in-library filter prompt: `filter: <query>█  [N matches]` while typing, or
/// `filter: <query>  (Esc to clear)` once committed. Rendered borderless on the spacer row.
fn render_filter(frame: &mut Frame, app: &App, area: Rect, matches: usize) {
    let editing = app.library_ui.filter_editing;
    let query = &app.library_ui.filter_query;

    let mut spans = vec![
        Span::styled(t!("filter: ", "필터: "), app.theme.style(R::TextMuted)),
        Span::styled(query.clone(), app.theme.style(R::TextPrimary)),
    ];
    if editing {
        spans.push(crate::ui::anim::caret_span(
            app,
            app.theme.style(R::Accent),
            app.theme.color(R::Background),
        ));
    }
    let hint = if matches == 0 {
        t!("  (no matches)", "  (일치 없음)").to_owned()
    } else if editing {
        if crate::i18n::is_korean() {
            format!("  [{matches}개]")
        } else {
            format!("  [{matches} matches]")
        }
    } else {
        t!("  (Esc to clear)", "  (Esc: 지우기)").to_owned()
    };
    spans.push(Span::styled(hint, app.theme.style(R::TextMuted)));
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_list(frame: &mut Frame, app: &App, area: Rect, rows: &[&crate::api::Song]) {
    // Record the viewport height so PageUp/PageDown can move by a screenful (see app::page_step).
    app.bridges.list_viewport_rows.set(area.height);

    if rows.is_empty() {
        // A filtered-to-nothing list gets a filter-specific message, not the per-tab "empty" hint.
        let msg: String = if !app.library_ui.filter_query.is_empty() {
            if crate::i18n::is_korean() {
                format!("'{}' 와 일치하는 곡이 없어요.", app.library_ui.filter_query)
            } else {
                format!("No tracks match \"{}\".", app.library_ui.filter_query)
            }
        } else if app.library_ui.tab == LibraryTab::Playlists {
            // The root level never reaches here (it renders via `render_playlist_list`),
            // so this is an opened-but-empty playlist. Built from the live keymap so a
            // rebind updates the hint in lock-step.
            let add = app.keymap.label_for_display(
                crate::keymap::KeyContext::Library,
                crate::keymap::Action::AddToPlaylist,
                app.retro_mode(),
            );
            let gem = app.keymap.label_for_display(
                crate::keymap::KeyContext::Playlists,
                crate::keymap::Action::OpenAi,
                app.retro_mode(),
            );
            if crate::i18n::is_korean() {
                format!(
                    "빈 플레이리스트예요 — 다른 목록에서 {add} 로 곡을 추가하거나 DJ Gem({gem})에게 부탁하세요."
                )
            } else {
                format!(
                    "This playlist is empty — press {add} on any song to add it here, or ask DJ Gem ({gem})."
                )
            }
        } else {
            match app.library_ui.tab {
                LibraryTab::All => t!(
                    "No library tracks yet — play, favorite, or download something.",
                    "아직 라이브러리에 곡이 없어요 — 재생하거나 즐겨찾기하거나 다운로드해 보세요."
                ),
                LibraryTab::Favorites => t!(
                    "No favorites yet — press f on a track to save it.",
                    "즐겨찾기가 없어요 — 곡에서 f 를 눌러 저장하세요."
                ),
                LibraryTab::History => {
                    t!(
                        "No history yet — play something.",
                        "재생 기록이 없어요 — 뭐든 재생해 보세요."
                    )
                }
                LibraryTab::RadioFavorites => t!(
                    "No radio favorites yet — press f on a Radio Browser station.",
                    "라디오 즐겨찾기가 없어요 — Radio Browser 방송에서 f 를 눌러 저장하세요."
                ),
                LibraryTab::Radio => t!(
                    "No recent radio stations yet — play one from Radio Browser search.",
                    "최근 라디오가 없어요 — Radio Browser 검색에서 재생해 보세요."
                ),
                LibraryTab::Downloads => t!(
                    "No downloaded tracks found in the download folder.",
                    "다운로드 폴더에 받은 곡이 없어요."
                ),
                // Handled by the keymap-aware branch above; benign fallback for exhaustiveness.
                LibraryTab::Playlists => t!("This playlist is empty.", "빈 플레이리스트예요."),
            }
            .to_owned()
        };
        frame.render_widget(
            Paragraph::new(Line::from(msg).style(app.theme.style(R::TextMuted))),
            area,
        );
        return;
    }

    if area.height == 0 || area.width < 4 {
        return;
    }

    // Rows are rendered by hand (like the queue window) so a mouse drag can highlight the
    // inclusive anchor..=cursor range and each deletable row can carry a trailing ✗ button.
    // The All tab is an aggregate view (a track may live in several tabs at once), so it has
    // no per-row delete — manage tracks from their own Favorites/History/Downloads tab.
    let len = rows.len();
    let cursor = app.library_ui.selected.min(len - 1);
    let sel_lo = cursor.min(app.library_ui.anchor);
    let sel_hi = cursor.max(app.library_ui.anchor);
    let deletable = app.library_ui.tab != LibraryTab::All;
    let del_w: u16 = if deletable { 2 } else { 0 };

    // The wheel scrolls this viewport freely (decoupled from the cursor); the render only
    // nudges it to keep a keyboard-moved cursor on-screen with a margin — see `ui::scroll`.
    let visible = area.height as usize;
    let start =
        app.bridges
            .library_scroll
            .resolve(cursor, area.height, len, crate::ui::scroll::SCROLLOFF);

    let body_w = area.width.saturating_sub(del_w) as usize;
    for (vis, (i, song)) in rows
        .iter()
        .enumerate()
        .skip(start)
        .take(visible)
        .enumerate()
    {
        let y = area.y + vis as u16;
        let selected = i >= sel_lo && i <= sel_hi;
        let marker = if i == cursor { "▶ " } else { "  " };
        let heart = if app.library.is_favorite(&song.video_id) {
            "♥ "
        } else {
            "  "
        };
        let title = app.display_title(song);
        let artist = app.display_artist(song);
        let text = if song.duration.is_empty() {
            format!("{title} — {artist}")
        } else {
            format!("{title} — {artist}  ({})", song.duration)
        };
        // The cursor row marquees when clipped (the 4-cell marker+heart gutter stays
        // put) so the full text stays readable even in a sliver-narrow window; every
        // other row hard-truncates as before.
        let text = if i == cursor {
            crate::ui::anim::selected_marquee(
                app,
                ScrollSurface::Library,
                i,
                &text,
                body_w.saturating_sub(5),
            )
        } else {
            text
        };
        let body = crate::ui::text::truncate_owned_to_width(
            format!("{marker}{heart}{text}"),
            body_w.saturating_sub(1),
        );

        let base = if selected {
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
        // Rows cascade in top-to-bottom right after a tab/view switch (identity when off).
        let base = crate::ui::anim::stagger_style(app, crate::app::Mode::Library, vis, base);
        let row = Rect {
            x: area.x,
            y,
            width: area.width,
            height: 1,
        };
        frame.render_widget(Paragraph::new(Line::from(body).style(base)), row);

        // The row body selects (single-click) / plays (double-click).
        let body_rect = Rect {
            x: row.x,
            y,
            width: row.width.saturating_sub(del_w),
            height: 1,
        };
        app.register_mouse_button(body_rect, MouseTarget::ListRow(i));

        if deletable {
            // Trailing ✗ delete button, kept on the row's highlight when selected.
            let del_x = row.x + row.width.saturating_sub(del_w);
            let del_rect = Rect {
                x: del_x,
                y,
                width: del_w,
                height: 1,
            };
            let mut del_style = app.theme.style(R::Error);
            if selected {
                del_style = del_style.bg(app.theme.color(R::SelectionBg));
            }
            frame.render_widget(Paragraph::new(Line::from("✗").style(del_style)), del_rect);
            app.register_mouse_button(del_rect, MouseTarget::LibraryDel(i));
        }
    }

    // Scrollbar on the right border, tracking the viewport position; hidden when the list fits.
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

/// The Playlists tab's root level: one row per local playlist (`▶ ♪ Name — N tracks`),
/// cursor-only highlight (no multi-select range — playlist actions are single-row), a
/// trailing ✗ that asks before deleting the playlist, and the shared Library scrollbar.
fn render_playlist_list(frame: &mut Frame, app: &App, area: Rect) {
    // Record the viewport height so PageUp/PageDown can move by a screenful (see app::page_step).
    app.bridges.list_viewport_rows.set(area.height);

    let rows = app.filtered_playlists();
    if rows.is_empty() {
        let msg: String = if !app.library_ui.filter_query.is_empty() {
            if crate::i18n::is_korean() {
                format!(
                    "'{}' 와 일치하는 플레이리스트가 없어요.",
                    app.library_ui.filter_query
                )
            } else {
                format!("No playlists match \"{}\".", app.library_ui.filter_query)
            }
        } else {
            // Built from the live keymap so a rebind updates the hint in lock-step.
            let key = app.keymap.label_for_display(
                crate::keymap::KeyContext::Playlists,
                crate::keymap::Action::PlaylistCreate,
                app.retro_mode(),
            );
            if crate::i18n::is_korean() {
                format!("아직 플레이리스트가 없어요 — {key} 로 만들거나 Spotify에서 가져오세요.")
            } else {
                format!("No playlists yet — press {key} to create one, or import from Spotify.")
            }
        };
        frame.render_widget(
            Paragraph::new(Line::from(msg).style(app.theme.style(R::TextMuted))),
            area,
        );
        return;
    }

    if area.height == 0 || area.width < 4 {
        return;
    }

    let len = rows.len();
    let cursor = app.library_ui.selected.min(len - 1);
    let del_w: u16 = 2;
    let visible = area.height as usize;
    let start =
        app.bridges
            .library_scroll
            .resolve(cursor, area.height, len, crate::ui::scroll::SCROLLOFF);

    let body_w = area.width.saturating_sub(del_w) as usize;
    for (vis, (i, playlist)) in rows
        .iter()
        .enumerate()
        .skip(start)
        .take(visible)
        .enumerate()
    {
        let y = area.y + vis as u16;
        let selected = i == cursor;
        let marker = if selected { "▶ " } else { "  " };
        let count = playlist.songs.len();
        let text = if crate::i18n::is_korean() {
            format!("♪ {} — {count}곡", playlist.name)
        } else {
            let noun = if count == 1 { "track" } else { "tracks" };
            format!("♪ {} — {count} {noun}", playlist.name)
        };
        // Long playlist names on the cursor row marquee like the track lists do.
        let text = if selected {
            crate::ui::anim::selected_marquee(
                app,
                ScrollSurface::Library,
                i,
                &text,
                body_w.saturating_sub(3),
            )
        } else {
            text
        };
        let body = crate::ui::text::truncate_owned_to_width(
            format!("{marker}{text}"),
            body_w.saturating_sub(1),
        );

        let base = if selected {
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
        // Playlist rows cascade too (same Library reveal window as the track lists).
        let base = crate::ui::anim::stagger_style(app, crate::app::Mode::Library, vis, base);
        let row = Rect {
            x: area.x,
            y,
            width: area.width,
            height: 1,
        };
        frame.render_widget(Paragraph::new(Line::from(body).style(base)), row);

        // The row body selects (single-click) / opens (double-click).
        let body_rect = Rect {
            x: row.x,
            y,
            width: row.width.saturating_sub(del_w),
            height: 1,
        };
        app.register_mouse_button(body_rect, MouseTarget::ListRow(i));

        // Trailing ✗: asks before deleting the whole playlist.
        let del_rect = Rect {
            x: row.x + row.width.saturating_sub(del_w),
            y,
            width: del_w,
            height: 1,
        };
        let mut del_style = app.theme.style(R::Error);
        if selected {
            del_style = del_style.bg(app.theme.color(R::SelectionBg));
        }
        frame.render_widget(Paragraph::new(Line::from("✗").style(del_style)), del_rect);
        app.register_mouse_button(del_rect, MouseTarget::LibraryDel(i));
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

/// The opened-playlist breadcrumb on the spacer row: `⟵ Name — N tracks  (q: back)`.
/// Clicking it returns to the playlist list (same as Back).
fn render_playlist_breadcrumb(frame: &mut Frame, app: &App, area: Rect) {
    let Some(playlist) = app
        .library_ui
        .open_playlist
        .as_ref()
        .and_then(|key| app.playlists.find(key))
    else {
        return;
    };
    let back_key = app.keymap.label_for_display(
        crate::keymap::KeyContext::Playlists,
        crate::keymap::Action::Back,
        app.retro_mode(),
    );
    let count = playlist.songs.len();
    let text = if crate::i18n::is_korean() {
        format!("⟵ {} — {count}곡  ({back_key}: 뒤로)", playlist.name)
    } else {
        let noun = if count == 1 { "track" } else { "tracks" };
        format!("⟵ {} — {count} {noun}  ({back_key}: back)", playlist.name)
    };
    let w = buttons::text_width(&text).min(area.width);
    frame.render_widget(
        Paragraph::new(Line::from(text).style(app.theme.style(R::TextMuted))),
        area,
    );
    app.register_mouse_button(
        Rect {
            x: area.x,
            y: area.y,
            width: w,
            height: 1,
        },
        MouseTarget::PlaylistBack,
    );
}

/// The create-playlist popup ("new window", per UX request): a centered card with a name
/// input, committed with Enter / the Create button, cancelled with Esc / Cancel / a click
/// outside. Buttons publish `ConfirmPlaylistCreate` / `CancelPlaylistCreate` hit rects.
pub fn render_playlist_create(frame: &mut Frame, app: &App, area: Rect) {
    let Some(name) = app.library_ui.create_input.as_ref() else {
        return;
    };
    let popup = centered_fixed(area, 56, 8);
    crate::ui::render_popup_background(frame, app, popup);

    let block = Block::default()
        .title(t!(" ♪ New playlist ", " ♪ 새 플레이리스트 "))
        .borders(Borders::ALL)
        .border_style(crate::ui::popup_style(app, R::Accent).add_modifier(Modifier::BOLD))
        .style(crate::ui::popup_style(app, R::TextPrimary));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let rows = Layout::vertical([
        Constraint::Length(1), // spacer
        Constraint::Length(1), // input
        Constraint::Length(1), // spacer
        Constraint::Min(1),    // buttons
    ])
    .split(inner);

    // `name: <typed>█` — the filter prompt's visual language, inside the popup.
    let shown = crate::ui::text::truncate_owned_to_width(
        name.clone(),
        (rows[1].width as usize).saturating_sub(12),
    );
    let input = Line::from(vec![
        Span::styled(
            t!("  name: ", "  이름: "),
            crate::ui::popup_style(app, R::TextMuted),
        ),
        Span::styled(shown, crate::ui::popup_style(app, R::TextPrimary)),
        crate::ui::anim::caret_span(
            app,
            crate::ui::popup_style(app, R::Accent),
            crate::ui::popup_bg(app),
        ),
    ]);
    frame.render_widget(Paragraph::new(input), rows[1]);

    let segs = [
        buttons::Seg::button(
            MouseTarget::ConfirmPlaylistCreate,
            t!(" Create (Enter) ", " 만들기 (Enter) "),
        ),
        buttons::Seg::label("    "),
        buttons::Seg::button(
            MouseTarget::CancelPlaylistCreate,
            t!(" Cancel (Esc) ", " 취소 (Esc) "),
        ),
    ];
    buttons::render_segments(
        frame,
        app,
        rows[3],
        &segs,
        crate::ui::popup_style(app, R::Accent).add_modifier(Modifier::BOLD),
        crate::ui::popup_style(app, R::Accent).add_modifier(Modifier::BOLD),
        Alignment::Center,
    );
    crate::ui::seal_popup_background(frame, app, popup);
    crate::ui::mark_art_rows_for_popup(frame, app, popup);
}

/// The "add to playlist" picker popup, drawn over whichever screen opened it: pick an
/// existing playlist (or the trailing "New playlist…" row, which flips to an inline name
/// entry) for the pending song(s). Rows publish `PlaylistPickRow` hit rects; the name
/// entry publishes `ConfirmPickerCreate` / `CancelPickerCreate`.
pub fn render_playlist_picker(frame: &mut Frame, app: &App, area: Rect) {
    let Some(picker) = app.playlist_picker.as_ref() else {
        return;
    };
    let playlists = app.playlists.list();
    let rows_total = playlists.len() + 1; // + "New playlist…"
    let visible = rows_total.min(8);
    let h = (visible as u16 + 6).min(area.height);
    let popup = centered_fixed(area, 56, h);
    crate::ui::render_popup_background(frame, app, popup);

    let block = Block::default()
        .title(t!(" ♪ Add to playlist ", " ♪ 플레이리스트에 추가 "))
        .borders(Borders::ALL)
        .border_style(crate::ui::popup_style(app, R::Accent).add_modifier(Modifier::BOLD))
        .style(crate::ui::popup_style(app, R::TextPrimary));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let rows = Layout::vertical([
        Constraint::Length(1), // what's being added
        Constraint::Min(1),    // list / name entry
        Constraint::Length(1), // hint / buttons
    ])
    .split(inner);

    // "adding: <title>" for one song, "adding: N tracks" for a selection.
    let subject = if picker.songs.len() == 1 {
        app.display_title(&picker.songs[0]).into_owned()
    } else if crate::i18n::is_korean() {
        format!("{}곡", picker.songs.len())
    } else {
        format!("{} tracks", picker.songs.len())
    };
    let subject = crate::ui::text::truncate_owned_to_width(
        format!("{}{subject}", t!("  adding: ", "  추가할 곡: ")),
        inner.width as usize,
    );
    frame.render_widget(
        Paragraph::new(Line::from(subject).style(crate::ui::popup_style(app, R::TextMuted))),
        rows[0],
    );

    if let Some(name) = picker.naming.as_ref() {
        // Phase two: the inline new-playlist name entry.
        let shown = crate::ui::text::truncate_owned_to_width(
            name.clone(),
            (rows[1].width as usize).saturating_sub(12),
        );
        let input = Line::from(vec![
            Span::styled(
                t!("  name: ", "  이름: "),
                crate::ui::popup_style(app, R::TextMuted),
            ),
            Span::styled(shown, crate::ui::popup_style(app, R::TextPrimary)),
            crate::ui::anim::caret_span(
                app,
                crate::ui::popup_style(app, R::Accent),
                crate::ui::popup_bg(app),
            ),
        ]);
        frame.render_widget(Paragraph::new(input), rows[1]);

        let segs = [
            buttons::Seg::button(
                MouseTarget::ConfirmPickerCreate,
                t!(" Create + add (Enter) ", " 만들고 추가 (Enter) "),
            ),
            buttons::Seg::label("    "),
            buttons::Seg::button(
                MouseTarget::CancelPickerCreate,
                t!(" Back (Esc) ", " 뒤로 (Esc) "),
            ),
        ];
        buttons::render_segments(
            frame,
            app,
            rows[2],
            &segs,
            crate::ui::popup_style(app, R::Accent).add_modifier(Modifier::BOLD),
            crate::ui::popup_style(app, R::Accent).add_modifier(Modifier::BOLD),
            Alignment::Center,
        );
    } else {
        // Phase one: the playlist list (+ trailing "New playlist…" row), cursor-windowed.
        let list_h = rows[1].height as usize;
        let start = picker.cursor.saturating_sub(list_h.saturating_sub(1));
        for (vis, i) in (start..rows_total).take(list_h).enumerate() {
            let y = rows[1].y + vis as u16;
            let selected = i == picker.cursor;
            let marker = if selected { "▶ " } else { "  " };
            let body = if i < playlists.len() {
                let p = &playlists[i];
                let count = p.songs.len();
                if crate::i18n::is_korean() {
                    format!("{marker}♪ {} — {count}곡", p.name)
                } else {
                    let noun = if count == 1 { "track" } else { "tracks" };
                    format!("{marker}♪ {} — {count} {noun}", p.name)
                }
            } else {
                format!("{marker}{}", t!("＋ New playlist…", "＋ 새 플레이리스트…"))
            };
            let body = crate::ui::text::truncate_owned_to_width(body, rows[1].width as usize);
            let style = if selected {
                crate::ui::anim::selection_style(
                    app,
                    Style::default()
                        .fg(app.theme.color(R::SelectionFg))
                        .bg(app.theme.color(R::SelectionBg))
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                crate::ui::popup_style(app, R::TextPrimary)
            };
            let row = Rect {
                x: rows[1].x,
                y,
                width: rows[1].width,
                height: 1,
            };
            frame.render_widget(Paragraph::new(Line::from(body).style(style)), row);
            app.register_mouse_button(row, MouseTarget::PlaylistPickRow(i));
        }
        frame.render_widget(
            Paragraph::new(t!(
                "↑↓ move · Enter add · n new · Esc close",
                "↑↓ 이동 · Enter 추가 · n 새로 만들기 · Esc 닫기"
            ))
            .alignment(Alignment::Center)
            .style(crate::ui::popup_style(app, R::TextMuted)),
            rows[2],
        );
    }
    crate::ui::seal_popup_background(frame, app, popup);
    crate::ui::mark_art_rows_for_popup(frame, app, popup);
}

/// A modal confirming deletion of a whole playlist. The tracks themselves are untouched
/// (they may still live in favorites/history/downloads), but the list is gone at once, so
/// it's gated like the download delete — Enter/`y`/Delete button confirm, anything else
/// cancels. Buttons publish `ConfirmPlaylistDelete` / `CancelPlaylistDelete` hit rects.
pub fn render_confirm_playlist_delete(frame: &mut Frame, app: &App, area: Rect) {
    let Some(key) = app.library_ui.confirm_playlist_delete.as_ref() else {
        return;
    };
    let (name, count) = app
        .playlists
        .find(key)
        .map(|p| (p.name.clone(), p.songs.len()))
        .unwrap_or_else(|| (key.clone(), 0));
    let popup = centered_fixed(area, 56, 9);
    crate::ui::render_popup_background(frame, app, popup);

    let block = Block::default()
        .title(t!(" ⚠ Delete playlist ", " ⚠ 플레이리스트 삭제 "))
        .borders(Borders::ALL)
        .border_style(crate::ui::popup_style(app, R::Error).add_modifier(Modifier::BOLD))
        .style(crate::ui::popup_style(app, R::TextPrimary));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let rows = Layout::vertical([
        Constraint::Length(1), // spacer
        Constraint::Length(1), // prompt
        Constraint::Length(1), // detail
        Constraint::Length(1), // spacer
        Constraint::Min(1),    // buttons
    ])
    .split(inner);

    let prompt = if crate::i18n::is_korean() {
        format!("'{name}' ({count}곡) 플레이리스트를 삭제할까요?")
    } else {
        let noun = if count == 1 { "track" } else { "tracks" };
        format!("Delete \"{name}\" ({count} {noun})?")
    };
    let prompt = crate::ui::text::truncate_owned_to_width(prompt, inner.width as usize);
    frame.render_widget(
        Paragraph::new(prompt)
            .alignment(Alignment::Center)
            .style(crate::ui::popup_style(app, R::TextPrimary)),
        rows[1],
    );
    frame.render_widget(
        Paragraph::new(t!(
            "The tracks themselves are not deleted.",
            "곡 자체는 삭제되지 않아요."
        ))
        .alignment(Alignment::Center)
        .style(crate::ui::popup_style(app, R::TextMuted)),
        rows[2],
    );

    let segs = [
        buttons::Seg::button(
            MouseTarget::ConfirmPlaylistDelete,
            t!(" Delete (Enter) ", " 삭제 (Enter) "),
        ),
        buttons::Seg::label("    "),
        buttons::Seg::button(
            MouseTarget::CancelPlaylistDelete,
            t!(" Cancel (Esc) ", " 취소 (Esc) "),
        ),
    ];
    buttons::render_segments(
        frame,
        app,
        rows[4],
        &segs,
        crate::ui::popup_style(app, R::Error).add_modifier(Modifier::BOLD),
        crate::ui::popup_style(app, R::Accent).add_modifier(Modifier::BOLD),
        Alignment::Center,
    );
    crate::ui::seal_popup_background(frame, app, popup);
    crate::ui::mark_art_rows_for_popup(frame, app, popup);
}

/// A modal confirming deletion of downloaded files from disk. Deleting a real file is
/// irreversible, so it's gated behind an explicit choice — Enter/`y` or the Delete button
/// confirm, Esc or a click elsewhere cancels. The buttons publish `ConfirmDelete` /
/// `CancelDelete` hit rects so the modal is fully usable by mouse as well as keyboard.
pub fn render_confirm_delete(frame: &mut Frame, app: &App, area: Rect) {
    let count = app.library_ui.confirm_delete.as_ref().map_or(0, Vec::len);
    let popup = centered_fixed(area, 56, 9);
    crate::ui::render_popup_background(frame, app, popup);

    let block = Block::default()
        .title(t!(
            " ⚠ Delete downloaded files ",
            " ⚠ 다운로드한 파일 삭제 "
        ))
        .borders(Borders::ALL)
        .border_style(crate::ui::popup_style(app, R::Error).add_modifier(Modifier::BOLD))
        .style(crate::ui::popup_style(app, R::TextPrimary));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let rows = Layout::vertical([
        Constraint::Length(1), // spacer
        Constraint::Length(1), // prompt
        Constraint::Length(1), // detail
        Constraint::Length(1), // spacer
        Constraint::Min(1),    // buttons
    ])
    .split(inner);

    let prompt = if crate::i18n::is_korean() {
        // Korean has no plural form, so the count carries the quantity on its own.
        format!("다운로드한 파일 {count}개를 디스크에서 삭제할까요?")
    } else {
        let noun = if count == 1 { "file" } else { "files" };
        format!("Delete {count} downloaded {noun} from disk?")
    };
    frame.render_widget(
        Paragraph::new(prompt)
            .alignment(Alignment::Center)
            .style(crate::ui::popup_style(app, R::TextPrimary)),
        rows[1],
    );
    frame.render_widget(
        Paragraph::new(t!(
            "This permanently removes the actual files.",
            "실제 파일이 영구적으로 삭제됩니다."
        ))
        .alignment(Alignment::Center)
        .style(crate::ui::popup_style(app, R::TextMuted)),
        rows[2],
    );

    let segs = [
        buttons::Seg::button(
            MouseTarget::ConfirmDelete,
            t!(" Delete (Enter) ", " 삭제 (Enter) "),
        ),
        buttons::Seg::label("    "),
        buttons::Seg::button(
            MouseTarget::CancelDelete,
            t!(" Cancel (Esc) ", " 취소 (Esc) "),
        ),
    ];
    buttons::render_segments(
        frame,
        app,
        rows[4],
        &segs,
        crate::ui::popup_style(app, R::Error).add_modifier(Modifier::BOLD),
        crate::ui::popup_style(app, R::Accent).add_modifier(Modifier::BOLD),
        Alignment::Center,
    );
    crate::ui::seal_popup_background(frame, app, popup);
    crate::ui::mark_art_rows_for_popup(frame, app, popup);
}

/// A modal confirming a bulk download ("Download N songs?"), raised by a drag-selected range
/// (`d`) or a whole list/playlist (`Shift+D`). The batch is already deduped, so `N` is the real
/// number of fetches. Enter/`y` or the Download button confirm; Esc or a click elsewhere
/// cancels. Publishes `ConfirmDownload` / `CancelDownload` hit rects for mouse use.
pub fn render_confirm_download(frame: &mut Frame, app: &App, area: Rect) {
    let count = app.library_ui.confirm_download.as_ref().map_or(0, Vec::len);
    let popup = centered_fixed(area, 56, 9);
    crate::ui::render_popup_background(frame, app, popup);

    let block = Block::default()
        .title(t!(" ⬇ Download songs ", " ⬇ 곡 다운로드 "))
        .borders(Borders::ALL)
        .border_style(crate::ui::popup_style(app, R::Accent).add_modifier(Modifier::BOLD))
        .style(crate::ui::popup_style(app, R::TextPrimary));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let rows = Layout::vertical([
        Constraint::Length(1), // spacer
        Constraint::Length(1), // prompt
        Constraint::Length(1), // detail
        Constraint::Length(1), // spacer
        Constraint::Min(1),    // buttons
    ])
    .split(inner);

    let prompt = if crate::i18n::is_korean() {
        format!("{count}곡을 다운로드할까요?")
    } else {
        let noun = if count == 1 { "song" } else { "songs" };
        format!("Download {count} {noun}?")
    };
    frame.render_widget(
        Paragraph::new(prompt)
            .alignment(Alignment::Center)
            .style(crate::ui::popup_style(app, R::TextPrimary)),
        rows[1],
    );
    frame.render_widget(
        Paragraph::new(t!(
            "Already-downloaded tracks are skipped.",
            "이미 받은 곡은 제외됩니다."
        ))
        .alignment(Alignment::Center)
        .style(crate::ui::popup_style(app, R::TextMuted)),
        rows[2],
    );

    let segs = [
        buttons::Seg::button(
            MouseTarget::ConfirmDownload,
            t!(" Download (Enter) ", " 다운로드 (Enter) "),
        ),
        buttons::Seg::label("    "),
        buttons::Seg::button(
            MouseTarget::CancelDownload,
            t!(" Cancel (Esc) ", " 취소 (Esc) "),
        ),
    ];
    buttons::render_segments(
        frame,
        app,
        rows[4],
        &segs,
        crate::ui::popup_style(app, R::Accent).add_modifier(Modifier::BOLD),
        crate::ui::popup_style(app, R::Accent).add_modifier(Modifier::BOLD),
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
