//! Local Deck shell rendered under the Library mode.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::app::{
    App, LocalModeConfirm, LocalOrganizeConfirm, LocalSection, MouseTarget, ScrollSurface,
};
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
    let count = app.local_total_rows_len();
    let visible_count = app.local_rows_len();
    let count_label = if !app.local_mode.ui.filter_query.is_empty() {
        if crate::i18n::is_korean() {
            format!("{visible_count}/{count}개")
        } else {
            format!("{visible_count}/{count} rows")
        }
    } else if count == 1 {
        t!("1 row", "1개").to_owned()
    } else if crate::i18n::is_korean() {
        format!("{count}개")
    } else {
        format!("{count} rows")
    };
    let scan_label = if app.local_mode.index.scanning {
        t!("scan: running", "스캔: 진행 중")
    } else if app.local_mode.index.loading {
        t!("scan: loading", "스캔: 로드 중")
    } else {
        t!("scan: idle", "스캔: 대기")
    };
    let section = app.local_section_title();
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
    let text = if app.local_mode.ui.filter_editing || !app.local_mode.ui.filter_query.is_empty() {
        if app.local_mode.ui.filter_editing {
            format!("/{}", app.local_mode.ui.filter_query)
        } else {
            format!(
                "{}: /{}",
                t!("Filter", "필터"),
                app.local_mode.ui.filter_query
            )
        }
    } else if let Some(hint) = app.local_import_action_hint() {
        hint
    } else if app.local_mode.index.loading {
        t!(
            "Loading the Local Deck index...",
            "로컬 덱 인덱스를 불러오는 중..."
        )
        .to_owned()
    } else if app.local_mode.index.scanning {
        app.local_scan_progress_text().unwrap_or_else(|| {
            t!(
                "Scanning the download folder for local audio...",
                "다운로드 폴더의 로컬 오디오를 스캔 중..."
            )
            .to_owned()
        })
    } else if let Some(summary) = &app.local_mode.index.last_summary {
        let issue_count = app.local_scan_issue_count();
        if issue_count > 0 {
            format!(
                "{}: {} {} (+{} / ~{} / -{} / {} {})",
                t!("Last scan", "마지막 스캔"),
                summary.indexed,
                t!("tracks", "곡"),
                summary.added,
                summary.changed,
                summary.removed,
                issue_count,
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
    if app.local_mode.ui.section == LocalSection::Home {
        render_home(frame, app, area);
        return;
    }
    if area.width >= 110 {
        let panes = Layout::new(
            Direction::Horizontal,
            [
                Constraint::Length(18),
                Constraint::Min(32),
                Constraint::Length(30),
            ],
        )
        .split(area);
        render_sidebar(frame, app, panes[0]);
        render_rows(frame, app, panes[1]);
        render_details(frame, app, panes[2]);
    } else if area.width >= 80 {
        let rows = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(area);
        let panes = Layout::new(
            Direction::Horizontal,
            [Constraint::Length(18), Constraint::Min(0)],
        )
        .split(rows[0]);
        render_sidebar(frame, app, panes[0]);
        render_rows(frame, app, panes[1]);
        render_details_strip(frame, app, rows[1]);
    } else if area.width >= 72 {
        let panes = Layout::new(
            Direction::Horizontal,
            [Constraint::Length(18), Constraint::Min(0)],
        )
        .split(area);
        render_sidebar(frame, app, panes[0]);
        render_rows(frame, app, panes[1]);
    } else {
        render_rows(frame, app, area);
    }
}

fn render_details(frame: &mut Frame, app: &App, area: Rect) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let width = area.width as usize;
    for (i, line) in app
        .local_details_lines()
        .into_iter()
        .take(area.height as usize)
        .enumerate()
    {
        let style = if line.is_empty() {
            app.theme.style(R::TextMuted)
        } else if i == 0 || line == t!("Now playing", "재생 중") || line == t!("Up next", "다음 곡")
        {
            app.theme.style(R::Accent).add_modifier(Modifier::BOLD)
        } else {
            app.theme.style(R::TextMuted)
        };
        let text = crate::ui::text::truncate_owned_to_width(line, width.saturating_sub(1));
        frame.render_widget(
            Paragraph::new(Line::from(text).style(style)),
            Rect {
                x: area.x,
                y: area.y + i as u16,
                width: area.width,
                height: 1,
            },
        );
    }
}

fn render_details_strip(frame: &mut Frame, app: &App, area: Rect) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let text = crate::ui::text::truncate_owned_to_width(
        app.local_details_summary(),
        area.width.saturating_sub(1) as usize,
    );
    frame.render_widget(
        Paragraph::new(Line::from(text).style(app.theme.style(R::TextMuted))),
        area,
    );
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
                "Press r to scan the download folder.",
                "r로 다운로드 폴더를 스캔하세요."
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
                "Open Tracks to play downloaded files.",
                "곡 목록에서 다운로드 파일을 재생하세요."
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

fn render_sidebar(frame: &mut Frame, app: &App, area: Rect) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let width = area.width as usize;
    for (i, section) in LocalSection::ALL
        .iter()
        .enumerate()
        .take(area.height as usize)
    {
        let y = area.y + i as u16;
        let selected = *section == app.local_mode.ui.section;
        let marker = if selected { "> " } else { "  " };
        let key = if i == 9 {
            "0".to_owned()
        } else {
            (i + 1).to_string()
        };
        let label = format!("{key} {}", section.label());
        let text = crate::ui::text::truncate_owned_to_width(
            format!("{marker}{label}"),
            width.saturating_sub(1),
        );
        let style = if selected {
            let base = Style::default()
                .fg(app.theme.color(R::SelectionFg))
                .bg(app.theme.color(R::SelectionBg))
                .add_modifier(Modifier::BOLD);
            crate::ui::anim::selection_style(app, base)
        } else {
            app.theme.style(R::TextMuted)
        };
        let row = Rect {
            x: area.x,
            y,
            width: area.width.saturating_sub(1),
            height: 1,
        };
        frame.render_widget(Paragraph::new(Line::from(text).style(style)), row);
        app.register_mouse_button(row, MouseTarget::LocalNav(i));
    }
}

