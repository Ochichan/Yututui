//! Collection-wide, offline-only Local Deck Find view.
//!
//! The top-level Search navigation slot is intentionally reused while Local Deck is active,
//! but this renderer reads only `local_mode.find`. Online Search state and provider controls are
//! never rendered here.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::app::{
    App, LocalFindBulkAction, LocalFindBulkConfirm, LocalFindFocus, LocalFindPointerStamp,
    MouseTarget, ScrollSurface, StatusKind,
};
use crate::local::find::{
    LocalFindHit, LocalFindHitId, LocalFindMatchReason, LocalFindScope, LocalFindSort,
};
use crate::t;
use crate::theme::ThemeRole as R;
use crate::ui::buttons;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FindWidthTier {
    Wide,
    Medium,
    Sidebar,
    Compact,
    Narrow,
}

fn width_tier(width: u16) -> FindWidthTier {
    match width {
        110.. => FindWidthTier::Wide,
        80..=109 => FindWidthTier::Medium,
        72..=79 => FindWidthTier::Sidebar,
        48..=71 => FindWidthTier::Compact,
        _ => FindWidthTier::Narrow,
    }
}

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

    let tier = width_tier(area.width);
    // At the minimum supported 14-row terminal, preserve useful body rows by dropping the action
    // strip. Keep the status row when it owns a drill breadcrumb or a non-docked global toast.
    let dense_height = inner.height <= 12;
    let status_visible = !dense_height
        || app.local_mode.find.drill.is_some()
        || (!app.status.text.is_empty() && !app.control_box_active());
    let rows = Layout::vertical([
        Constraint::Length(if status_visible { 1 } else { 0 }),
        Constraint::Length(3),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(if dense_height { 0 } else { 1 }),
        Constraint::Length(crate::ui::control_box::docked_rows(app)),
        Constraint::Length(1),
    ])
    .split(inner);

    render_status(frame, app, rows[0]);
    render_input(frame, app, rows[1], tier);
    render_scope_strip(frame, app, rows[2], tier);
    render_body(frame, app, rows[3], tier);
    render_actions(frame, app, rows[4]);
    crate::ui::control_box::render_docked(frame, app, rows[5]);
    buttons::render_help_button(frame, app, rows[6]);

    app.local_mode.find.refine_popup.rect.set(None);
    if app.local_mode.find.refine_popup.open {
        render_refine_popup(frame, app, inner);
    }
    if let Some(confirm) = &app.local_mode.find.pending_bulk_confirm {
        render_bulk_confirm(frame, app, inner, confirm);
    }
    if app.local_mode.find.pending_rebuild_confirm {
        render_rebuild_confirm(frame, app, inner);
    }
}

