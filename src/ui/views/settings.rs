//! The settings view: a tab bar over a list of editable
//! field rows. State lives in `App.settings`; like the library view, the `ListState`
//! that highlights the focused row is rebuilt fresh each frame from a `usize` index.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Tabs};

use crate::app::App;
use crate::keymap::{self, Action, KeyContext};
use crate::settings::{Field, FieldKind, SettingsState, SettingsTab};
use crate::settings::{BAND_GAIN_MAX, BAND_GAIN_MIN};
use crate::config::{SPEED_MAX, SPEED_MIN};
use crate::theme::ThemeRole as R;

pub fn render(frame: &mut Frame, app: &App, area: Rect) {
    // No screen without state — but render defensively rather than panic.
    let Some(st) = app.settings.as_deref() else {
        return;
    };
    let theme = &st.draft.theme;
    let block = Block::default()
        .title(" Settings ")
        .borders(Borders::ALL)
        .border_style(theme.style(R::BorderPrimary))
        .style(theme.style(R::TextPrimary));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = Layout::vertical([
        Constraint::Length(1), // tab bar
        Constraint::Length(1), // spacer
        Constraint::Min(0),    // field list
        Constraint::Length(1), // help
    ])
    .split(inner);

    render_tabs(frame, st, rows[0]);
    if st.tab == SettingsTab::Keys {
        render_keys(frame, st, rows[2]);
    } else {
        render_fields(frame, st, rows[2]);
    }

    // Footer reflects the *committed* keymap, since that's what operates the screen until
    // the edits are saved.
    let k = |a| app.keymap.label(KeyContext::Settings, a);
    let help_text = if st.editing_text && st.tab == SettingsTab::Colors {
        "type #RRGGBB · Enter save · Backspace delete".to_owned()
    } else if st.editing_text {
        // While typing a path/key, Enter or Esc both commit *and* persist it immediately,
        // so the value can't be lost by leaving the screen later.
        "type value · Enter or Esc save · Backspace delete".to_owned()
    } else if st.tab == SettingsTab::Keys {
        format!(
            "{}/{} select · {} rebind · {} reset · {} switch tab · {}/{} save+close",
            k(Action::MoveUp),
            k(Action::MoveDown),
            k(Action::Confirm),
            k(Action::DeleteChar),
            k(Action::FocusNext),
            k(Action::SettingsSave),
            k(Action::SettingsCancel),
        )
    } else if st.tab == SettingsTab::Colors {
        format!(
            "{}/{} color · {} edit · {} reset · {} switch tab · {}/{} save+close",
            k(Action::MoveUp),
            k(Action::MoveDown),
            k(Action::Confirm),
            k(Action::DeleteChar),
            k(Action::FocusNext),
            k(Action::SettingsSave),
            k(Action::SettingsCancel),
        )
    } else {
        format!(
            "{}/{} field · {}/{} change · {} edit/toggle · {} switch tab · {}/{} save+close",
            k(Action::MoveUp),
            k(Action::MoveDown),
            k(Action::ChangeDecrease),
            k(Action::ChangeIncrease),
            k(Action::Confirm),
            k(Action::FocusNext),
            k(Action::SettingsSave),
            k(Action::SettingsCancel),
        )
    };
    frame.render_widget(Paragraph::new(Line::from(help_text).style(theme.style(R::TextMuted))), rows[3]);
}

/// The Keys tab: a scrollable list of every remappable binding, grouped by context. The
/// chord shown is from the *draft* keymap so edits appear immediately; the row being
/// rebound shows a capture prompt.
fn render_keys(frame: &mut Frame, st: &SettingsState, area: Rect) {
    let theme = &st.draft.theme;
    let entries = keymap::editable_entries();
    let mut prev_ctx: Option<KeyContext> = None;
    let items: Vec<ListItem> = entries
        .iter()
        .enumerate()
        .map(|(i, &(ctx, action))| {
            let key = if st.capturing == Some((ctx, action)) {
                "<press a key…>".to_owned()
            } else {
                st.keymap.chord(ctx, action).map_or_else(|| "—".to_owned(), keymap::format_chord)
            };
            // A context header label, shown once at the start of each group. Always keep at
            // least one space before the action column: a couple of titles ("Navigation (all
            // screens)", "Search results") are wider than the padding column and would
            // otherwise run straight into the white action label.
            let group = if prev_ctx != Some(ctx) { ctx.title() } else { "" };
            prev_ctx = Some(ctx);
            let group_cell =
                if group.is_empty() { format!("{group:<13}") } else { format!("{group:<12} ") };
            let focused = i == st.row;
            let key_role = if focused { R::SettingsValueFocused } else { R::SettingsValue };
            ListItem::new(Line::from(vec![
                Span::styled(group_cell, theme.style(R::SettingsGroup)),
                Span::styled(format!("{:<24}", action.human_label_for(ctx)), theme.style(R::SettingsLabel)),
                Span::styled(key, theme.style(key_role)),
            ]))
        })
        .collect();

    let list = List::new(items)
        .style(theme.style(R::TextPrimary))
        .highlight_style(theme.style(R::SettingsValueFocused).add_modifier(Modifier::BOLD))
        .highlight_symbol("▶ ");

    let mut state = ListState::default();
    state.select(Some(st.row.min(entries.len().saturating_sub(1))));
    frame.render_stateful_widget(list, area, &mut state);
}

