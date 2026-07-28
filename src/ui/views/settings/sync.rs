//! Presentation-only renderer for the Sync settings tab.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{HighlightSpacing, List, ListItem, ListState, Paragraph, Wrap};

use crate::app::App;
use crate::app::MouseTarget;
use crate::settings::SettingsState;
use crate::settings::sync::{
    SyncAuditRow, SyncDeviceRow, SyncMergeSummary, SyncRow, SyncSettingsModel, audit_action_label,
    audit_outcome_label, failure_label, failure_recovery_label, health_label,
};
use crate::sync::{SyncAuditOutcome, SyncHealthState};
use crate::t;
use crate::theme::ThemeRole as R;

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
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(2),
        Constraint::Min(0),
    ])
    .split(area);

    let state_role = match model.health {
        SyncHealthState::Off => R::TextMuted,
        SyncHealthState::UpToDate => R::Success,
        SyncHealthState::Syncing => R::Accent,
        SyncHealthState::OfflineWillRetry => R::Warning,
        SyncHealthState::NeedsAttention => R::Error,
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                model.page.title(),
                theme.style(R::SettingsGroup).add_modifier(Modifier::BOLD),
            ),
            Span::styled("  ", theme.style(R::TextMuted)),
            Span::styled(health_label(model.health), theme.style(state_role)),
        ])),
        rows[0],
    );

    let detail = model.failure.map_or_else(
        || Line::from(model.page.description()).style(theme.style(R::TextMuted)),
        |failure| {
            Line::from(vec![
                Span::styled(failure_label(failure), theme.style(R::Error)),
                Span::styled("  ·  ", theme.style(R::TextMuted)),
                Span::styled(
                    failure_recovery_label(failure),
                    theme.style(R::SettingsValueFocused),
                ),
            ])
        },
    );
    frame.render_widget(Paragraph::new(detail).wrap(Wrap { trim: true }), rows[1]);

    let items: Vec<ListItem<'static>> = model
        .rows
        .iter()
        .map(|row| render_row(row, model.busy, settings))
        .collect();
    let selected = model.selected();
    let list = List::new(items)
        .style(theme.style(R::TextPrimary))
        .highlight_style(
            theme
                .style(R::SettingsValueFocused)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ")
        .highlight_spacing(HighlightSpacing::Always);
    let offset = selected.map_or_else(
        || {
            app.bridges
                .settings_scroll
                .view(rows[2].height, model.rows.len())
        },
        |selected| {
            app.bridges
                .settings_scroll
                .resolve(selected, rows[2].height, model.rows.len(), 0)
        },
    );
    let mut state = ListState::default().with_offset(offset);
    state.select(selected);
    frame.render_stateful_widget(list, rows[2], &mut state);
    for visible in 0..rows[2].height {
        let row = state.offset() + visible as usize;
        if row >= model.rows.len() {
            break;
        }
        app.register_mouse_button(
            Rect {
                x: rows[2].x,
                y: rows[2].y + visible,
                width: rows[2].width,
                height: 1,
            },
            MouseTarget::SettingsSyncRow(row),
        );
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
