//! Local Deck shell rendered under the Library mode.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::app::{App, LocalModeConfirm, LocalSection, MouseTarget, ScrollSurface};
use crate::t;
use crate::theme::ThemeRole as R;
use crate::ui::buttons;

pub fn render(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(app.theme.style(R::BorderFocused))
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
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .split(inner);

    render_header(frame, app, rows[0]);
    render_status(frame, app, rows[1]);
    render_body(frame, app, rows[2]);
    buttons::render_help_button(frame, app, rows[3]);
}

fn render_header(frame: &mut Frame, app: &App, area: Rect) {
    let indexed = app.local_mode.index.index.tracks().len();
    let count = indexed.max(app.library_ui.downloaded.len());
    let count_label = if count == 1 {
        t!("1 track", "1곡").to_owned()
    } else if crate::i18n::is_korean() {
        format!("{count}곡")
    } else {
        format!("{count} tracks")
    };
    let scan_label = if app.local_mode.index.scanning {
        t!("scan: running", "스캔: 진행 중")
    } else if app.local_mode.index.loading {
        t!("scan: loading", "스캔: 로드 중")
    } else {
        t!("scan: idle", "스캔: 대기")
    };
    let section = match app.local_mode.ui.section {
        LocalSection::Home => t!("Home", "홈"),
        LocalSection::Tracks => t!("Tracks", "곡"),
    };
    let line = Line::from(vec![
        Span::styled(
            " LOCAL DECK ",
            app.theme.style(R::Accent).add_modifier(Modifier::BOLD),
        ),
        Span::styled("  ", app.theme.style(R::TextMuted)),
        Span::styled(section, app.theme.style(R::TextPrimary)),
        Span::styled("  -  ", app.theme.style(R::TextMuted)),
        Span::styled(count_label, app.theme.style(R::TextMuted)),
        Span::styled("  -  ", app.theme.style(R::TextMuted)),
        Span::styled(scan_label, app.theme.style(R::TextMuted)),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn render_status(frame: &mut Frame, app: &App, area: Rect) {
    let text = if app.local_mode.index.loading {
        t!(
            "Loading the Local Deck index...",
            "로컬 덱 인덱스를 불러오는 중..."
        )
        .to_owned()
    } else if app.local_mode.index.scanning {
        t!(
            "Scanning the download folder for local audio...",
            "다운로드 폴더의 로컬 오디오를 스캔 중..."
        )
        .to_owned()
    } else if let Some(summary) = &app.local_mode.index.last_summary {
        if summary.errors > 0 {
            format!(
                "{}: {} {} (+{} / ~{} / -{} / {} {})",
                t!("Last scan", "마지막 스캔"),
                summary.indexed,
                t!("tracks", "곡"),
                summary.added,
                summary.changed,
                summary.removed,
                summary.errors,
                t!("errors", "오류")
            )
        } else {
            format!(
                "{}: {} {} (+{} / ~{} / -{})",
                t!("Last scan", "마지막 스캔"),
                summary.indexed,
                t!("tracks", "곡"),
                summary.added,
                summary.changed,
                summary.removed
            )
        }
    } else if !app.local_mode.index.index.is_empty() {
        t!(
            "Indexed local audio. Press r to rescan or R for a full rebuild.",
            "로컬 오디오 인덱스 사용 중. r로 재스캔, R로 전체 재빌드."
        )
        .to_owned()
    } else if app.library_ui.downloaded.is_empty() {
        t!(
            "No local downloads indexed yet - press r to scan the download folder.",
            "아직 로컬 다운로드가 없어요 - r로 다운로드 폴더를 스캔하세요."
        )
        .to_owned()
    } else {
        t!(
            "Showing downloaded audio until the Local Deck index is built.",
            "로컬 덱 인덱스가 만들어질 때까지 다운로드 오디오를 표시합니다."
        )
        .to_owned()
    };
    frame.render_widget(
        Paragraph::new(Line::from(text).style(app.theme.style(R::TextMuted))),
        area,
    );
}

fn render_body(frame: &mut Frame, app: &App, area: Rect) {
    if area.height == 0 || area.width < 4 {
        return;
    }
    match app.local_mode.ui.section {
        LocalSection::Home => render_home(frame, app, area),
        LocalSection::Tracks => render_tracks(frame, app, area),
    }
}

fn render_home(frame: &mut Frame, app: &App, area: Rect) {
    let lines = if app.library_ui.downloaded.is_empty() {
        vec![
            Line::from(t!(
                "Local music has not been indexed yet.",
                "아직 로컬 음악을 인덱싱하지 않았어요."
            )),
            Line::from(""),
            Line::from(t!(
                "The first scanner milestone will add music folders and recursive indexing.",
                "첫 스캐너 단계에서 음악 폴더와 재귀 인덱싱을 추가합니다."
            )),
        ]
    } else {
        vec![
            Line::from(t!(
                "Downloaded local audio is ready.",
                "다운로드된 로컬 오디오를 탐색할 수 있어요."
            )),
            Line::from(""),
            Line::from(t!(
                "Press Enter to open Tracks.",
                "Enter로 곡 목록을 여세요."
            )),
        ]
    };
    frame.render_widget(
        Paragraph::new(lines)
            .alignment(Alignment::Center)
            .style(app.theme.style(R::TextMuted)),
        area,
    );
}

fn render_tracks(frame: &mut Frame, app: &App, area: Rect) {
    if !app.local_mode.index.index.is_empty() {
        render_indexed_tracks(frame, app, area);
        return;
    }
    render_download_seed_tracks(frame, app, area);
}

fn render_indexed_tracks(frame: &mut Frame, app: &App, area: Rect) {
    let rows = app.local_mode.index.index.tracks();
    let len = rows.len();
    let cursor = app.local_mode.ui.selected.min(len - 1);
    let visible = area.height as usize;
    let start =
        app.bridges
            .library_scroll
            .resolve(cursor, area.height, len, crate::ui::scroll::SCROLLOFF);
    let body_w = area.width.saturating_sub(1) as usize;

    for (vis, (i, track)) in rows
        .iter()
        .enumerate()
        .skip(start)
        .take(visible)
        .enumerate()
    {
        let y = area.y + vis as u16;
        let selected = i == cursor;
        let marker = if selected { "> " } else { "  " };
        let song = track.to_song();
        let title = app.display_title(&song);
        let artist = app.display_artist(&song);
        let text = if song.duration.is_empty() {
            format!("{title} - {artist}")
        } else {
            format!("{title} - {artist}  ({})", song.duration)
        };
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
        let style = if selected {
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
        let row = Rect {
            x: area.x,
            y,
            width: area.width.saturating_sub(1),
            height: 1,
        };
        frame.render_widget(Paragraph::new(Line::from(body).style(style)), row);
        app.register_mouse_button(row, MouseTarget::LocalRow(i));
    }

    buttons::render_list_scrollbar(
        frame,
        app,
        Rect {
            x: area.right().saturating_sub(1),
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

fn render_download_seed_tracks(frame: &mut Frame, app: &App, area: Rect) {
    let rows = &app.library_ui.downloaded;
    if rows.is_empty() {
        render_home(frame, app, area);
        return;
    }

    let len = rows.len();
    let cursor = app.local_mode.ui.selected.min(len - 1);
    let visible = area.height as usize;
    let start =
        app.bridges
            .library_scroll
            .resolve(cursor, area.height, len, crate::ui::scroll::SCROLLOFF);
    let body_w = area.width.saturating_sub(1) as usize;

    for (vis, (i, song)) in rows
        .iter()
        .enumerate()
        .skip(start)
        .take(visible)
        .enumerate()
    {
        let y = area.y + vis as u16;
        let selected = i == cursor;
        let marker = if selected { "> " } else { "  " };
        let title = app.display_title(song);
        let artist = app.display_artist(song);
        let text = if song.duration.is_empty() {
            format!("{title} - {artist}")
        } else {
            format!("{title} - {artist}  ({})", song.duration)
        };
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
        let style = if selected {
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
        let row = Rect {
            x: area.x,
            y,
            width: area.width.saturating_sub(1),
            height: 1,
        };
        frame.render_widget(Paragraph::new(Line::from(body).style(style)), row);
        app.register_mouse_button(row, MouseTarget::LocalRow(i));
    }

    buttons::render_list_scrollbar(
        frame,
        app,
        Rect {
            x: area.right().saturating_sub(1),
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

pub fn render_local_mode_confirm(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    confirm: LocalModeConfirm,
) {
    let popup = centered_fixed(area, 64, 9);
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
            MouseTarget::ConfirmLocalMode,
            t!(" Confirm (Enter) ", " 확인 (Enter) "),
        ),
        buttons::Seg::label("    "),
        buttons::Seg::button(
            MouseTarget::CancelLocalMode,
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

fn centered_fixed(area: Rect, w: u16, h: u16) -> Rect {
    let width = w.min(area.width.saturating_sub(2).max(20));
    let height = h.min(area.height.saturating_sub(2).max(5));
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
    .intersection(area)
}