fn render_status(frame: &mut Frame, app: &App, area: Rect) {
    if area.is_empty() {
        return;
    }
    // Match normal Search: a global action/scan toast owns this band while no docked player title
    // exists to show it. Local activity returns as soon as the transient status clears.
    if !app.status.text.is_empty() && !app.control_box_active() {
        if let Some(line) = crate::ui::anim::status_toast_line(app, area.width) {
            frame.render_widget(Paragraph::new(line), area);
        } else {
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
                area,
            );
        }
        return;
    }
    let find = &app.local_mode.find;
    let count = find.drill.as_ref().map_or_else(
        || {
            find.snapshot
                .as_ref()
                .map_or(0, |snapshot| snapshot.total_hits)
        },
        |d| d.track_ids.len(),
    );
    let activity = if app.local_mode.index.loading {
        t!("index loading", "인덱스 로딩 중")
    } else if find.searching {
        t!("searching...", "검색 중...")
    } else if app.local_mode.index.scanning {
        t!("scan running", "스캔 진행 중")
    } else if find.parse_error.is_some() {
        t!("query needs attention", "검색어 확인 필요")
    } else {
        t!("offline", "오프라인")
    };
    let count = if count == 1 {
        t!("1 result", "결과 1개").to_owned()
    } else if crate::i18n::is_korean() {
        format!("결과 {count}개")
    } else {
        format!("{count} results")
    };
    let title = find
        .drill
        .as_ref()
        .map(|drill| format!("{}  /  {}", t!("LOCAL FIND", "로컬 찾기"), drill.title))
        .unwrap_or_else(|| t!("LOCAL FIND", "로컬 찾기").to_owned());
    let line = Line::from(vec![
        Span::styled(
            format!(" {title} "),
            app.theme.style(R::Accent).add_modifier(Modifier::BOLD),
        ),
        Span::styled("  -  ", app.theme.style(R::TextMuted)),
        Span::styled(activity, app.theme.style(R::TextMuted)),
        Span::styled("  -  ", app.theme.style(R::TextMuted)),
        Span::styled(count, app.theme.style(R::TextMuted)),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn render_input(frame: &mut Frame, app: &App, area: Rect, tier: FindWidthTier) {
    if area.is_empty() {
        return;
    }
    let scope_width = match tier {
        FindWidthTier::Wide => 14,
        FindWidthTier::Medium | FindWidthTier::Sidebar => 12,
        FindWidthTier::Compact => 11,
        FindWidthTier::Narrow => 9,
    };
    let (scope_area, input_area, submit_area, refine_area) = match tier {
        FindWidthTier::Wide | FindWidthTier::Medium | FindWidthTier::Sidebar => {
            let submit_w = if tier == FindWidthTier::Wide { 10 } else { 8 };
            let refine_w = if tier == FindWidthTier::Wide { 12 } else { 10 };
            let cols = Layout::horizontal([
                Constraint::Length(scope_width),
                Constraint::Min(12),
                Constraint::Length(submit_w),
                Constraint::Length(refine_w),
            ])
            .split(area);
            (cols[0], cols[1], Some(cols[2]), Some(cols[3]))
        }
        FindWidthTier::Compact => {
            let cols = Layout::horizontal([
                Constraint::Length(scope_width),
                Constraint::Min(8),
                Constraint::Length(8),
            ])
            .split(area);
            (cols[0], cols[1], Some(cols[2]), None)
        }
        FindWidthTier::Narrow => {
            let cols = Layout::horizontal([Constraint::Length(scope_width), Constraint::Min(1)])
                .split(area);
            (cols[0], cols[1], None, None)
        }
    };
    render_scope_chip(frame, app, scope_area, tier);

    let focused = app.local_mode.find.focus == LocalFindFocus::Input;
    let border = if focused {
        R::BorderFocused
    } else {
        R::BorderMuted
    };
    let title = if tier == FindWidthTier::Narrow {
        format!(" {} ", t!("Find", "찾기"))
    } else {
        format!(" {} ", t!("Find local collection", "로컬 컬렉션 찾기"))
    };
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(app.theme.style(border))
        .style(app.theme.style(R::TextPrimary));
    let input = &app.local_mode.find.query;
    let content_width = block.inner(input_area).width as usize;
    let paragraph = if focused && app.local_mode.find.select_all && !input.is_empty() {
        let selected = Style::default()
            .fg(app.theme.color(R::SelectionFg))
            .bg(app.theme.color(R::SelectionBg));
        let visible = crate::ui::text::tail_to_width(input, content_width);
        Paragraph::new(Line::from(Span::styled(visible, selected)))
    } else if focused {
        let cursor = app.local_mode.find.input_cursor.byte_index(input);
        let window = crate::ui::text::editable_window(input, cursor, content_width);
        Paragraph::new(Line::from(vec![
            Span::styled(window.before, app.theme.style(R::TextPrimary)),
            crate::ui::anim::caret_span(
                app,
                app.theme.style(R::TextPrimary),
                app.theme.color(R::Background),
            ),
            Span::styled(window.after, app.theme.style(R::TextPrimary)),
        ]))
    } else {
        Paragraph::new(crate::ui::text::tail_to_width(input, content_width))
            .style(app.theme.style(R::TextPrimary))
    };
    frame.render_widget(paragraph.block(block), input_area);
    app.register_mouse_button(input_area, MouseTarget::LocalFindInput);

    if let Some(area) = submit_area {
        render_bar_button(
            frame,
            app,
            area,
            if tier == FindWidthTier::Wide {
                t!(" Find ", " 찾기 ")
            } else {
                t!("Find", "찾기")
            },
            MouseTarget::LocalFindSubmit,
            R::Accent,
        );
    }
    if let Some(area) = refine_area {
        render_bar_button(
            frame,
            app,
            area,
            if tier == FindWidthTier::Wide {
                t!(" Refine ", " 상세 ")
            } else {
                t!("Refine", "상세")
            },
            MouseTarget::LocalFindRefineOpen,
            R::TextPrimary,
        );
    }
}

/// Local Find's left chip occupies the same visual role as normal Search's provider chip, but it
/// opens the local scope/sort editor and can never select an online source.
fn render_scope_chip(frame: &mut Frame, app: &App, area: Rect, tier: FindWidthTier) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(app.theme.style(R::BorderMuted))
        .style(app.theme.style(R::TextPrimary));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let label = if matches!(tier, FindWidthTier::Compact | FindWidthTier::Narrow) {
        scope_short_label(app.local_mode.find.scope)
    } else {
        scope_label(app.local_mode.find.scope)
    };
    let label = crate::ui::text::truncate_owned_to_width(format!("{label}▾"), inner.width as usize);
    frame.render_widget(
        Paragraph::new(
            Line::from(label)
                .style(app.theme.style(R::Accent).add_modifier(Modifier::BOLD))
                .alignment(Alignment::Center),
        ),
        inner,
    );
    app.register_mouse_button(area, MouseTarget::LocalFindRefineOpen);
}

fn render_bar_button(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    label: &str,
    target: MouseTarget,
    role: R,
) {
    if area.is_empty() {
        return;
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(app.theme.style(R::BorderMuted))
        .style(app.theme.style(R::TextPrimary));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(
        Paragraph::new(Line::from(label).style(app.theme.style(role).add_modifier(Modifier::BOLD)))
            .alignment(Alignment::Center),
        inner,
    );
    app.register_mouse_button(area, target);
}

fn render_scope_strip(frame: &mut Frame, app: &App, area: Rect, tier: FindWidthTier) {
    if area.is_empty() {
        return;
    }
    let find = &app.local_mode.find;
    let error = find.parse_error.as_deref();
    let effective_sort = find
        .snapshot
        .as_ref()
        .map_or(find.sort, |snapshot| snapshot.sort);
    let text = if let Some(error) = error {
        format!(
            "! {error}  -  {}",
            t!("previous results kept", "이전 결과 유지")
        )
    } else if tier == FindWidthTier::Narrow {
        format!(
            "{} - {} - / {}",
            scope_label(find.scope),
            sort_label(effective_sort),
            t!("refine", "상세")
        )
    } else {
        format!(
            "{}: {}   {}: {}   {}",
            t!("Scope", "범위"),
            scope_label(find.scope),
            t!("Sort", "정렬"),
            sort_label(effective_sort),
            t!("offline index only", "오프라인 인덱스만 사용")
        )
    };
    let role = if error.is_some() {
        R::Error
    } else {
        R::TextMuted
    };
    let shown = crate::ui::text::truncate_owned_to_width(text, area.width as usize);
    frame.render_widget(
        Paragraph::new(Line::from(shown).style(app.theme.style(role))).alignment(Alignment::Center),
        area,
    );
    app.register_mouse_button(area, MouseTarget::LocalFindRefineOpen);
}

fn render_body(frame: &mut Frame, app: &App, area: Rect, tier: FindWidthTier) {
    if area.is_empty() {
        return;
    }
    app.bridges.list_viewport_rows.set(area.height);
    if app.local_mode.find.query.trim().is_empty() && app.local_mode.find.drill.is_none() {
        render_launchpad(frame, app, area, tier);
        return;
    }
    if app.local_mode.find.drill.is_some() {
        render_results_and_details(frame, app, area, tier, true);
        return;
    }
    if app.local_mode.index.index.is_empty() && app.local_mode.index.loading {
        render_centered_message(
            frame,
            app,
            area,
            t!(
                "Loading the Local Deck index...",
                "로컬 덱 인덱스를 불러오는 중..."
            ),
            R::Accent,
        );
        return;
    }
    if app.local_mode.index.index.is_empty() && app.local_mode.index.scanning {
        render_centered_message(
            frame,
            app,
            area,
            t!("Scanning local audio...", "로컬 오디오를 스캔하는 중..."),
            R::Accent,
        );
        return;
    }
    match app.local_mode.find.snapshot.as_ref() {
        Some(snapshot) if snapshot.total_hits > 0 => {
            render_results_and_details(frame, app, area, tier, false)
        }
        _ if app.local_mode.find.searching => render_centered_message(
            frame,
            app,
            area,
            t!(
                "Searching the local index...",
                "로컬 인덱스를 검색하는 중..."
            ),
            R::Accent,
        ),
        _ if unknown_command(app) => render_unknown_command(frame, app, area),
        _ if app.local_mode.index.index.is_empty()
            && app.status.kind == StatusKind::Error
            && !app.status.text.is_empty() =>
        {
            render_message_with_recovery(frame, app, area, &app.status.text, R::Error)
        }
        _ => render_no_results(frame, app, area),
    }
}

fn unknown_command(app: &App) -> bool {
    app.local_mode
        .find
        .snapshot
        .as_ref()
        .and_then(|snapshot| snapshot.query.command.as_ref())
        .is_some_and(|command| command.exact.is_none() && command.suggestions.is_empty())
}

fn render_results_and_details(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    tier: FindWidthTier,
    drill: bool,
) {
    if tier == FindWidthTier::Wide && area.width >= 72 {
        let panes = Layout::new(
            Direction::Horizontal,
            [Constraint::Percentage(68), Constraint::Percentage(32)],
        )
        .split(area);
        render_result_list(frame, app, panes[0], tier, drill);
        render_selection_details(frame, app, panes[1]);
    } else {
        render_result_list(frame, app, area, tier, drill);
    }
}

fn render_result_list(frame: &mut Frame, app: &App, area: Rect, tier: FindWidthTier, drill: bool) {
    let len = app.local_find_rows_len();
    if len == 0 || area.is_empty() {
        return;
    }
    let cursor = app.local_mode.find.selected.min(len - 1);
    let visible = area.height as usize;
    let start = app.bridges.local_find_scroll.resolve(
        cursor,
        area.height,
        len,
        crate::ui::scroll::SCROLLOFF,
    );
    let row_width = area.width.saturating_sub(1);
    let stamp = app.local_find_pointer_stamp();

    if drill {
        for (visible_index, index) in (start..len).take(visible).enumerate() {
            let Some(track) = app.local_find_drill_track_at(index) else {
                continue;
            };
            let content = format_track_row(track, tier);
            render_result_row(
                frame,
                app,
                Rect {
                    x: area.x,
                    y: area.y + visible_index as u16,
                    width: row_width,
                    height: 1,
                },
                index,
                cursor,
                &stamp,
                content,
            );
        }
    } else if let Some(snapshot) = &app.local_mode.find.snapshot {
        // Flat lookup is bounded by the six groups; never walk `0..start` at the tail of a 50k
        // result merely to paint one viewport.
        for (visible_index, index) in (start..len).take(visible).enumerate() {
            let Some(hit) = snapshot.hit_at(index) else {
                continue;
            };
            let Some((scope, group_index, group_len)) = group_meta(snapshot, index) else {
                continue;
            };
            let content = format_hit_row(hit, scope, group_index, group_len, tier);
            render_result_row(
                frame,
                app,
                Rect {
                    x: area.x,
                    y: area.y + visible_index as u16,
                    width: row_width,
                    height: 1,
                },
                index,
                cursor,
                &stamp,
                content,
            );
        }
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
        ScrollSurface::LocalFind,
        len,
        start,
        visible,
    );
}

fn group_meta(
    snapshot: &crate::local::find::LocalFindSnapshot,
    mut index: usize,
) -> Option<(LocalFindScope, usize, usize)> {
    for group in &snapshot.groups {
        if index < group.hits.len() {
            return Some((group.scope, index, group.hits.len()));
        }
        index = index.saturating_sub(group.hits.len());
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn render_result_row(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    index: usize,
    cursor: usize,
    stamp: &LocalFindPointerStamp,
    content: String,
) {
    let selected = index == cursor;
    let marker = if selected { "> " } else { "  " };
    let body_width = area.width as usize;
    let text = if selected {
        let mut text = crate::ui::anim::selected_marquee(
            app,
            ScrollSurface::LocalFind,
            index,
            content.as_str(),
            body_width.saturating_sub(3),
        );
        text.insert_str(0, marker);
        crate::ui::text::truncate_owned_to_width(text, body_width)
    } else {
        crate::ui::text::truncate_owned_to_width(format!("{marker}{content}"), body_width)
    };
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
    frame.render_widget(Paragraph::new(Line::from(text).style(style)), area);
    app.register_mouse_button(
        area,
        MouseTarget::LocalFindRow {
            index,
            stamp: stamp.clone(),
        },
    );
}

fn format_hit_row(
    hit: &LocalFindHit,
    scope: LocalFindScope,
    group_index: usize,
    group_len: usize,
    tier: FindWidthTier,
) -> String {
    let kind = hit_kind_label(hit);
    let compact = matches!(tier, FindWidthTier::Compact | FindWidthTier::Narrow);
    let group = if compact {
        format!("[{kind} {}/{}]", group_index + 1, group_len)
    } else if group_index == 0 {
        format!("[{} · {group_len}] [{kind}]", scope_label(scope))
    } else {
        format!("[{kind}]")
    };
    let playlist = matches!(&hit.id, LocalFindHitId::Playlist(_));
    let count = match (playlist, hit.locally_playable_count, hit.total_track_count) {
        (true, local, total) => format!("{local}/{total} {}", t!("local", "로컬")),
        (false, 0, 0) => String::new(),
        (false, local, total) if local != total => format!("{local}/{total}"),
        (false, _, 1) => String::new(),
        (false, local, _) => format!("{local} {}", t!("tracks", "곡")),
    };
    let reason = match hit.match_reason {
        Some(LocalFindMatchReason::PlaylistName) => t!("name match", "이름 일치"),
        Some(LocalFindMatchReason::ResolvedLocalTrack) => {
            t!("local track match", "로컬 곡 일치")
        }
        None => "",
    };
    // Playlist safety metadata leads the row so truncation can only remove descriptive text,
    // never the offline projection count or match reason.
    if playlist {
        let reason = if reason.is_empty() {
            String::new()
        } else {
            format!(" · {reason}")
        };
        let secondary = if hit.secondary.trim().is_empty() {
            String::new()
        } else {
            format!(" · {}", hit.secondary.trim())
        };
        let group_count = if compact {
            format!(" · {}/{}", group_index + 1, group_len)
        } else if group_index == 0 {
            format!(" · [{} · {group_len}]", scope_label(scope))
        } else {
            String::new()
        };
        return format!(
            "[{kind}] {count}{reason}{group_count} · {}{secondary}",
            hit.label
        );
    }
    let count = if count.is_empty() {
        String::new()
    } else {
        format!("  {count}")
    };
    let year = hit
        .year
        .map_or_else(String::new, |year| format!("  {year}"));
    let secondary = if hit.secondary.trim().is_empty() {
        String::new()
    } else if tier == FindWidthTier::Narrow {
        format!(" - {}", hit.secondary.trim())
    } else {
        format!("  -  {}", hit.secondary.trim())
    };
    format!("{group} {}{secondary}{year}{count}", hit.label)
}

fn hit_kind_label(hit: &LocalFindHit) -> &'static str {
    match &hit.id {
        LocalFindHitId::Track(_) => t!("Track", "곡"),
        LocalFindHitId::Album(_) => t!("Album", "앨범"),
        LocalFindHitId::Artist(_) => t!("Artist", "아티스트"),
        LocalFindHitId::Genre(_) => t!("Genre", "장르"),
        LocalFindHitId::Folder(_) => t!("Folder", "폴더"),
        LocalFindHitId::Playlist(_) => t!("Playlist", "플레이리스트"),
        LocalFindHitId::Command(_) => t!("Command", "명령"),
    }
}

fn format_track_row(track: &crate::local::LocalTrack, tier: FindWidthTier) -> String {
    let kind = t!("Track", "곡");
    let artist = track.display_artist();
    let album = track.album.as_deref().unwrap_or_default();
    let duration = track.duration_ms.map(format_duration).unwrap_or_default();
    match tier {
        FindWidthTier::Narrow => format!("[{kind}] {} - {artist}", track.display_title()),
        FindWidthTier::Compact | FindWidthTier::Sidebar => {
            format!(
                "[{kind}] {}  -  {artist}  {duration}",
                track.display_title()
            )
        }
        FindWidthTier::Medium | FindWidthTier::Wide => format!(
            "[{kind}] {}  -  {artist}  -  {album}  {duration}",
            track.display_title()
        ),
    }
}

fn render_selection_details(frame: &mut Frame, app: &App, area: Rect) {
    if area.is_empty() {
        return;
    }
    let block = Block::default()
        .title(format!(" {} ", t!("Selection", "선택")))
        .borders(Borders::LEFT)
        .border_style(app.theme.style(R::BorderMuted))
        .style(app.theme.style(R::TextPrimary));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let mut lines = Vec::new();
    if let Some(track) = app.local_find_drill_track_at(app.local_mode.find.selected) {
        lines.push(Line::from(Span::styled(
            track.display_title(),
            app.theme.style(R::Accent).add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(track.display_artist()).style(app.theme.style(R::TextMuted)));
        if let Some(album) = &track.album {
            lines.push(Line::from(album.clone()).style(app.theme.style(R::TextMuted)));
        }
        lines.push(
            Line::from(track.path.display().to_string()).style(app.theme.style(R::TextMuted)),
        );
    } else if let Some(hit) = app.local_find_hit_at(app.local_mode.find.selected) {
        lines.push(Line::from(Span::styled(
            hit.label.clone(),
            app.theme.style(R::Accent).add_modifier(Modifier::BOLD),
        )));
        if !hit.secondary.is_empty() {
            lines.push(Line::from(hit.secondary.clone()).style(app.theme.style(R::TextMuted)));
        }
        if let Some(year) = hit.year {
            lines.push(
                Line::from(format!("{}: {year}", t!("Year", "연도")))
                    .style(app.theme.style(R::TextMuted)),
            );
        }
        lines.push(
            Line::from(format!(
                "{}: {}/{}",
                t!("Playable", "재생 가능"),
                hit.locally_playable_count,
                hit.total_track_count
            ))
            .style(app.theme.style(R::TextMuted)),
        );
        if let Some(reason) = hit.match_reason {
            let reason = match reason {
                LocalFindMatchReason::PlaylistName => t!("Playlist name", "플레이리스트 이름"),
                LocalFindMatchReason::ResolvedLocalTrack => {
                    t!("Resolved local track", "확인된 로컬 곡")
                }
            };
            lines.push(
                Line::from(format!("{}: {reason}", t!("Matched by", "일치 기준")))
                    .style(app.theme.style(R::TextMuted)),
            );
        }
    }
    lines.push(Line::from(""));
    lines.push(
        Line::from(t!("Enter open/play  a add", "Enter 열기/재생  a 추가"))
            .style(app.theme.style(R::TextMuted)),
    );
    lines.push(
        Line::from(t!("P play  A add all", "P 재생  A 모두 추가"))
            .style(app.theme.style(R::TextMuted)),
    );
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
}

const LAUNCHPAD: [(&str, &str); 7] = [
    ("1  Recently added", "1  최근 추가"),
    ("2  Local-only", "2  로컬 전용"),
    ("3  Lossless", "3  무손실"),
    ("4  Missing artist", "4  아티스트 누락"),
    ("5  Missing album", "5  앨범 누락"),
    ("6  No embedded cover", "6  내장 커버 없음"),
    ("r  Rescan", "r  다시 스캔"),
];

fn render_launchpad(frame: &mut Frame, app: &App, area: Rect, tier: FindWidthTier) {
    if app.local_mode.index.index.is_empty() {
        render_recovery(frame, app, area);
        return;
    }
    if area.is_empty() {
        return;
    }
    let title = Rect { height: 1, ..area };
    frame.render_widget(
        Paragraph::new(t!(
            "Start with a smart view, type a query, or use >commands",
            "스마트 보기에서 시작하거나 검색어 또는 >명령을 입력하세요"
        ))
        .alignment(Alignment::Center)
        .style(app.theme.style(R::TextMuted)),
        title,
    );
    let options = Rect {
        y: area.y.saturating_add(1),
        height: area.height.saturating_sub(1),
        ..area
    };
    if options.is_empty() {
        return;
    }
    // These are reducer rows (Up/Down moves one item), so keep a vertical list even at wide
    // widths. A centered column avoids stretching short launch actions across the whole screen;
    // short terminals scroll the selected row into view.
    let max_width = match tier {
        FindWidthTier::Wide => 60,
        FindWidthTier::Medium | FindWidthTier::Sidebar => 54,
        FindWidthTier::Compact | FindWidthTier::Narrow => options.width,
    };
    let list = centered_column(options, max_width);
    let selected = app.local_mode.find.selected.min(LAUNCHPAD.len() - 1);
    let start = app.bridges.local_find_scroll.resolve(
        selected,
        list.height,
        LAUNCHPAD.len(),
        crate::ui::scroll::SCROLLOFF,
    );
    let row_width = list.width.saturating_sub(1);
    let stamp = app.local_find_pointer_stamp();
    for (visible, (index, (english, korean))) in LAUNCHPAD
        .iter()
        .enumerate()
        .skip(start)
        .take(list.height as usize)
        .enumerate()
    {
        let rect = Rect {
            x: list.x,
            y: list.y + visible as u16,
            width: row_width,
            height: 1,
        };
        let label = if crate::i18n::is_korean() {
            *korean
        } else {
            *english
        };
        let selected = app.local_mode.find.focus == LocalFindFocus::Results
            && app.local_mode.find.selected == index;
        let marker = if selected { "> " } else { "  " };
        let shown = crate::ui::text::truncate_owned_to_width(
            format!("{marker}{label}"),
            rect.width.saturating_sub(1) as usize,
        );
        frame.render_widget(
            Paragraph::new(Line::from(shown).style(local_find_row_style(app, selected))),
            rect,
        );
        app.register_mouse_button(
            rect,
            MouseTarget::LocalFindLaunchpad {
                index,
                stamp: stamp.clone(),
            },
        );
    }
    render_local_find_scrollbar(frame, app, list, LAUNCHPAD.len(), start);
}

fn render_recovery(frame: &mut Frame, app: &App, area: Rect) {
    if area.is_empty() {
        return;
    }
    let actions = recovery_rows(app, false);
    let message_height = area.height.saturating_sub(actions.len() as u16);
    let message_area = Rect {
        height: message_height,
        ..area
    };
    let options = Rect {
        y: area.y + message_height,
        height: area.height.saturating_sub(message_height),
        ..area
    };
    let (message, role) = if app.local_mode.index.loading {
        (
            t!(
                "Loading the Local Deck index...",
                "로컬 덱 인덱스를 불러오는 중..."
            )
            .to_owned(),
            R::Accent,
        )
    } else if app.local_mode.index.scanning {
        (
            t!("Scanning local audio...", "로컬 오디오를 스캔하는 중...").to_owned(),
            R::Accent,
        )
    } else if app.status.kind == StatusKind::Error && !app.status.text.is_empty() {
        (app.status.text.clone(), R::Error)
    } else {
        (
            t!(
                "No local music is indexed yet.",
                "아직 인덱싱된 로컬 음악이 없습니다."
            )
            .to_owned(),
            R::TextMuted,
        )
    };
    if !message_area.is_empty() {
        frame.render_widget(
            Paragraph::new(message)
                .alignment(Alignment::Center)
                .wrap(Wrap { trim: true })
                .style(app.theme.style(role)),
            message_area,
        );
    }
    render_recovery_rows(frame, app, options, &actions);
}

fn render_no_results(frame: &mut Frame, app: &App, area: Rect) {
    if area.is_empty() {
        return;
    }
    let query = crate::ui::text::truncate_owned_to_width(
        app.local_mode.find.query.clone(),
        area.width.saturating_sub(8) as usize,
    );
    let text = if crate::i18n::is_korean() {
        format!(
            "'{query}'와 일치하는 로컬 항목이 없습니다. 범위 칩을 누르거나 Down 뒤 / 로 상세 검색을 여세요."
        )
    } else {
        format!(
            "No local items match \"{query}\". Use the scope chip, or press Down then / to refine."
        )
    };
    render_message_with_recovery(frame, app, area, &text, R::TextMuted);
}

fn render_unknown_command(frame: &mut Frame, app: &App, area: Rect) {
    let query = crate::ui::text::truncate_owned_to_width(
        app.local_mode.find.query.clone(),
        area.width.saturating_sub(10) as usize,
    );
    let text = if crate::i18n::is_korean() {
        format!("알 수 없는 로컬 명령: {query}. 예: > tracks, > rescan, > scan errors")
    } else {
        format!("Unknown Local command: {query}. Try > tracks, > rescan, or > scan errors.")
    };
    render_message_with_recovery(frame, app, area, &text, R::Error);
}

fn render_message_with_recovery(frame: &mut Frame, app: &App, area: Rect, text: &str, role: R) {
    let actions = recovery_rows(app, true);
    render_message_with_rows(frame, app, area, text, role, &actions);
}

fn render_message_with_rows(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    text: &str,
    role: R,
    actions: &[(usize, &str, &str)],
) {
    if area.is_empty() {
        return;
    }
    let message_height = area.height.saturating_sub(actions.len() as u16);
    let message_area = Rect {
        height: message_height,
        ..area
    };
    let options = Rect {
        y: area.y + message_height,
        height: area.height.saturating_sub(message_height),
        ..area
    };
    if !message_area.is_empty() {
        render_centered_message(frame, app, message_area, text, role);
    }
    render_recovery_rows(frame, app, options, actions);
}

fn recovery_rows(app: &App, include_clear: bool) -> Vec<(usize, &'static str, &'static str)> {
    app.local_find_recovery_actions(include_clear)
        .into_iter()
        .filter_map(|action| {
            let labels = match action {
                6 => ("r  Rescan", "r  다시 스캔"),
                7 => ("c  Clear query", "c  검색어 지우기"),
                8 => ("+  Add music folder", "+  음악 폴더 추가"),
                9 => ("!  View scan errors", "!  스캔 오류 보기"),
                _ => return None,
            };
            Some((action, labels.0, labels.1))
        })
        .collect()
}

fn render_recovery_rows(frame: &mut Frame, app: &App, area: Rect, actions: &[(usize, &str, &str)]) {
    if area.is_empty() || actions.is_empty() {
        return;
    }
    let list = centered_column(area, 52);
    let cursor = app.local_mode.find.selected.min(actions.len() - 1);
    let start = app.bridges.local_find_scroll.resolve(
        cursor,
        list.height,
        actions.len(),
        crate::ui::scroll::SCROLLOFF,
    );
    let row_width = list.width.saturating_sub(1);
    let stamp = app.local_find_pointer_stamp();
    for (visible, (position, (target, english, korean))) in actions
        .iter()
        .enumerate()
        .skip(start)
        .take(list.height as usize)
        .enumerate()
    {
        let selected = app.local_mode.find.focus == LocalFindFocus::Results && cursor == position;
        let marker = if selected { "> " } else { "  " };
        let label = if crate::i18n::is_korean() {
            *korean
        } else {
            *english
        };
        let text = crate::ui::text::truncate_owned_to_width(
            format!("{marker}{label}"),
            row_width.saturating_sub(1) as usize,
        );
        let rect = Rect {
            x: list.x,
            y: list.y + visible as u16,
            width: row_width,
            height: 1,
        };
        let style = if !selected && *target == 6 {
            app.theme.style(R::Accent).add_modifier(Modifier::BOLD)
        } else {
            local_find_row_style(app, selected)
        };
        frame.render_widget(Paragraph::new(Line::from(text).style(style)), rect);
        app.register_mouse_button(
            rect,
            MouseTarget::LocalFindLaunchpad {
                index: *target,
                stamp: stamp.clone(),
            },
        );
    }
    render_local_find_scrollbar(frame, app, list, actions.len(), start);
}

fn render_local_find_scrollbar(frame: &mut Frame, app: &App, area: Rect, len: usize, start: usize) {
    buttons::render_list_scrollbar(
        frame,
        app,
        Rect {
            x: area.right().saturating_sub(1),
            y: area.y,
            width: 1,
            height: area.height,
        },
        ScrollSurface::LocalFind,
        len,
        start,
        area.height as usize,
    );
}

fn centered_column(area: Rect, max_width: u16) -> Rect {
    let width = area.width.min(max_width);
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        width,
        ..area
    }
}

fn render_centered_message(frame: &mut Frame, app: &App, area: Rect, text: &str, role: R) {
    frame.render_widget(
        Paragraph::new(text)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true })
            .style(app.theme.style(role)),
        area,
    );
}

fn render_actions(frame: &mut Frame, app: &App, area: Rect) {
    if area.is_empty() {
        return;
    }
    let text = if app.local_mode.find.focus == LocalFindFocus::Input {
        t!(
            "Enter search  Down results  Esc back",
            "Enter 검색  Down 결과  Esc 뒤로"
        )
    } else {
        t!(
            "Enter open/play  a add  P play  A add all  s shuffle all  / refine  Esc back",
            "Enter 열기/재생  a 추가  P 재생  A 모두 추가  s 전체 셔플  / 상세  Esc 뒤로"
        )
    };
    let shown = crate::ui::text::truncate_owned_to_width(text.to_owned(), area.width as usize);
    frame.render_widget(
        Paragraph::new(Line::from(shown).style(app.theme.style(R::TextMuted)))
            .alignment(Alignment::Center),
        area,
    );
}

fn render_refine_popup(frame: &mut Frame, app: &App, area: Rect) {
    let popup = centered_fixed(area, 70, 15);
    if popup.is_empty() {
        return;
    }
    app.local_mode.find.refine_popup.rect.set(Some(popup));
    crate::ui::render_popup_background(frame, app, popup);
    let block = Block::default()
        .title(t!(" Refine Local Find ", " 로컬 찾기 상세 설정 "))
        .borders(Borders::ALL)
        .border_style(crate::ui::popup_style(app, R::Accent).add_modifier(Modifier::BOLD))
        .style(crate::ui::popup_style(app, R::TextPrimary));
    let inner = block.inner(popup);
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .split(inner);
    let help = refine_help_lines(rows[2].width as usize);
    let offset = app
        .local_mode
        .find
        .refine_popup
        .help_scroll
        .view(rows[2].height, help.len());
    let block = if help.len() > rows[2].height as usize {
        block.title_bottom(
            Line::from(t!(" PgUp/PgDn scroll ", " PgUp/PgDn 스크롤 "))
                .right_aligned()
                .style(crate::ui::popup_style(app, R::TextMuted)),
        )
    } else {
        block
    };
    frame.render_widget(block, popup);

    render_refine_value(
        frame,
        app,
        rows[0],
        0,
        t!("Scope", "범위"),
        scope_label(app.local_mode.find.refine_popup.draft_scope),
    );
    render_refine_value(
        frame,
        app,
        rows[1],
        1,
        t!("Sort", "정렬"),
        sort_label(app.local_mode.find.refine_popup.draft_sort),
    );

    frame.render_widget(
        Paragraph::new(help)
            .scroll((offset.min(u16::MAX as usize) as u16, 0))
            .style(crate::ui::popup_style(app, R::TextMuted)),
        rows[2],
    );

    let actions =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(rows[3]);
    let (apply, cancel) = if rows[3].width < 32 {
        (
            t!("Enter apply", "Enter 적용"),
            t!("Esc cancel", "Esc 취소"),
        )
    } else {
        (
            t!("Apply (Enter)", "적용 (Enter)"),
            t!("Cancel (Esc)", "취소 (Esc)"),
        )
    };
    render_refine_action(frame, app, actions[0], 2, apply);
    render_refine_action(frame, app, actions[1], 3, cancel);
    crate::ui::seal_popup_background(frame, app, popup);
    crate::ui::mark_art_rows_for_popup(frame, app, popup);
}

fn refine_help_lines(width: usize) -> Vec<Line<'static>> {
    let rows = [
        t!(
            "Fields: t: title · ar: artist · al: album · aa: album artist",
            "필드: t: 제목 · ar: 아티스트 · al: 앨범 · aa: 앨범 아티스트"
        ),
        t!(
            "g: genre · path: filename/path · fmt: audio format",
            "g: 장르 · path: 파일명/경로 · fmt: 오디오 형식"
        ),
        t!(
            "Year: year:1990 or year:1990..1999",
            "연도: year:1990 또는 year:1990..1999"
        ),
        t!(
            "Properties: is:downloaded/local-only/lossless",
            "속성: is:downloaded/local-only/lossless"
        ),
        t!(
            "Missing: missing:artist/album/cover",
            "누락: missing:artist/album/cover"
        ),
        t!(
            "Sort: sort:relevance/title/artist/album/year/recent",
            "정렬: sort:relevance/title/artist/album/year/recent"
        ),
        t!(
            "Use quotes for phrases. Start with > for safe Local commands.",
            "구문은 따옴표로 묶고, 안전한 로컬 명령은 >로 시작하세요."
        ),
    ];
    rows.into_iter()
        .flat_map(|row| crate::ui::text::wrap_to_width(row, width))
        .map(Line::from)
        .collect()
}

fn render_refine_value(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    row: usize,
    label: &str,
    value: &str,
) {
    let selected = app.local_mode.find.refine_popup.row == row;
    let style = popup_row_style(app, selected);
    let marker = if selected { ">" } else { " " };
    let text = format!("{marker} {label}:  < {value} >");
    frame.render_widget(Paragraph::new(Line::from(text).style(style)), area);
    app.register_mouse_button(area, MouseTarget::LocalFindRefineRow(row));
}

fn render_refine_action(frame: &mut Frame, app: &App, area: Rect, row: usize, label: &str) {
    let selected = app.local_mode.find.refine_popup.row == row;
    frame.render_widget(
        Paragraph::new(label)
            .alignment(Alignment::Center)
            .style(popup_row_style(app, selected)),
        area,
    );
    app.register_mouse_button(area, MouseTarget::LocalFindRefineRow(row));
}

fn popup_row_style(app: &App, selected: bool) -> Style {
    if selected {
        Style::default()
            .fg(app.theme.color(R::SelectionFg))
            .bg(app.theme.color(R::SelectionBg))
            .add_modifier(Modifier::BOLD)
    } else {
        crate::ui::popup_style(app, R::TextPrimary)
    }
}

fn local_find_row_style(app: &App, selected: bool) -> Style {
    if selected {
        crate::ui::anim::selection_style(
            app,
            Style::default()
                .fg(app.theme.color(R::SelectionFg))
                .bg(app.theme.color(R::SelectionBg))
                .add_modifier(Modifier::BOLD),
        )
    } else {
        app.theme.style(R::TextPrimary)
    }
}

fn render_rebuild_confirm(frame: &mut Frame, app: &App, area: Rect) {
    let popup = centered_fixed(area, 68, 9);
    if popup.is_empty() {
        return;
    }
    frame.render_widget(Clear, popup);
    crate::ui::render_popup_background(frame, app, popup);
    let block = Block::default()
        .title(t!(
            " Full Local index rebuild ",
            " 로컬 인덱스 전체 재구축 "
        ))
        .borders(Borders::ALL)
        .border_style(crate::ui::confirm_border_style(app))
        .style(crate::ui::popup_style(app, R::TextPrimary));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(4),
        Constraint::Min(1),
    ])
    .split(inner);
    frame.render_widget(
        Paragraph::new(t!(
            "Rebuild the complete Local Deck index? The last committed index stays searchable until the new scan finishes.",
            "로컬 덱 인덱스를 전체 재구축할까요? 새 스캔이 끝날 때까지 마지막으로 완료된 인덱스는 계속 검색할 수 있습니다."
        ))
        .alignment(Alignment::Center)
        .wrap(Wrap { trim: true })
        .style(crate::ui::popup_style(app, R::TextPrimary)),
        rows[1],
    );
    let segs = [
        buttons::Seg::button(
            MouseTarget::ConfirmLocalFindRebuild,
            t!(" Rebuild (Enter) ", " 재구축 (Enter) "),
        ),
        buttons::Seg::label("    "),
        buttons::Seg::button(
            MouseTarget::CancelLocalFindRebuild,
            t!(" Cancel (Esc) ", " 취소 (Esc) "),
        ),
    ];
    buttons::render_segments(
        frame,
        app,
        rows[2],
        &segs,
        crate::ui::confirm_button_style(app),
        crate::ui::confirm_gap_style(app),
        Alignment::Center,
    );
    crate::ui::seal_popup_background(frame, app, popup);
    crate::ui::mark_art_rows_for_popup(frame, app, popup);
}

