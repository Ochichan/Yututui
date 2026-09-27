//! Presentation-only renderer for the Sync settings tab.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{HighlightSpacing, List, ListItem, ListState};

use crate::app::App;
use crate::app::{MouseTarget, ScrollSurface};
use crate::settings::SettingsState;
use crate::settings::sync::{
    SyncAuditRow, SyncDeviceRow, SyncMergeSummary, SyncRow, SyncSettingsModel, audit_action_label,
    audit_outcome_label, failure_label, failure_recovery_label, health_label,
};
use crate::sync::{SyncAuditOutcome, SyncHealthState};
use crate::t;
use crate::theme::ThemeRole as R;
use crate::ui::buttons;

/// Render a privacy-safe Sync snapshot. Form inputs and connection secrets are intentionally
/// absent from [`SyncSettingsModel`] and should be rendered by their owning modal.
pub(crate) fn render_sync(
    frame: &mut Frame,
    app: &App,
    settings: &SettingsState,
    model: &SyncSettingsModel,
    area: Rect,
) {
    if area.is_empty() {
        return;
    }
    let theme = &settings.draft.theme;
    let state_role = health_role(model.health);
    let mut head = vec![Line::from(vec![
        Span::styled(
            model.page.title(),
            theme.style(R::SettingsGroup).add_modifier(Modifier::BOLD),
        ),
        Span::styled("  ", theme.style(R::TextMuted)),
        // The dot repeats the state in color so it reads before the words do.
        Span::styled("● ", theme.style(state_role)),
        Span::styled(health_label(model.health), theme.style(state_role)),
    ])];
    // The page description, or the failure and its recovery step on separate lines.
    let width = pane_text_width(area);
    match model.failure {
        None => head.extend(wrap_lines(
            model.page.description(),
            width,
            theme.style(R::TextMuted),
        )),
        Some(failure) => {
            head.extend(wrap_lines(
                failure_label(failure),
                width,
                theme.style(R::Error),
            ));
            head.extend(wrap_lines(
                &format!("› {}", failure_recovery_label(failure)),
                width,
                theme.style(R::SettingsValueFocused),
            ));
        }
    }
    let actions = model
        .rows
        .iter()
        .map(|row| render_row(row, model.busy, settings))
        .collect();
    render_pane(
        frame,
        app,
        settings,
        area,
        head,
        actions,
        model.selected(),
        MouseTarget::SettingsSyncRow,
    );
}

/// Cells a Sync pane's text can use: the pane minus the list's marker gutter.
pub(super) fn pane_text_width(area: Rect) -> usize {
    usize::from(area.width).saturating_sub(2).max(1)
}

/// `text` wrapped whole to `width` cells, one styled line per row.
pub(super) fn wrap_lines(text: &str, width: usize, style: Style) -> Vec<Line<'static>> {
    crate::ui::text::wrap_to_width(text, width)
        .into_iter()
        .map(|part| Line::from(Span::styled(part, style)))
        .collect()
}

