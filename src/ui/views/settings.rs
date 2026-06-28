//! The settings view: a tab bar over a list of editable
//! field rows. State lives in `App.settings`; like the library view, the `ListState`
//! that highlights the focused row is rebuilt fresh each frame from a `usize` index.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};

use crate::app::{App, MouseTarget};
use crate::keymap::{self, Action, Conflict, KeyContext};
use crate::settings::{Field, FieldKind, SettingsState, SettingsTab};
use crate::settings::{BAND_GAIN_MAX, BAND_GAIN_MIN};
use crate::config::{SEEK_SECONDS_MAX, SEEK_SECONDS_MIN, SPEED_MAX, SPEED_MIN};
use crate::theme::ThemeRole as R;
use crate::ui::buttons;

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
        Constraint::Length(1), // nav bar
        Constraint::Length(1), // tab bar
        Constraint::Length(1), // spacer
        Constraint::Min(0),    // field list
        Constraint::Length(1), // help
    ])
    .split(inner);

    buttons::render_nav(frame, app, rows[0]);
    render_tabs(frame, app, st, rows[1]);
    if st.tab == SettingsTab::Keys {
        render_keys(frame, app, st, rows[3]);
    } else {
        render_fields(frame, app, st, rows[3]);
    }

    // Footer reflects the *committed* keymap, since that's what operates the screen until
    // the edits are saved.
    let k = |a| app.keymap.label(KeyContext::Settings, a);
    let help_text = if st.editing_text && st.tab == SettingsTab::Colors {
        "type #RRGGBB or none  ·  Enter save  ·  Backspace delete".to_owned()
    } else if st.editing_text {
        // While typing a path/key, Enter or Esc both commit *and* persist it immediately,
        // so the value can't be lost by leaving the screen later.
        "type value  ·  Enter or Esc save  ·  Backspace delete".to_owned()
    } else if st.tab == SettingsTab::Keys {
        format!(
            "{}/{} select  ·  {} rebind  ·  {} reset  ·  {} switch tab  ·  {} save + quit",
            k(Action::MoveUp),
            k(Action::MoveDown),
            k(Action::Confirm),
            k(Action::DeleteChar),
            k(Action::FocusNext),
            k(Action::SettingsCancel),
        )
    } else if st.tab == SettingsTab::Colors {
        format!(
            "{}/{} color  ·  {} edit  ·  {} reset  ·  {} switch tab  ·  {} save + quit",
            k(Action::MoveUp),
            k(Action::MoveDown),
            k(Action::Confirm),
            k(Action::DeleteChar),
            k(Action::FocusNext),
            k(Action::SettingsCancel),
        )
    } else {
        format!(
            "{}/{} field  ·  {}/{} change  ·  {} edit/toggle  ·  {} switch tab  ·  {} save + quit",
            k(Action::MoveUp),
            k(Action::MoveDown),
            k(Action::ChangeDecrease),
            k(Action::ChangeIncrease),
            k(Action::Confirm),
            k(Action::FocusNext),
            k(Action::SettingsCancel),
        )
    };
    frame.render_widget(Paragraph::new(Line::from(help_text).style(theme.style(R::TextMuted))), rows[4]);
}