fn render_bulk_confirm(frame: &mut Frame, app: &App, area: Rect, confirm: &LocalFindBulkConfirm) {
    let popup = centered_fixed(area, 68, 9);
    if popup.is_empty() {
        return;
    }
    frame.render_widget(Clear, popup);
    crate::ui::render_popup_background(frame, app, popup);
    let block = Block::default()
        .title(t!(" Queue capacity ", " 큐 용량 "))
        .borders(Borders::ALL)
        .border_style(crate::ui::confirm_border_style(app))
        .style(crate::ui::popup_style(app, R::TextPrimary));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(4),
        Constraint::Min(1),
    ])
    .split(inner);
    let action = match confirm.action {
        LocalFindBulkAction::Play => t!("play", "재생"),
        LocalFindBulkAction::Enqueue => t!("add", "추가"),
        LocalFindBulkAction::ShufflePlay => t!("shuffle-play", "셔플 재생"),
    };
    let omitted = confirm
        .track_ids
        .len()
        .saturating_sub(confirm.accepted_count);
    let prompt = if confirm.capacity_recalculated && crate::i18n::is_korean() {
        format!(
            "큐가 바뀌었습니다. 전체 {}곡 중 {}곡을 {}하고 {}곡은 제외할까요?",
            confirm.track_ids.len(),
            confirm.accepted_count,
            action,
            omitted
        )
    } else if confirm.capacity_recalculated {
        format!(
            "Queue changed. {action} {} of {} tracks and omit {omitted}?",
            confirm.accepted_count,
            confirm.track_ids.len()
        )
    } else if crate::i18n::is_korean() {
        format!(
            "전체 {}곡 중 앞의 {}곡을 {}하고 {}곡은 제외할까요?",
            confirm.track_ids.len(),
            confirm.accepted_count,
            action,
            omitted
        )
    } else {
        format!(
            "{action} the first {} of {} tracks and omit {omitted}?",
            confirm.accepted_count,
            confirm.track_ids.len(),
        )
    };
    frame.render_widget(
        Paragraph::new(prompt)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true })
            .style(crate::ui::popup_style(app, R::TextPrimary)),
        rows[1],
    );
    let segs = [
        buttons::Seg::button(
            MouseTarget::ConfirmLocalFindBulk,
            t!(" Continue (Enter) ", " 계속 (Enter) "),
        ),
        buttons::Seg::label("    "),
        buttons::Seg::button(
            MouseTarget::CancelLocalFindBulk,
            t!(" Cancel (Esc) ", " 취소 (Esc) "),
        ),
    ];
    buttons::render_segments(
        frame,
        app,
        rows[2],
        &segs,
        crate::ui::confirm_button_style(app),
        crate::ui::confirm_gap_style(app),
        Alignment::Center,
    );
    crate::ui::seal_popup_background(frame, app, popup);
    crate::ui::mark_art_rows_for_popup(frame, app, popup);
}

