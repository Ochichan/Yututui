//! The Hotkeys tab: every remappable key binding, then the mouse gestures, grouped by context
//! and balanced into two scrolling columns.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{HighlightSpacing, List, ListItem, ListState};
use unicode_width::UnicodeWidthStr;

use super::HL_SYMBOL;
use crate::app::App;
use crate::keymap::{self, Action, KeyContext};
use crate::settings::SettingsState;
use crate::t;
use crate::theme::ThemeConfig;
use crate::theme::ThemeRole as R;
use crate::ui::buttons;
use crate::ui::text::pad_to_width;

#[derive(Clone, Copy)]
enum EditableBinding {
    Key {
        logical: usize,
        context: KeyContext,
        action: Action,
    },
    Mouse {
        logical: usize,
        context: crate::mousemap::MouseContext,
        gesture: crate::mousemap::MouseGesture,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BindingGroupKind {
    Key(KeyContext),
    Mouse(crate::mousemap::MouseContext),
}

struct BindingGroup {
    kind: BindingGroupKind,
    rows: Vec<EditableBinding>,
}

impl BindingGroup {
    fn title(&self) -> String {
        match self.kind {
            BindingGroupKind::Key(context) => context.title().to_owned(),
            BindingGroupKind::Mouse(context) => {
                format!("{} · {}", t!("Mouse", "마우스", "マウス"), context.title())
            }
        }
    }
}

/// The Keys tab: a scrollable list of every remappable binding, grouped by context. The
/// chord shown is from the *draft* keymap so edits appear immediately; the row being rebound
/// shows a capture prompt. Eight safe right-button gesture presets follow the keyboard rows.
pub(super) fn render_keys(frame: &mut Frame, app: &App, st: &SettingsState, area: Rect) {
    let theme = &st.draft.theme;
    let entries = keymap::editable_entries();

    // Group consecutive keyboard bindings by context, then append one two-row mouse group per
    // semantic surface. Whole groups stay together when the list is balanced into two columns.
    let mut groups: Vec<BindingGroup> = Vec::new();
    for (logical, &(context, action)) in entries.iter().enumerate() {
        let row = EditableBinding::Key {
            logical,
            context,
            action,
        };
        match groups.last_mut() {
            Some(group) if group.kind == BindingGroupKind::Key(context) => group.rows.push(row),
            _ => groups.push(BindingGroup {
                kind: BindingGroupKind::Key(context),
                rows: vec![row],
            }),
        }
    }
    let key_count = entries.len();
    for (context_index, context) in crate::mousemap::MouseContext::ALL.into_iter().enumerate() {
        let rows = crate::mousemap::MouseGesture::ALL
            .into_iter()
            .enumerate()
            .map(|(gesture_index, gesture)| EditableBinding::Mouse {
                logical: key_count
                    + context_index * crate::mousemap::MouseGesture::ALL.len()
                    + gesture_index,
                context,
                gesture,
            })
            .collect();
        groups.push(BindingGroup {
            kind: BindingGroupKind::Mouse(context),
            rows,
        });
    }

    // Break at the whole-group boundary that most closely balances rendered height. A blank
    // line separates groups and is counted here exactly as it is drawn below.
    let height = |group: &BindingGroup| group.rows.len() + 2;
    let total: usize = groups.iter().map(height).sum();
    let (mut split, mut acc, mut best) = (groups.len(), 0usize, usize::MAX);
    for (group_index, group) in groups.iter().enumerate() {
        acc += height(group);
        let diff = acc.abs_diff(total - acc);
        if diff < best {
            best = diff;
            split = group_index + 1;
        }
    }
    let split = split.min(groups.len());
    let label_width = groups
        .iter()
        .flat_map(|group| group.rows.iter())
        .map(|row| match row {
            EditableBinding::Key {
                context, action, ..
            } => UnicodeWidthStr::width(action.human_label_for(*context)),
            EditableBinding::Mouse { gesture, .. } => UnicodeWidthStr::width(gesture.human_label()),
        })
        .max()
        .unwrap_or(22)
        .max(22)
        + 2;

    let columns =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(area);
    for (column_index, slice) in [&groups[..split], &groups[split..]].into_iter().enumerate() {
        // A 2-cell gutter between the columns keeps the left labels off the right block.
        let col = columns[column_index];
        let col = if column_index == 0 {
            Rect {
                width: col.width.saturating_sub(2),
                ..col
            }
        } else {
            col
        };
        let (items, display_to_binding, selected) =
            build_keys_column(st, theme, slice, app.retro_mode(), label_width, col.width);
        let len = items.len();
        let list = List::new(items)
            .style(theme.style(R::TextPrimary))
            .highlight_style(
                theme
                    .style(R::SettingsValueFocused)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("▶ ")
            .highlight_spacing(HighlightSpacing::Always);
        let offset = match selected {
            Some(selected) => {
                app.bridges.settings_keys_scroll[column_index].resolve(selected, col.height, len, 0)
            }
            None => app.bridges.settings_keys_scroll[column_index].view(col.height, len),
        };
        let mut state = ListState::default().with_offset(offset);
        if let Some(selected) = selected {
            state.select(Some(selected));
        }
        frame.render_stateful_widget(list, col, &mut state);
        buttons::register_list_rows(app, col, state.offset(), display_to_binding.len(), |row| {
            display_to_binding.get(row).copied().flatten()
        });
    }
}

/// Build one Keys-tab column: rendered rows, display-row to logical-binding map, and the
/// highlighted display row when this column owns the cursor.
fn build_keys_column(
    st: &SettingsState,
    theme: &ThemeConfig,
    groups: &[BindingGroup],
    retro: bool,
    label_width: usize,
    width: u16,
) -> (Vec<ListItem<'static>>, Vec<Option<usize>>, Option<usize>) {
    let mut items: Vec<ListItem> = Vec::new();
    let mut display_to_binding: Vec<Option<usize>> = Vec::new();
    let mut selected = None;
    for (group_index, group) in groups.iter().enumerate() {
        if group_index > 0 {
            items.push(ListItem::new(Line::from("")));
            display_to_binding.push(None);
        }
        let title = group.title();
        // Same ruled header as the field tabs; the list reserves the marker gutter here too.
        let rule_width = (width as usize)
            .saturating_sub(UnicodeWidthStr::width(HL_SYMBOL))
            .saturating_sub(UnicodeWidthStr::width(title.as_str()) + 1);
        items.push(ListItem::new(Line::from(vec![
            Span::styled(
                title,
                theme.style(R::SettingsGroup).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" {}", "─".repeat(rule_width)),
                theme.style(R::BorderMuted),
            ),
        ])));
        display_to_binding.push(None);
        for &binding in &group.rows {
            let logical = match binding {
                EditableBinding::Key { logical, .. } | EditableBinding::Mouse { logical, .. } => {
                    logical
                }
            };
            let focused = logical == st.row;
            if focused {
                selected = Some(items.len());
            }
            let (label, value) = match binding {
                EditableBinding::Key {
                    context, action, ..
                } => {
                    let value = if st.capturing == Some((context, action)) {
                        t!("<press a key…>", "<키 입력 대기…>", "<キー入力待ち…>").to_owned()
                    } else {
                        st.keymap.chord(context, action).map_or_else(
                            || "—".to_owned(),
                            |chord| keymap::format_chord_for_display(chord, retro),
                        )
                    };
                    (action.human_label_for(context), value)
                }
                EditableBinding::Mouse {
                    context, gesture, ..
                } => (
                    gesture.human_label(),
                    st.mousemap
                        .action(context, gesture)
                        .human_label()
                        .to_owned(),
                ),
            };
            let value_role = if focused {
                R::SettingsValueFocused
            } else {
                R::SettingsValue
            };
            items.push(ListItem::new(Line::from(vec![
                Span::styled(
                    format!("  {}", pad_to_width(label, label_width)),
                    theme.style(R::SettingsLabel),
                ),
                Span::styled(value, theme.style(value_role)),
            ])));
            display_to_binding.push(Some(logical));
        }
    }
    (items, display_to_binding, selected)
}