fn render_tabs(frame: &mut Frame, st: &SettingsState, area: Rect) {
    let theme = &st.draft.theme;
    let titles: Vec<&str> = crate::settings::SettingsTab::ALL.iter().map(|t| t.label()).collect();
    let tabs = Tabs::new(titles)
        .select(st.tab.index())
        .style(theme.style(R::TextMuted))
        .highlight_style(
            Style::default()
                .fg(theme.color(R::SelectionFg))
                .bg(theme.color(R::SelectionBg))
                .add_modifier(Modifier::BOLD),
        )
        .divider(" ");
    frame.render_widget(tabs, area);
}

fn render_fields(frame: &mut Frame, st: &SettingsState, area: Rect) {
    let fields = st.tab.fields();
    let items: Vec<ListItem> = fields
        .iter()
        .enumerate()
        .map(|(i, &field)| field_row(st, field, i == st.row))
        .collect();

    let list = List::new(items)
        .style(st.draft.theme.style(R::TextPrimary))
        .highlight_style(st.draft.theme.style(R::SettingsValueFocused).add_modifier(Modifier::BOLD))
        .highlight_symbol("▶ ");

    let mut state = ListState::default();
    state.select(Some(st.row.min(fields.len().saturating_sub(1))));
    frame.render_stateful_widget(list, area, &mut state);
}

/// One field row: a left-aligned label and its current value (with a slider bar for
/// numeric fields and a caret for the text field being edited).
fn field_row<'a>(st: &SettingsState, field: Field, focused: bool) -> ListItem<'a> {
    let theme = &st.draft.theme;
    let label = format!("{:<22}", field.label());
    if let Field::ThemeColor(role) = field {
        let value = if focused && st.editing_text {
            st.draft.text_value(field).unwrap_or_default().to_owned()
        } else {
            st.draft.value_display(field)
        };
        let value_role = if focused { R::SettingsValueFocused } else { R::SettingsValue };
        return ListItem::new(Line::from(vec![
            Span::styled(label, theme.style(R::SettingsLabel)),
            Span::styled("  ", Style::default().bg(theme.color(role))),
            Span::raw("  "),
            Span::styled(format!("{:<9}", value), theme.style(value_role)),
            Span::styled(role.description().to_owned(), theme.style(R::TextMuted)),
        ]));
    }
    let value = match (field, field.kind()) {
        // Secret fields (API key) are never shown in clear text; while editing, render a
        // masked buffer of *that field's* typed length so keystrokes still register visibly.
        (f, _) if f.is_secret() && focused && st.editing_text => {
            let len = st.draft.text_value(field).map_or(0, |s| s.chars().count());
            format!("{}▏", "•".repeat(len))
        }
        // Show the live edit buffer with a caret for the focused text field.
        (Field::CookiesFile | Field::DownloadDir, _) if focused && st.editing_text => {
            format!("{}▏", st.draft.text_value(field).unwrap_or_default())
        }
        (Field::Speed, _) => {
            format!("{}  {:.1}x", bar(st.draft.speed, SPEED_MIN, SPEED_MAX), st.draft.speed)
        }
        (Field::Band(i), _) => {
            format!("{}  {:+.0} dB", bar(st.draft.eq_bands[i], BAND_GAIN_MIN, BAND_GAIN_MAX), st.draft.eq_bands[i])
        }
        (_, FieldKind::Select) => format!("< {} >", st.draft.value_display(field)),
        _ => st.draft.value_display(field),
    };
    let value_role = if focused { R::SettingsValueFocused } else { R::SettingsValue };
    ListItem::new(Line::from(vec![
        Span::styled(label, theme.style(R::SettingsLabel)),
        Span::styled(value, theme.style(value_role)),
    ]))
}

/// A compact 11-cell slider bar with a marker at `value`'s position in `[min, max]`.
fn bar(value: f64, min: f64, max: f64) -> String {
    const WIDTH: usize = 11;
    let frac = if max > min { ((value - min) / (max - min)).clamp(0.0, 1.0) } else { 0.0 };
    let pos = (frac * (WIDTH - 1) as f64).round() as usize;
    let mut s = String::with_capacity(WIDTH);
    for i in 0..WIDTH {
        s.push(if i == pos { '●' } else { '─' });
    }
    s
}