fn centered_fixed(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width.saturating_sub(2).max(1));
    let height = height.min(area.height.saturating_sub(2).max(1));
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
    .intersection(area)
}

fn scope_label(scope: LocalFindScope) -> &'static str {
    match scope {
        LocalFindScope::All => t!("All", "전체"),
        LocalFindScope::Tracks => t!("Tracks", "곡"),
        LocalFindScope::Albums => t!("Albums", "앨범"),
        LocalFindScope::Artists => t!("Artists", "아티스트"),
        LocalFindScope::Genres => t!("Genres", "장르"),
        LocalFindScope::Folders => t!("Folders", "폴더"),
        LocalFindScope::Playlists => t!("Playlists", "플레이리스트"),
    }
}

fn scope_short_label(scope: LocalFindScope) -> &'static str {
    match scope {
        LocalFindScope::All => t!("All", "전체"),
        LocalFindScope::Tracks => t!("Tracks", "곡"),
        LocalFindScope::Albums => t!("Albums", "앨범"),
        LocalFindScope::Artists => t!("Artists", "아티스트"),
        LocalFindScope::Genres => t!("Genres", "장르"),
        LocalFindScope::Folders => t!("Folders", "폴더"),
        LocalFindScope::Playlists => t!("Lists", "목록"),
    }
}

fn sort_label(sort: LocalFindSort) -> &'static str {
    match sort {
        LocalFindSort::Relevance => t!("Relevance", "관련도"),
        LocalFindSort::Title => t!("Title", "제목"),
        LocalFindSort::Artist => t!("Artist", "아티스트"),
        LocalFindSort::Album => t!("Album", "앨범"),
        LocalFindSort::Year => t!("Year", "연도"),
        LocalFindSort::Recent => t!("Recent", "최근"),
    }
}

fn format_duration(ms: u64) -> String {
    let total = ms / 1_000;
    let hours = total / 3_600;
    let minutes = (total % 3_600) / 60;
    let seconds = total % 60;
    if hours == 0 {
        format!("{minutes}:{seconds:02}")
    } else {
        format!("{hours}:{minutes:02}:{seconds:02}")
    }
}

#[cfg(test)]
#[path = "local_find_tests.rs"]
mod tests;
