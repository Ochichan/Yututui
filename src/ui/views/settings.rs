//! The settings view: a tab bar over a list of editable
//! field rows. State lives in `App.settings`; like the library view, the `ListState`
//! that highlights the focused row is rebuilt fresh each frame from a `usize` index.

mod dialogs;
mod recording;
mod spotify;

pub use dialogs::{render_confirm, render_conflict};
pub use recording::{render_recording_settings, render_recordings_browser};
pub(super) use spotify::render_spotify_import_mode_dropdown_popup;
pub use spotify::render_spotify_picker;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, HighlightSpacing, List, ListItem, ListState, Paragraph};
use unicode_width::UnicodeWidthStr;

use crate::app::{App, MouseTarget, ScrollSurface, StatusKind};
use crate::config::{
    FPS_DEFAULT, FPS_MAX, FPS_MIN, SEEK_SECONDS_MAX, SEEK_SECONDS_MIN, SPEED_MAX, SPEED_MIN,
};
use crate::i18n::Language;
use crate::keymap::{self, Action, KeyContext};
use crate::settings::{BAND_GAIN_MAX, BAND_GAIN_MIN};
use crate::settings::{Field, FieldKind, SettingsState, SettingsTab};
use crate::t;
use crate::theme::ThemeConfig;
use crate::theme::ThemeRole as R;
use crate::ui::buttons;
use crate::ui::text::pad_to_width;