/// The Keys tab: a scrollable list of every remappable binding, grouped by context. The
/// chord shown is from the *draft* keymap so edits appear immediately; the row being
/// rebound shows a capture prompt.
fn render_keys(frame: &mut Frame, app: &App, st: &SettingsState, area: Rect) {
    let theme = &st.draft.theme;
    let entries = keymap::editable_entries();
    // Each group gets its own header row (blank line before it, except the first), with the
    // bindings listed underneath. The rendered list therefore holds more rows than `entries`,
    // so map the focused binding index (`st.row`) to its display position for selection.
    let mut items: Vec<ListItem> = Vec::with_capacity(entries.len() + 24);
    // Parallel to `items`: the binding index a display row maps to, or `None` for the
    // header/blank rows. Used to turn a click on a visible row back into a binding index.
    let mut display_to_binding: Vec<Option<usize>> = Vec::with_capacity(entries.len() + 24);
    let mut selected_display = 0usize;
    let mut prev_ctx: Option<KeyContext> = None;
    for (i, &(ctx, action)) in entries.iter().enumerate() {
        if prev_ctx != Some(ctx) {
            items.push(ListItem::new(Line::from(Span::styled(
                ctx.title().to_owned(),
                theme.style(R::SettingsGroup).add_modifier(Modifier::BOLD),
            ))));
            display_to_binding.push(None);
            prev_ctx = Some(ctx);
        }
        let focused = i == st.row;
        if focused {
            selected_display = items.len();
        }
        let key = if st.capturing == Some((ctx, action)) {
            "<press a key…>".to_owned()
        } else {
            st.keymap.chord(ctx, action).map_or_else(|| "—".to_owned(), keymap::format_chord)
        };
        let key_role = if focused { R::SettingsValueFocused } else { R::SettingsValue };
        // Bindings indent one step in from their group header for a clear hierarchy.
        items.push(ListItem::new(Line::from(vec![
            Span::styled(
                format!("  {:<24}", action.human_label_for(ctx)),
                theme.style(R::SettingsLabel),
            ),
            Span::styled(key, theme.style(key_role)),
        ])));
        display_to_binding.push(Some(i));
        // A little padding after each group so the sections breathe.
        if entries.get(i + 1).map(|(c, _)| *c) != Some(ctx) {
            items.push(ListItem::new(Line::from("")));
            display_to_binding.push(None);
        }
    }

    let list = List::new(items)
        .style(theme.style(R::TextPrimary))
        .highlight_style(theme.style(R::SettingsValueFocused).add_modifier(Modifier::BOLD))
        .highlight_symbol("▶ ");

    let mut state = ListState::default();
    state.select(Some(selected_display));
    frame.render_stateful_widget(list, area, &mut state);
    // Clicking a binding row selects that binding; header/blank rows aren't targets.
    buttons::register_list_rows(app, area, state.offset(), display_to_binding.len(), |d| {
        display_to_binding.get(d).copied().flatten()
    });
}