/// Draw a Sync pane as one scrolling list: the `head` text rows, a blank row, then the
/// `actions`. Keeping text and actions in one list means nothing is clipped on a short
/// terminal: the wheel and scrollbar reach every row, and keyboard focus scrolls its action
/// just into view (at the bottom edge when it starts below), keeping as much of the text above
/// it as fits. Visible actions publish `target(index)` click rects.
#[allow(clippy::too_many_arguments)]
pub(super) fn render_pane(
    frame: &mut Frame,
    app: &App,
    settings: &SettingsState,
    area: Rect,
    head: Vec<Line<'static>>,
    actions: Vec<ListItem<'static>>,
    selected: Option<usize>,
    target: impl Fn(usize) -> MouseTarget,
) {
    let theme = &settings.draft.theme;
    let action_count = actions.len();
    let first_action = if action_count > 0 && !head.is_empty() {
        head.len() + 1
    } else {
        head.len()
    };
    let mut items: Vec<ListItem<'static>> = head.into_iter().map(ListItem::new).collect();
    if first_action > items.len() {
        items.push(ListItem::new(Line::default()));
    }
    items.extend(actions);
    let len = items.len();
    app.bridges.settings_list_len.set(Some(len));
    let selected = selected
        .filter(|_| action_count > 0)
        .map(|index| first_action + index.min(action_count - 1));
    app.bridges
        .settings_focus
        .set(selected.map(|row| (first_action, row, row)));
    let offset = match selected {
        Some(row) => super::resolve_keeping_cursor_on_resize(
            &app.bridges.settings_scroll,
            row,
            area.height,
            len,
            0,
        ),
        None => app.bridges.settings_scroll.view(area.height, len),
    };
    let list = List::new(items)
        .style(theme.style(R::TextPrimary))
        .highlight_style(
            theme
                .style(R::SettingsValueFocused)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ")
        .highlight_spacing(HighlightSpacing::Always);
    // Select only while the focused action is inside the window: ratatui scrolls any
    // selection back into view (overriding a wheel offset that moved past it), and
    // `select(None)` resets the offset to 0.
    let mut state = ListState::default().with_offset(offset);
    if let Some(row) = selected
        && (offset..offset + usize::from(area.height)).contains(&row)
    {
        state.select(Some(row));
    }
    frame.render_stateful_widget(list, area, &mut state);
    let offset = state.offset();
    for visible in 0..area.height {
        let Some(action) = (offset + usize::from(visible)).checked_sub(first_action) else {
            continue;
        };
        if action >= action_count {
            break;
        }
        app.register_mouse_button(
            Rect {
                x: area.x,
                y: area.y + visible,
                width: area.width,
                height: 1,
            },
            target(action),
        );
    }
    // A scrollbar on the frame border, like the field tabs; a no-op when every row fits.
    buttons::render_list_scrollbar(
        frame,
        app,
        Rect {
            x: area.right(),
            y: area.y,
            width: 1,
            height: area.height,
        },
        ScrollSurface::Settings,
        len,
        offset,
        usize::from(area.height),
    );
}

/// The color for a personal-sync health state, shared by every Sync pane that shows it.
pub(super) fn health_role(health: SyncHealthState) -> R {
    match health {
        SyncHealthState::Off => R::TextMuted,
        SyncHealthState::UpToDate => R::Success,
        SyncHealthState::Syncing => R::Accent,
        SyncHealthState::OfflineWillRetry => R::Warning,
        SyncHealthState::NeedsAttention => R::Error,
    }
}

fn render_row(row: &SyncRow, busy: bool, settings: &SettingsState) -> ListItem<'static> {
    let theme = &settings.draft.theme;
    match row {
        SyncRow::Action(action) => {
            let role = if busy { R::TextMuted } else { R::SettingsValue };
            ListItem::new(Line::from(vec![
                Span::styled("  ↵ ", theme.style(R::SettingsLabel)),
                Span::styled(action.label().to_owned(), theme.style(role)),
            ]))
        }
        SyncRow::Device(device) => device_row(device, settings),
        SyncRow::Audit(audit) => audit_row(audit, settings),
        SyncRow::MergeSummary(summary) => merge_row(*summary, settings),
        SyncRow::Notice(notice) => ListItem::new(Line::from(vec![
            Span::styled("  • ", theme.style(R::SettingsLabel)),
            Span::styled(notice.label().to_owned(), theme.style(R::TextMuted)),
        ])),
    }
}

fn device_row(device: &SyncDeviceRow, settings: &SettingsState) -> ListItem<'static> {
    let theme = &settings.draft.theme;
    let marker = match (device.current, device.active) {
        (true, true) => t!("this device", "이 기기", "このデバイス"),
        (_, true) => t!("connected", "연결됨", "接続済み"),
        (_, false) => t!("removed", "제거됨", "削除済み"),
    };
    ListItem::new(Line::from(vec![
        Span::styled("  ", theme.style(R::SettingsLabel)),
        Span::styled(
            device.name().to_owned(),
            theme.style(R::SettingsValue).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("  {marker}  "), theme.style(R::TextMuted)),
        Span::styled(device.fingerprint().to_owned(), theme.style(R::TextSubtle)),
    ]))
}

fn audit_row(audit: &SyncAuditRow, settings: &SettingsState) -> ListItem<'static> {
    let theme = &settings.draft.theme;
    let outcome_role = match audit.outcome {
        SyncAuditOutcome::Succeeded => R::Success,
        SyncAuditOutcome::NoChanges => R::TextMuted,
        SyncAuditOutcome::Failed => R::Error,
    };
    let changes = audit.local_changes.saturating_add(audit.remote_changes);
    let suffix = if changes == 0 {
        String::new()
    } else {
        format!("  ·  {} {changes}", t!("changes", "변경", "件の変更"))
    };
    let mut spans = vec![
        Span::styled("  ", theme.style(R::SettingsLabel)),
        Span::styled(
            audit_action_label(audit.action).to_owned(),
            theme.style(R::SettingsValue),
        ),
        Span::styled("  ·  ", theme.style(R::TextMuted)),
        Span::styled(
            audit_outcome_label(audit.outcome).to_owned(),
            theme.style(outcome_role),
        ),
        Span::styled(suffix, theme.style(R::TextMuted)),
    ];
    if let Some(failure) = audit.failure {
        spans.extend([
            Span::styled("  ·  ", theme.style(R::TextMuted)),
            Span::styled(failure_label(failure).to_owned(), theme.style(R::Error)),
        ]);
    }
    ListItem::new(Line::from(spans))
}

fn merge_row(summary: SyncMergeSummary, settings: &SettingsState) -> ListItem<'static> {
    let theme = &settings.draft.theme;
    let text = format!(
        "{} {}  ·  {} {}  ·  {} {}",
        t!("This device", "이 기기", "このデバイス"),
        summary.local_changes,
        t!("Other devices", "다른 기기", "ほかのデバイス"),
        summary.remote_changes,
        t!("Already present", "이미 있음", "既に存在"),
        summary.duplicates_skipped,
    );
    ListItem::new(Line::from(Span::styled(
        format!("  {text}"),
        theme.style(R::SettingsValue),
    )))
}