pub fn render(frame: &mut Frame, app: &App, area: Rect) {
    // No screen without state — but render defensively rather than panic.
    let Some(st) = app.settings.as_deref() else {
        return;
    };
    let theme = &st.draft.theme;
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.style(R::BorderPrimary))
        .style(theme.style(R::TextPrimary));
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
        Constraint::Length(1),                                        // tab bar
        Constraint::Length(1),                                        // spacer
        Constraint::Min(0),                                           // field list
        Constraint::Length(crate::ui::control_box::docked_rows(app)), // docked player bar
        Constraint::Length(1),                                        // help
    ])
    .split(inner);

    render_tabs(frame, app, st, rows[0]);
    if st.tab == SettingsTab::Keys {
        render_keys(frame, app, st, rows[2]);
    } else {
        render_fields(frame, app, st, rows[2]);
    }
    crate::ui::control_box::render_docked(frame, app, rows[3]);

    // Footer reflects the *committed* keymap, since that's what operates the screen until
    // the edits are saved.
    let k = |a| {
        app.keymap
            .label_for_display(KeyContext::Settings, a, app.retro_mode())
    };
    let lang = crate::i18n::current();
    let help_text = if st.editing_text && matches!(st.current_field(), Some(Field::ThemeColor(_))) {
        t!(
            "type #RRGGBB or none  ·  Enter save  ·  Backspace delete",
            "#RRGGBB 또는 none 입력  ·  Enter 저장  ·  Backspace 삭제",
            "#RRGGBB または none を入力  ·  Enter 保存  ·  Backspace 削除"
        )
        .to_owned()
    } else if st.editing_text {
        // While typing a path/key, Enter or Esc both commit *and* persist it immediately,
        // so the value can't be lost by leaving the screen later.
        t!(
            "type value  ·  Enter or Esc save  ·  Backspace delete",
            "값 입력  ·  Enter 또는 Esc 저장  ·  Backspace 삭제",
            "値を入力  ·  Enter または Esc 保存  ·  Backspace 削除"
        )
        .to_owned()
    } else if matches!(st.current_field(), Some(Field::ExportPersonalData)) {
        format!(
            "{} {}",
            k(Action::Confirm),
            t!(
                "export · unencrypted JSON · includes private listening history",
                "내보내기 · 암호화되지 않은 JSON · 개인 감상 기록 포함",
                "エクスポート · 暗号化されないJSON · 個人の再生履歴を含む"
            )
        )
    } else if st.tab == SettingsTab::Keys {
        let mouse_row = st.row >= keymap::editable_entries().len();
        match (lang, mouse_row) {
            (Language::Korean, true) => format!(
                "{}/{} 선택  ·  {}/{} 또는 {} 변경  ·  {} 초기화  ·  {} 탭 전환  ·  {} 저장하고 닫기",
                k(Action::MoveUp),
                k(Action::MoveDown),
                k(Action::ChangeDecrease),
                k(Action::ChangeIncrease),
                k(Action::Confirm),
                k(Action::DeleteChar),
                k(Action::FocusNext),
                k(Action::SettingsCancel),
            ),
            (Language::Korean, false) => format!(
                "{}/{} 선택  ·  {} 재설정  ·  {} 초기화  ·  {} 탭 전환  ·  {} 저장하고 닫기",
                k(Action::MoveUp),
                k(Action::MoveDown),
                k(Action::Confirm),
                k(Action::DeleteChar),
                k(Action::FocusNext),
                k(Action::SettingsCancel),
            ),
            (Language::Japanese, true) => format!(
                "{}/{} 選択  ·  {}/{} または {} 変更  ·  {} リセット  ·  {} タブ切替  ·  {} 保存して閉じる",
                k(Action::MoveUp),
                k(Action::MoveDown),
                k(Action::ChangeDecrease),
                k(Action::ChangeIncrease),
                k(Action::Confirm),
                k(Action::DeleteChar),
                k(Action::FocusNext),
                k(Action::SettingsCancel),
            ),
            (Language::Japanese, false) => format!(
                "{}/{} 選択  ·  {} 再割り当て  ·  {} リセット  ·  {} タブ切替  ·  {} 保存して閉じる",
                k(Action::MoveUp),
                k(Action::MoveDown),
                k(Action::Confirm),
                k(Action::DeleteChar),
                k(Action::FocusNext),
                k(Action::SettingsCancel),
            ),
            (_, true) => format!(
                "{}/{} select  ·  {}/{} or {} change  ·  {} reset  ·  {} switch tab  ·  {} save + quit",
                k(Action::MoveUp),
                k(Action::MoveDown),
                k(Action::ChangeDecrease),
                k(Action::ChangeIncrease),
                k(Action::Confirm),
                k(Action::DeleteChar),
                k(Action::FocusNext),
                k(Action::SettingsCancel),
            ),
            (_, false) => format!(
                "{}/{} select  ·  {} rebind  ·  {} reset  ·  {} switch tab  ·  {} save + quit",
                k(Action::MoveUp),
                k(Action::MoveDown),
                k(Action::Confirm),
                k(Action::DeleteChar),
                k(Action::FocusNext),
                k(Action::SettingsCancel),
            ),
        }
    } else if matches!(st.current_field(), Some(Field::ThemeColor(_))) {
        match lang {
            Language::Korean => format!(
                "{}/{} 색상  ·  {} 편집  ·  {} 초기화  ·  {} 탭 전환  ·  {} 저장하고 닫기",
                k(Action::MoveUp),
                k(Action::MoveDown),
                k(Action::Confirm),
                k(Action::DeleteChar),
                k(Action::FocusNext),
                k(Action::SettingsCancel),
            ),
            Language::Japanese => format!(
                "{}/{} カラー  ·  {} 編集  ·  {} リセット  ·  {} タブ切替  ·  {} 保存して閉じる",
                k(Action::MoveUp),
                k(Action::MoveDown),
                k(Action::Confirm),
                k(Action::DeleteChar),
                k(Action::FocusNext),
                k(Action::SettingsCancel),
            ),
            _ => format!(
                "{}/{} color  ·  {} edit  ·  {} reset  ·  {} switch tab  ·  {} save + quit",
                k(Action::MoveUp),
                k(Action::MoveDown),
                k(Action::Confirm),
                k(Action::DeleteChar),
                k(Action::FocusNext),
                k(Action::SettingsCancel),
            ),
        }
    } else {
        match lang {
            Language::Korean => format!(
                "{}/{} 이동  ·  {}/{} 변경  ·  {} 편집/전환  ·  {} 탭 전환  ·  {} 저장하고 닫기",
                k(Action::MoveUp),
                k(Action::MoveDown),
                k(Action::ChangeDecrease),
                k(Action::ChangeIncrease),
                k(Action::Confirm),
                k(Action::FocusNext),
                k(Action::SettingsCancel),
            ),
            Language::Japanese => format!(
                "{}/{} 移動  ·  {}/{} 変更  ·  {} 編集/切替  ·  {} タブ切替  ·  {} 保存して閉じる",
                k(Action::MoveUp),
                k(Action::MoveDown),
                k(Action::ChangeDecrease),
                k(Action::ChangeIncrease),
                k(Action::Confirm),
                k(Action::FocusNext),
                k(Action::SettingsCancel),
            ),
            _ => format!(
                "{}/{} field  ·  {}/{} change  ·  {} edit/toggle  ·  {} switch tab  ·  {} save + quit",
                k(Action::MoveUp),
                k(Action::MoveDown),
                k(Action::ChangeDecrease),
                k(Action::ChangeIncrease),
                k(Action::Confirm),
                k(Action::FocusNext),
                k(Action::SettingsCancel),
            ),
        }
    };
    // The footer row doubles as the status/toast surface. An active status message (Spotify
    // connect/import feedback, errors, the browser/clipboard-fallback hint) takes the row so it
    // is visible without leaving Settings; otherwise the keybinding hint shows. Every other view
    // renders `app.status` — Settings must too, or account actions look like silent no-ops.
    // With the docked control box on screen its title row already shows the same status, so
    // the footer keeps the keybinding hint instead of doubling the message.
    if !app.status.text.is_empty() && !app.control_box_active() {
        if let Some(line) = crate::ui::anim::status_toast_line(app, rows[4].width) {
            frame.render_widget(Paragraph::new(line), rows[4]);
        } else {
            let role = match app.status.kind {
                StatusKind::Error => R::Error,
                StatusKind::Info => R::Success,
            };
            frame.render_widget(
                Paragraph::new(
                    Line::from(app.status.text.as_str())
                        .style(theme.style(role))
                        .alignment(Alignment::Center),
                ),
                rows[4],
            );
        }
    } else {
        frame.render_widget(
            Paragraph::new(Line::from(help_text).style(theme.style(R::TextMuted))),
            rows[4],
        );
    }
    // Settings rolls its own footer (no `render_help_button`), so the docked-bar collapse
    // toggle rides the row's right edge here — same target and glyphs as the shared footer.
    if app.player_bar_position() == crate::config::PlayerBarPosition::Bottom && rows[4].width >= 2 {
        let glyph = match (app.config.control_box_collapsed(), app.retro_mode()) {
            (false, false) => "▼",
            (true, false) => "▲",
            (false, true) => "v",
            (true, true) => "^",
        };
        let rect = Rect {
            x: rows[4].right().saturating_sub(2),
            y: rows[4].y,
            width: 2,
            height: 1,
        };
        frame.render_widget(
            Paragraph::new(Line::from(glyph).style(theme.style(R::TextMuted))),
            rect,
        );
        app.register_mouse_button(rect, MouseTarget::Global(Action::ToggleControlBox));
    }
}

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
fn render_keys(frame: &mut Frame, app: &App, st: &SettingsState, area: Rect) {
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
        let (items, display_to_binding, selected) =
            build_keys_column(st, theme, slice, app.retro_mode(), label_width);
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
) -> (Vec<ListItem<'static>>, Vec<Option<usize>>, Option<usize>) {
    let mut items: Vec<ListItem> = Vec::new();
    let mut display_to_binding: Vec<Option<usize>> = Vec::new();
    let mut selected = None;
    for (group_index, group) in groups.iter().enumerate() {
        if group_index > 0 {
            items.push(ListItem::new(Line::from("")));
            display_to_binding.push(None);
        }
        items.push(ListItem::new(Line::from(Span::styled(
            group.title(),
            theme.style(R::SettingsGroup).add_modifier(Modifier::BOLD),
        ))));
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
    for (i, t) in crate::settings::SettingsTab::ALL
        .iter()
        .copied()
        .enumerate()
    {
        if i > 0 {
            spans.push(Span::styled(DIVIDER, muted));
            x = x.saturating_add(buttons::text_width(DIVIDER));
        }
        let label = format!(" {} ", t.label());
        let w = buttons::text_width(&label);
        app.register_mouse_button(
            Rect {
                x,
                y: area.y,
                width: w,
                height: 1,
            },
            MouseTarget::SettingsTab(i),
        );
        x = x.saturating_add(w);
        let style = if st.tab == t {
            // A brief accent wash right after a tab switch (identity when off).
            crate::ui::anim::active_tab_style(app, crate::ui::anim::TabPop::Inner, active)
        } else {
            muted
        };
        spans.push(Span::styled(label, style));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// The list's selection marker. `HighlightSpacing::Always` reserves its width on *every* row
/// (selected or not), so control hit-rects can assume a fixed content x-offset.
const HL_SYMBOL: &str = "▶ ";

/// Width of the label column for ordinary (non-color) rows: the widest label in the tab,
/// floored at 20, plus a 2-cell gutter. Defined once so `field_row` and the click-target math
/// agree on where each value column begins.
fn other_label_width(tab: SettingsTab) -> usize {
    tab.fields()
        .iter()
        .filter(|f| !matches!(f, Field::ThemeColor(_)))
        .map(|f| UnicodeWidthStr::width(f.label().as_str()))
        .max()
        .unwrap_or(20)
        .max(20)
        + 2
}

/// Width of the label column for color-role rows (the widest role label by display width, so
/// two-cell Korean labels still line the swatch/hex/description columns up).
fn color_label_width() -> usize {
    R::ALL
        .iter()
        .map(|r| UnicodeWidthStr::width(r.label()))
        .max()
        .unwrap_or(22)
}

/// A slider value as it appears in a row: `‹ {bar}  {num} ›`. The `‹`/`›` are the clickable
/// decrease/increase arrows (their hit-rects are published by [`register_field_controls`]).
fn slider_str(bar: &str, num: &str) -> String {
    format!("‹ {bar}  {num} ›")
}

/// Value text shared by rendering and click-target measurement.
fn field_value_text(
    app: &App,
    st: &SettingsState,
    field: Field,
    focused: bool,
    width: usize,
) -> String {
    match (field, field.kind()) {
        (Field::ExportPersonalData, _) => st.personal_data_export.value_display(),
        (Field::AudioOutput, _) => app.audio_output_display_label(&st.draft.audio_mpv_device),
        (f, FieldKind::Text) if focused && st.editing_text => {
            let value = st.draft.text_value(field).unwrap_or_default();
            let cursor = st.text_cursor.byte_index(value);
            crate::ui::text::editable_value(
                value,
                cursor,
                width,
                crate::ui::anim::caret_char(app),
                f.is_secret(),
            )
        }
        (Field::Speed, _) => slider_str(
            &bar(st.draft.speed, SPEED_MIN, SPEED_MAX),
            &format!("{:.1}x", st.draft.speed),
        ),
        (Field::SeekInterval, _) => slider_str(
            &bar(st.draft.seek_seconds, SEEK_SECONDS_MIN, SEEK_SECONDS_MAX),
            &format!("{:.0}s", st.draft.seek_seconds),
        ),
        (Field::Band(i), _) => slider_str(
            &bar(st.draft.eq_bands[i], BAND_GAIN_MIN, BAND_GAIN_MAX),
            &format!("{:+.0} dB", st.draft.eq_bands[i]),
        ),
        (Field::AnimFps, _) => {
            let fps = st.draft.animations.effective_fps();
            slider_str(
                &bar(f64::from(fps), f64::from(FPS_MIN), f64::from(FPS_MAX)),
                &format!("{fps} fps"),
            )
        }
        (_, FieldKind::Select) => format!("< {} >", st.draft.value_display(field)),
        _ => st.draft.value_display(field),
    }
}

fn render_fields(frame: &mut Frame, app: &App, st: &SettingsState, area: Rect) {
    let theme = &st.draft.theme;
    let fields = st.fields();
    // Must use `st.sections()` (not `st.tab.sections()`): it applies the same visibility
    // filter as `st.fields()`, so the per-section counts stay a valid partition and the
    // `fields[i]` walk below never runs past the end.
    let sections = st.sections();
    let focused_field = st.row.min(fields.len().saturating_sub(1));

    // Build fields plus unselectable section headers/spacers and retain their index mapping.
    let mut items: Vec<ListItem> = Vec::new();
    let mut display_to_field: Vec<Option<usize>> = Vec::new();
    let mut selected = 0usize;
    let value_width = (area.width as usize)
        .saturating_sub(UnicodeWidthStr::width(HL_SYMBOL) + other_label_width(st.tab));

    if sections.is_empty() {
        for (i, &field) in fields.iter().enumerate() {
            if i == focused_field {
                selected = items.len();
            }
            let row = items.len();
            items.push(field_row(
                app,
                st,
                field,
                i == focused_field,
                row,
                value_width,
            ));
            display_to_field.push(Some(i));
        }
    } else {
        let mut i = 0usize;
        for (si, (title, count)) in sections.iter().enumerate() {
            if si > 0 {
                items.push(ListItem::new(Line::from("")));
                display_to_field.push(None);
            }
            items.push(ListItem::new(Line::from(Span::styled(
                (*title).to_owned(),
                crate::ui::anim::stagger_style(
                    app,
                    crate::app::Mode::Settings,
                    items.len(),
                    theme.style(R::SettingsGroup).add_modifier(Modifier::BOLD),
                ),
            ))));
            display_to_field.push(None);
            for _ in 0..*count {
                if i == focused_field {
                    selected = items.len();
                }
                let row = items.len();
                items.push(field_row(
                    app,
                    st,
                    fields[i],
                    i == focused_field,
                    row,
                    value_width,
                ));
                display_to_field.push(Some(i));
                i += 1;
            }
        }
    }

    let len = items.len();
    let list = List::new(items)
        .style(theme.style(R::TextPrimary))
        .highlight_style(
            theme
                .style(R::SettingsValueFocused)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol(HL_SYMBOL)
        // Reserve the marker gutter on every row so control hit-rects sit at a fixed x.
        .highlight_spacing(HighlightSpacing::Always);

    // Keep the scroll offset across frames so a click on a visible row focuses it *in place*.
    // (A fresh `ListState::default()` let ratatui re-derive the offset from 0 every frame and
    // pin the selection to an edge, so clicking the top/bottom row snapped the whole viewport.)
    // `scrolloff = 0` means an already-visible cursor never moves the offset; keyboard nav past
    // an edge still scrolls by one. The resolved offset always keeps `selected` on-screen, so
    // the highlight is set unconditionally.
    let offset = app
        .bridges
        .settings_scroll
        .resolve(selected, area.height, len, 0);
    let mut state = ListState::default().with_offset(offset);
    state.select(Some(selected));
    frame.render_stateful_widget(list, area, &mut state);

    let offset = state.offset();
    // Whole-row clicks select the field; header/blank rows aren't targets.
    buttons::register_list_rows(app, area, offset, display_to_field.len(), |d| {
        display_to_field.get(d).copied().flatten()
    });
    // Per-control click targets (checkbox / arrows / button / text) on top of the row rects, so
    // they win where they overlap (`mouse_target_at` takes the last-registered match).
    register_field_controls(app, st, area, offset, &display_to_field);
    spotify::render_spotify_import_mode_dropdown(frame, app, st, area, offset, &display_to_field);
    // Right-border scrollbar tracking the viewport; a no-op when every row fits.
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
        area.height as usize,
    );
}

/// Publish a hit rect for each visible field row's interactive control, layered over the row's
/// select rect. Toggles get a rect over the `[x]` checkbox (→ flip); Select/Slider get rects
/// over their `<`/`‹` and `>`/`›` arrows (→ −1 / +1); Buttons and text fields get a rect over
/// their value (→ activate, i.e. press / enter edit mode).
fn register_field_controls(
    app: &App,
    st: &SettingsState,
    area: Rect,
    offset: usize,
    display_to_field: &[Option<usize>],
) {
    let fields = st.fields();
    let focused_field = st.row.min(fields.len().saturating_sub(1));
    let gutter = buttons::text_width(HL_SYMBOL);
    let other_lw = other_label_width(st.tab) as u16;
    let color_lw = color_label_width() as u16;
    let right = area.right();

    // Register a clamped 1-row rect, skipping anything that starts past the visible width.
    let put = |x: u16, w: u16, y: u16, target: MouseTarget| {
        if w == 0 || x >= right {
            return;
        }
        app.register_mouse_button(
            Rect {
                x,
                y,
                width: w.min(right - x),
                height: 1,
            },
            target,
        );
    };

    for vis in 0..area.height {
        let display = offset + vis as usize;
        if display >= display_to_field.len() {
            break;
        }
        let Some(i) = display_to_field[display] else {
            continue;
        };
        let field = fields[i];
        let y = area.y + vis;
        let focused = i == focused_field;

        if let Field::ThemeColor(_) = field {
            // The swatch opens the palette; the hex value retains the inline editor.
            let vx = area.x + gutter + color_lw + 1; // gutter + label + leading space
            put(vx, 2, y, MouseTarget::SettingsColorSwatch(i));
            put(vx + 4, 9, y, MouseTarget::SettingsActivate(i));
            continue;
        }

        let vx = area.x + gutter + other_lw;
        // Text/Button rows use one whole-value target; slider arrow positions remain stable.
        let value = field_value_text(app, st, field, focused, right.saturating_sub(vx) as usize);
        let w = buttons::text_width(&value);
        match field.kind() {
            FieldKind::Toggle => {
                put(vx, 3, y, MouseTarget::SettingsChange { row: i, delta: 1 });
            }
            FieldKind::Select | FieldKind::Slider => {
                if field == Field::SpotifyImportMode {
                    put(vx, w, y, MouseTarget::SettingsSpotifyImportModeMenu);
                }
                put(vx, 1, y, MouseTarget::SettingsChange { row: i, delta: -1 });
                let last = vx.saturating_add(w.saturating_sub(1));
                put(last, 1, y, MouseTarget::SettingsChange { row: i, delta: 1 });
            }
            FieldKind::Button
                if field == Field::ExportPersonalData && st.personal_data_export.is_busy() =>
            {
                // Keep the row focusable while work is running, but do not publish an action
                // target that could enqueue a duplicate export from a second click.
            }
            FieldKind::Button | FieldKind::Text => {
                put(vx, w, y, MouseTarget::SettingsActivate(i));
            }
        }
    }
}

/// One field row: a left-aligned label and its current value (with a slider bar for numeric
/// fields, `< … >` arrows for cycles, and a caret for the text field being edited).
/// `display_row` is the row's position in the rendered list, used by the cascade reveal —
/// every span's style passes through `stagger_style`, an identity outside its window.
fn field_row<'a>(
    app: &App,
    st: &SettingsState,
    field: Field,
    focused: bool,
    display_row: usize,
    value_width: usize,
) -> ListItem<'a> {
    let theme = &st.draft.theme;
    let fade =
        |s: Style| crate::ui::anim::stagger_style(app, crate::app::Mode::Settings, display_row, s);
    if let Field::ThemeColor(role) = field {
        let label = pad_to_width(role.label(), color_label_width());
        let value = if focused && st.editing_text {
            let raw = st.draft.text_value(field).unwrap_or_default();
            let cursor = st.text_cursor.byte_index(raw);
            crate::ui::text::editable_value(raw, cursor, 9, crate::ui::anim::caret_char(app), false)
        } else {
            st.draft.value_display(field)
        };
        let value_role = if focused {
            R::SettingsValueFocused
        } else {
            R::SettingsValue
        };
        // Transparent roles have no fill — show a hatched marker so it reads as "terminal
        // background shows through" rather than a missing/black swatch.
        let swatch = if theme.is_role_transparent(role) {
            Span::styled("▒▒", fade(theme.style(R::TextMuted)))
        } else {
            Span::styled("  ", fade(Style::default().bg(theme.color(role))))
        };
        return ListItem::new(Line::from(vec![
            Span::styled(label, fade(theme.style(R::SettingsLabel))),
            Span::raw(" "),
            swatch,
            Span::raw("  "),
            Span::styled(pad_to_width(&value, 9), fade(theme.style(value_role))),
            Span::styled(
                role.description().to_owned(),
                fade(theme.style(R::TextMuted)),
            ),
        ]));
    }
    // Pad every label in this tab to the widest one (+ a 2-space gutter, min 20) so the value
    // column lines up regardless of label length. The value text itself is produced by the
    // shared `field_value_text`, so the click-target math stays in lockstep with the glyphs.
    let label = pad_to_width(&field.label(), other_label_width(st.tab));
    let value_role = if field == Field::ExportPersonalData && st.personal_data_export.is_busy() {
        R::TextMuted
    } else if focused {
        R::SettingsValueFocused
    } else {
        R::SettingsValue
    };
    // The frame-rate slider is the one row whose value isn't a single flat span: the track cells
    // above the 30-fps mark are always red (a "this is heavy" danger zone) and the number reddens
    // once fps > 30. The glyphs are byte-identical to `field_value_text`'s `AnimFps` arm, so the
    // arrow hit-rects from `register_field_controls` stay in lockstep.
    if field == Field::AnimFps {
        let fps = st.draft.animations.effective_fps();
        let track = bar(f64::from(fps), f64::from(FPS_MIN), f64::from(FPS_MAX));
        // Every track cell past the 30-fps thumb is the red zone.
        let width = track.chars().count().max(1);
        let mark = ((f64::from(FPS_DEFAULT - FPS_MIN) / f64::from(FPS_MAX - FPS_MIN))
            * (width - 1) as f64)
            .round() as usize;
        let normal: String = track.chars().take(mark + 1).collect();
        let hot: String = track.chars().skip(mark + 1).collect();
        let val_style = fade(theme.style(value_role));
        // Keep a literal red foreground while preserving the value role's background.
        let hot_style = fade(theme.style(value_role).fg(Color::Red));
        let num = format!("{fps} fps");
        let num_style = if fps > FPS_DEFAULT {
            hot_style
        } else {
            val_style
        };
        return ListItem::new(Line::from(vec![
            Span::styled(label, fade(theme.style(R::SettingsLabel))),
            Span::styled("\u{2039} ".to_owned(), val_style),
            Span::styled(normal, val_style),
            Span::styled(hot, hot_style),
            Span::styled("  ".to_owned(), val_style),
            Span::styled(num, num_style),
            Span::styled(" \u{203a}".to_owned(), val_style),
        ]));
    }
    let value = field_value_text(app, st, field, focused, value_width);
    ListItem::new(Line::from(vec![
        Span::styled(label, fade(theme.style(R::SettingsLabel))),
        Span::styled(value, fade(theme.style(value_role))),
    ]))
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
    let frac = if max > min {
        ((value - min) / (max - min)).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let pos = (frac * (WIDTH - 1) as f64).round() as usize;
    let mut s = String::with_capacity(WIDTH);
    for i in 0..WIDTH {
        s.push(if i == pos { '●' } else { '─' });
    }
    s
}