fn render_tabs(frame: &mut Frame, app: &App, st: &SettingsState, area: Rect) {
    let theme = &st.draft.theme;
    let active = Style::default()
        .fg(theme.color(R::SelectionFg))
        .bg(theme.color(R::SelectionBg))
        .add_modifier(Modifier::BOLD);
    let muted = theme.style(R::TextMuted);
    // A single space between tabs (matching the old `Tabs` divider). Each tab cell is
    // " label " (one cell of padding each side, as `Tabs` padded), and its hit rect is
    // computed by walking the x cursor with the same width math ratatui lays the spans with.
    const DIVIDER: &str = " ";
    let mut spans = Vec::new();
    let mut x = area.x;
    for (i, t) in crate::settings::SettingsTab::ALL.iter().copied().enumerate() {
        if i > 0 {
            spans.push(Span::styled(DIVIDER, muted));
            x = x.saturating_add(buttons::text_width(DIVIDER));
        }
        let label = format!(" {} ", t.label());
        let w = buttons::text_width(&label);
        app.register_mouse_button(
            Rect { x, y: area.y, width: w, height: 1 },
            MouseTarget::SettingsTab(i),
        );
        x = x.saturating_add(w);
        let style = if st.tab == t { active } else { muted };
        spans.push(Span::styled(label, style));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_fields(frame: &mut Frame, app: &App, st: &SettingsState, area: Rect) {
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
    // Each field row maps 1:1 to its index, so a click selects that row directly.
    buttons::register_list_rows(app, area, state.offset(), fields.len(), Some);
}

/// One field row: a left-aligned label and its current value (with a slider bar for
/// numeric fields and a caret for the text field being edited).
fn field_row<'a>(st: &SettingsState, field: Field, focused: bool) -> ListItem<'a> {
    let theme = &st.draft.theme;
    if let Field::ThemeColor(role) = field {
        // Pad every role label to the widest one so the swatch, hex value, and description
        // columns line up regardless of how long the role name is (role labels are ASCII,
        // so byte length equals display width here).
        let label_w = R::ALL.iter().map(|r| r.label().len()).max().unwrap_or(22);
        let label = format!("{:<label_w$}", role.label());
        let value = if focused && st.editing_text {
            st.draft.text_value(field).unwrap_or_default().to_owned()
        } else {
            st.draft.value_display(field)
        };
        let value_role = if focused { R::SettingsValueFocused } else { R::SettingsValue };
        // Transparent roles have no fill — show a hatched marker so it reads as "terminal
        // background shows through" rather than a missing/black swatch.
        let swatch = if theme.is_role_transparent(role) {
            Span::styled("▒▒", theme.style(R::TextMuted))
        } else {
            Span::styled("  ", Style::default().bg(theme.color(role)))
        };
        return ListItem::new(Line::from(vec![
            Span::styled(label, theme.style(R::SettingsLabel)),
            Span::raw(" "),
            swatch,
            Span::raw("  "),
            Span::styled(format!("{:<9}", value), theme.style(value_role)),
            Span::styled(role.description().to_owned(), theme.style(R::TextMuted)),
        ]));
    }
    // Pad every label in this tab to the widest one (+ a 2-space gutter, min 22) so the value
    // column lines up regardless of label length — e.g. "Album art (next launch)" no longer
    // pushes its value out of alignment with the shorter rows above it, and even that widest
    // label keeps a gap before its value. Labels are ASCII here, so char count == width.
    let label_w = st
        .tab
        .fields()
        .iter()
        .filter(|f| !matches!(f, Field::ThemeColor(_)))
        .map(|f| f.label().chars().count())
        .max()
        .unwrap_or(20)
        .max(20)
        + 2;
    let label = format!("{:<label_w$}", field.label());
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
        (Field::SeekInterval, _) => format!(
            "{}  {:.0}s",
            bar(st.draft.seek_seconds, SEEK_SECONDS_MIN, SEEK_SECONDS_MAX),
            st.draft.seek_seconds
        ),
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

/// A modal warning shown when a rebind on the Keys tab collides with an existing binding.
/// Names the offending chord, the action already holding it, and where it lives, so the
/// failure is loud and actionable rather than a silently-dropped remap.
pub fn render_conflict(frame: &mut Frame, app: &App, area: Rect, conflict: &Conflict) {
    let theme = &app.theme;
    let popup = centered_fixed(area, 54, 9);
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(" ⚠ Keybinding conflict ")
        .borders(Borders::ALL)
        .border_style(theme.style(R::Warning).add_modifier(Modifier::BOLD))
        .style(theme.style(R::TextPrimary));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let chord = keymap::format_chord(conflict.chord);
    let lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled(chord, theme.style(R::Warning).add_modifier(Modifier::BOLD)),
            Span::raw(" is already bound to"),
        ]),
        Line::from(Span::styled(
            format!("\u{201c}{}\u{201d}", conflict.existing.human_label_for(conflict.ctx)),
            theme.style(R::Accent).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(format!("in {}", conflict.ctx.title()), theme.style(R::TextMuted))),
        Line::from(""),
        Line::from(Span::styled(
            "Binding unchanged · press any key",
            theme.style(R::TextMuted),
        )),
    ];
    frame.render_widget(
        Paragraph::new(lines).alignment(Alignment::Center).style(theme.style(R::TextPrimary)),
        inner,
    );
}

/// A modal confirmation for the "reset all settings" button. Resetting wipes every setting
/// (keybindings, theme, API key included) and the screen always persists on close, so the
/// destructive action is gated behind an explicit yes/no rather than a single keypress.
pub fn render_confirm_reset(frame: &mut Frame, app: &App, area: Rect) {
    let theme = &app.theme;
    let popup = centered_fixed(area, 56, 9);
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(" ⚠ Reset all settings ")
        .borders(Borders::ALL)
        .border_style(theme.style(R::Error).add_modifier(Modifier::BOLD))
        .style(theme.style(R::TextPrimary));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let lines = vec![
        Line::from(""),
        Line::from("Restore every setting to its default?"),
        Line::from(Span::styled(
            "Keybindings, theme, and API key included.",
            theme.style(R::TextMuted),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("Enter / y", theme.style(R::Error).add_modifier(Modifier::BOLD)),
            Span::raw(" reset    "),
            Span::styled("Esc", theme.style(R::Accent).add_modifier(Modifier::BOLD)),
            Span::raw(" cancel"),
        ]),
    ];
    frame.render_widget(
        Paragraph::new(lines).alignment(Alignment::Center).style(theme.style(R::TextPrimary)),
        inner,
    );
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