fn render_rows(frame: &mut Frame, app: &App, area: Rect) {
    let rows = app.local_visible_rows();
    if rows.is_empty() {
        if app.local_mode.ui.filter_query.is_empty() {
            render_empty_section(frame, app, area);
        } else {
            render_no_filter_matches(frame, app, area);
        }
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
        let text = app.local_row_text(song);
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

fn render_empty_section(frame: &mut Frame, app: &App, area: Rect) {
    let msg = match app.local_mode.ui.section {
        LocalSection::Albums => t!("No indexed albums yet.", "아직 인덱싱된 앨범이 없어요."),
        LocalSection::Artists => t!(
            "No indexed artists yet.",
            "아직 인덱싱된 아티스트가 없어요."
        ),
        LocalSection::Genres => t!("No indexed genres yet.", "아직 인덱싱된 장르가 없어요."),
        LocalSection::Folders => t!("No indexed folders yet.", "아직 인덱싱된 폴더가 없어요."),
        LocalSection::SmartLists => t!(
            "No smart lists available.",
            "사용할 수 있는 스마트 목록이 없어요."
        ),
        LocalSection::ScanErrors => t!("No scan errors.", "스캔 오류가 없어요."),
        LocalSection::ImportSessions => t!(
            "No imported local downloads yet.",
            "아직 임포트된 로컬 다운로드가 없어요."
        ),
        LocalSection::Inbox => t!(
            "No import inbox rows need attention.",
            "정리할 임포트 인박스 행이 없어요."
        ),
        _ => t!("No local rows yet.", "아직 로컬 항목이 없어요."),
    };
    frame.render_widget(
        Paragraph::new(Line::from(msg))
            .alignment(Alignment::Center)
            .style(app.theme.style(R::TextMuted)),
        area,
    );
}

fn render_no_filter_matches(frame: &mut Frame, app: &App, area: Rect) {
    let msg = if crate::i18n::is_korean() {
        format!(
            "'{}' 와 일치하는 로컬 항목이 없어요.",
            app.local_mode.ui.filter_query
        )
    } else {
        format!(
            "No Local Deck rows match \"{}\".",
            app.local_mode.ui.filter_query
        )
    };
    frame.render_widget(
        Paragraph::new(Line::from(msg))
            .alignment(Alignment::Center)
            .style(app.theme.style(R::TextMuted)),
        area,
    );
}

pub fn render_local_mode_confirm(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    confirm: LocalModeConfirm,
) {
    let popup = centered_fixed(area, 64, 11);
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
        Constraint::Length(3),
        Constraint::Min(0),
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
    buttons::render_segments_with_hit_height(
        frame,
        app,
        rows[4],
        &segs,
        (
            crate::ui::confirm_button_style(app),
            crate::ui::confirm_gap_style(app),
        ),
        Alignment::Center,
        3,
    );
    crate::ui::seal_popup_background(frame, app, popup);
    crate::ui::mark_art_rows_for_popup(frame, app, popup);
}

pub fn render_local_organize_confirm(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    confirm: &LocalOrganizeConfirm,
) {
    let popup = centered_fixed(area, 72, 11);
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
        Constraint::Length(3),
        Constraint::Min(0),
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
            MouseTarget::ConfirmLocalOrganize,
            t!(" Commit (Enter) ", " 커밋 (Enter) "),
        ),
        buttons::Seg::label("    "),
        buttons::Seg::button(
            MouseTarget::CancelLocalOrganize,
            t!(" Cancel (Esc) ", " 취소 (Esc) "),
        ),
    ];
    buttons::render_segments_with_hit_height(
        frame,
        app,
        rows[4],
        &segs,
        (
            crate::ui::confirm_button_style(app),
            crate::ui::confirm_gap_style(app),
        ),
        Alignment::Center,
        3,
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
