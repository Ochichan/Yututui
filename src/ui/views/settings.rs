//! The settings view: a tab bar over a list of editable
//! field rows. State lives in `App.settings`; like the library view, the `ListState`
//! that highlights the focused row is rebuilt fresh each frame from a `usize` index.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, HighlightSpacing, List, ListItem, ListState, Paragraph};
use unicode_width::UnicodeWidthStr;

use crate::app::{App, MouseTarget};
use crate::keymap::{self, Action, Conflict, KeyContext};
use crate::settings::{Field, FieldKind, SettingsState, SettingsTab};
use crate::settings::{BAND_GAIN_MAX, BAND_GAIN_MIN};
use crate::config::{FPS_MAX, FPS_MIN, SEEK_SECONDS_MAX, SEEK_SECONDS_MIN, SPEED_MAX, SPEED_MIN};
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
        Rect { x: inner.x, y: area.y, width: inner.width, height: 1 },
    );

    let rows = Layout::vertical([
        Constraint::Length(1), // tab bar
        Constraint::Length(1), // spacer
        Constraint::Min(0),    // field list
        Constraint::Length(1), // help
    ])
    .split(inner);

    render_tabs(frame, app, st, rows[0]);
    if st.tab == SettingsTab::Keys {
        render_keys(frame, app, st, rows[2]);
    } else {
        render_fields(frame, app, st, rows[2]);
    }

    // Footer reflects the *committed* keymap, since that's what operates the screen until
    // the edits are saved.
    let k = |a| app.keymap.label(KeyContext::Settings, a);
    let ko = crate::i18n::is_korean();
    let help_text = if st.editing_text && matches!(st.current_field(), Some(Field::ThemeColor(_))) {
        t!(
            "type #RRGGBB or none  ·  Enter save  ·  Backspace delete",
            "#RRGGBB 또는 none 입력  ·  Enter 저장  ·  Backspace 삭제"
        )
        .to_owned()
    } else if st.editing_text {
        // While typing a path/key, Enter or Esc both commit *and* persist it immediately,
        // so the value can't be lost by leaving the screen later.
        t!(
            "type value  ·  Enter or Esc save  ·  Backspace delete",
            "값 입력  ·  Enter 또는 Esc 저장  ·  Backspace 삭제"
        )
        .to_owned()
    } else if st.tab == SettingsTab::Keys {
        if ko {
            format!(
                "{}/{} 선택  ·  {} 재설정  ·  {} 초기화  ·  {} 탭 전환  ·  {} 저장하고 닫기",
                k(Action::MoveUp),
                k(Action::MoveDown),
                k(Action::Confirm),
                k(Action::DeleteChar),
                k(Action::FocusNext),
                k(Action::SettingsCancel),
            )
        } else {
            format!(
                "{}/{} select  ·  {} rebind  ·  {} reset  ·  {} switch tab  ·  {} save + quit",
                k(Action::MoveUp),
                k(Action::MoveDown),
                k(Action::Confirm),
                k(Action::DeleteChar),
                k(Action::FocusNext),
                k(Action::SettingsCancel),
            )
        }
    } else if matches!(st.current_field(), Some(Field::ThemeColor(_))) {
        if ko {
            format!(
                "{}/{} 색상  ·  {} 편집  ·  {} 초기화  ·  {} 탭 전환  ·  {} 저장하고 닫기",
                k(Action::MoveUp),
                k(Action::MoveDown),
                k(Action::Confirm),
                k(Action::DeleteChar),
                k(Action::FocusNext),
                k(Action::SettingsCancel),
            )
        } else {
            format!(
                "{}/{} color  ·  {} edit  ·  {} reset  ·  {} switch tab  ·  {} save + quit",
                k(Action::MoveUp),
                k(Action::MoveDown),
                k(Action::Confirm),
                k(Action::DeleteChar),
                k(Action::FocusNext),
                k(Action::SettingsCancel),
            )
        }
    } else if ko {
        format!(
            "{}/{} 이동  ·  {}/{} 변경  ·  {} 편집/전환  ·  {} 탭 전환  ·  {} 저장하고 닫기",
            k(Action::MoveUp),
            k(Action::MoveDown),
            k(Action::ChangeDecrease),
            k(Action::ChangeIncrease),
            k(Action::Confirm),
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
    frame.render_widget(Paragraph::new(Line::from(help_text).style(theme.style(R::TextMuted))), rows[3]);
}

/// The Keys tab: a scrollable list of every remappable binding, grouped by context. The
/// chord shown is from the *draft* keymap so edits appear immediately; the row being
/// rebound shows a capture prompt.
fn render_keys(frame: &mut Frame, app: &App, st: &SettingsState, area: Rect) {
    let theme = &st.draft.theme;
    let entries = keymap::editable_entries();

    // Group consecutive bindings by context; each becomes a titled block. Whole groups are
    // kept together so a context never straddles the two columns.
    let mut groups: Vec<(KeyContext, Vec<usize>)> = Vec::new();
    for (i, &(ctx, _)) in entries.iter().enumerate() {
        match groups.last_mut() {
            Some((c, v)) if *c == ctx => v.push(i),
            _ => groups.push((ctx, vec![i])),
        }
    }

    // The list used to be one tall column that overflowed; lay it out in two side-by-side
    // columns instead. Break the (whole) groups at the point that best balances the two
    // columns' heights. A blank line separates groups — counted here and drawn the same way,
    // so the two columns' rows stay aligned.
    let height = |g: &(KeyContext, Vec<usize>)| g.1.len() + 2; // header + bindings + gap
    let total: usize = groups.iter().map(height).sum();
    let (mut split, mut acc, mut best) = (groups.len(), 0usize, usize::MAX);
    for (gi, g) in groups.iter().enumerate() {
        acc += height(g);
        let diff = acc.abs_diff(total - acc);
        if diff < best {
            best = diff;
            split = gi + 1;
        }
    }
    let split = split.min(groups.len());

    let columns =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(area);
    for (ci, slice) in [&groups[..split], &groups[split..]].into_iter().enumerate() {
        let (items, display_to_binding, selected) = build_keys_column(st, theme, slice, &entries);
        // A 2-cell gutter between the columns keeps the left labels off the right block.
        let col = columns[ci];
        let col = if ci == 0 {
            Rect { width: col.width.saturating_sub(2), ..col }
        } else {
            col
        };
        let list = List::new(items)
            .style(theme.style(R::TextPrimary))
            .highlight_style(theme.style(R::SettingsValueFocused).add_modifier(Modifier::BOLD))
            .highlight_symbol("▶ ")
            // Reserve the marker gutter in both columns (even the one with no selection) so
            // their rows line up — the focused column would otherwise shift 2 cells left.
            .highlight_spacing(HighlightSpacing::Always);
        let mut state = ListState::default();
        state.select(selected);
        frame.render_stateful_widget(list, col, &mut state);
        // Clicking a binding row selects that binding; header/blank rows aren't targets.
        buttons::register_list_rows(app, col, state.offset(), display_to_binding.len(), |d| {
            display_to_binding.get(d).copied().flatten()
        });
    }
}

/// Build one Keys-tab column from a slice of context groups: the rendered rows, a parallel
/// `display row -> binding index` map (None for header/blank rows), and the display row to
/// highlight (`Some` only when the focused binding `st.row` falls in this column).
fn build_keys_column(
    st: &SettingsState,
    theme: &ThemeConfig,
    groups: &[(KeyContext, Vec<usize>)],
    entries: &[(KeyContext, Action)],
) -> (Vec<ListItem<'static>>, Vec<Option<usize>>, Option<usize>) {
    let mut items: Vec<ListItem> = Vec::new();
    let mut display_to_binding: Vec<Option<usize>> = Vec::new();
    let mut selected = None;
    // Pad every action label to the widest one (+2-cell gutter) by terminal *display* width, so
    // the key column lines up flush across both columns and in any language — long Korean labels
    // like "자동재생 라디오 켜기 / 끄기" (~27 cells) would overflow a fixed width and shove their key
    // out of alignment.
    let label_w = entries
        .iter()
        .map(|(c, a)| UnicodeWidthStr::width(a.human_label_for(*c)))
        .max()
        .unwrap_or(22)
        .max(22)
        + 2;
    for (gi, (ctx, binds)) in groups.iter().enumerate() {
        // A blank line before every group but the first lets the sections breathe.
        if gi > 0 {
            items.push(ListItem::new(Line::from("")));
            display_to_binding.push(None);
        }
        items.push(ListItem::new(Line::from(Span::styled(
            ctx.title().to_owned(),
            theme.style(R::SettingsGroup).add_modifier(Modifier::BOLD),
        ))));
        display_to_binding.push(None);
        for &i in binds {
            let (c, action) = entries[i];
            let focused = i == st.row;
            if focused {
                selected = Some(items.len());
            }
            let key = if st.capturing == Some((c, action)) {
                t!("<press a key…>", "<키 입력 대기…>").to_owned()
            } else {
                st.keymap.chord(c, action).map_or_else(|| "—".to_owned(), keymap::format_chord)
            };
            let key_role = if focused { R::SettingsValueFocused } else { R::SettingsValue };
            // Bindings indent one step in from their group header for a clear hierarchy.
            items.push(ListItem::new(Line::from(vec![
                Span::styled(
                    format!("  {}", pad_to_width(action.human_label_for(c), label_w)),
                    theme.style(R::SettingsLabel),
                ),
                Span::styled(key, theme.style(key_role)),
            ])));
            display_to_binding.push(Some(i));
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
    R::ALL.iter().map(|r| UnicodeWidthStr::width(r.label())).max().unwrap_or(22)
}

/// A slider value as it appears in a row: `‹ {bar}  {num} ›`. The `‹`/`›` are the clickable
/// decrease/increase arrows (their hit-rects are published by [`register_field_controls`]).
fn slider_str(bar: &str, num: &str) -> String {
    format!("‹ {bar}  {num} ›")
}

/// The value text drawn to the right of a (non-color) field's label, exactly as [`field_row`]
/// renders it. Shared so the click-target math never drifts from the on-screen glyphs.
fn field_value_text(st: &SettingsState, field: Field, focused: bool) -> String {
    match (field, field.kind()) {
        // Secret fields (API key) are never shown in clear text; while editing, render a
        // masked buffer of *that field's* typed length so keystrokes still register visibly.
        (f, _) if f.is_secret() && focused && st.editing_text => {
            let len = st.draft.text_value(field).map_or(0, |s| s.chars().count());
            format!("{}▏", "•".repeat(len))
        }
        // Show the live edit buffer with a caret for the focused path field.
        (Field::CookiesFile | Field::DownloadDir, _) if focused && st.editing_text => {
            format!("{}▏", st.draft.text_value(field).unwrap_or_default())
        }
        (Field::Speed, _) => {
            slider_str(&bar(st.draft.speed, SPEED_MIN, SPEED_MAX), &format!("{:.1}x", st.draft.speed))
        }
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
    let fields = st.tab.fields();
    let sections = st.tab.sections();
    let focused_field = st.row.min(fields.len().saturating_sub(1));

    // Build the display list. Sectioned tabs interleave a bold header (with a blank spacer
    // before every header but the first) ahead of each section's fields; flat tabs list the
    // fields directly. `display_to_field` maps each rendered row back to its field index, or
    // `None` for header/blank rows — the same scheme the Keys tab uses, so headers stay
    // unselectable and aren't mistaken for fields by clicks or the cursor.
    let mut items: Vec<ListItem> = Vec::new();
    let mut display_to_field: Vec<Option<usize>> = Vec::new();
    let mut selected = 0usize;

    if sections.is_empty() {
        for (i, &field) in fields.iter().enumerate() {
            if i == focused_field {
                selected = items.len();
            }
            items.push(field_row(st, field, i == focused_field));
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
                theme.style(R::SettingsGroup).add_modifier(Modifier::BOLD),
            ))));
            display_to_field.push(None);
            for _ in 0..*count {
                if i == focused_field {
                    selected = items.len();
                }
                items.push(field_row(st, fields[i], i == focused_field));
                display_to_field.push(Some(i));
                i += 1;
            }
        }
    }

    let list = List::new(items)
        .style(theme.style(R::TextPrimary))
        .highlight_style(theme.style(R::SettingsValueFocused).add_modifier(Modifier::BOLD))
        .highlight_symbol(HL_SYMBOL)
        // Reserve the marker gutter on every row so control hit-rects sit at a fixed x.
        .highlight_spacing(HighlightSpacing::Always);

    let mut state = ListState::default();
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
    let fields = st.tab.fields();
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
        app.register_mouse_button(Rect { x, y, width: w.min(right - x), height: 1 }, target);
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
            // The swatch + hex value opens the inline hex editor.
            let vx = area.x + gutter + color_lw + 1; // gutter + label + leading space
            put(vx, 2 + 2 + 9, y, MouseTarget::SettingsActivate(i)); // swatch + gap + hex
            continue;
        }

        let vx = area.x + gutter + other_lw;
        // Slider/Select widths don't depend on `focused` (no edit mode), so the ‹ › / < >
        // arrow positions below are stable. Only Text/Button rows vary with `editing_text`, and
        // those register a single whole-value activate rect where exact width is non-critical.
        let value = field_value_text(st, field, focused);
        let w = buttons::text_width(&value);
        match field.kind() {
            FieldKind::Toggle => {
                put(vx, 3, y, MouseTarget::SettingsChange { row: i, delta: 1 });
            }
            FieldKind::Select | FieldKind::Slider => {
                put(vx, 1, y, MouseTarget::SettingsChange { row: i, delta: -1 });
                let last = vx.saturating_add(w.saturating_sub(1));
                put(last, 1, y, MouseTarget::SettingsChange { row: i, delta: 1 });
            }
            FieldKind::Button | FieldKind::Text => {
                put(vx, w, y, MouseTarget::SettingsActivate(i));
            }
        }
    }
}

/// One field row: a left-aligned label and its current value (with a slider bar for numeric
/// fields, `< … >` arrows for cycles, and a caret for the text field being edited).
fn field_row<'a>(st: &SettingsState, field: Field, focused: bool) -> ListItem<'a> {
    let theme = &st.draft.theme;
    if let Field::ThemeColor(role) = field {
        let label = pad_to_width(role.label(), color_label_width());
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
    // Pad every label in this tab to the widest one (+ a 2-space gutter, min 20) so the value
    // column lines up regardless of label length. The value text itself is produced by the
    // shared `field_value_text`, so the click-target math stays in lockstep with the glyphs.
    let label = pad_to_width(&field.label(), other_label_width(st.tab));
    let value = field_value_text(st, field, focused);
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
        .title(t!(" ⚠ Keybinding conflict ", " ⚠ 단축키 충돌 "))
        .borders(Borders::ALL)
        .border_style(theme.style(R::Warning).add_modifier(Modifier::BOLD))
        .style(theme.style(R::TextPrimary));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let chord = keymap::format_chord(conflict.chord);
    let where_line = if crate::i18n::is_korean() {
        format!("{} 화면", conflict.ctx.title())
    } else {
        format!("in {}", conflict.ctx.title())
    };
    let lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled(chord, theme.style(R::Warning).add_modifier(Modifier::BOLD)),
            Span::raw(t!(" is already bound to", " 은(는) 이미 사용 중")),
        ]),
        Line::from(Span::styled(
            format!("\u{201c}{}\u{201d}", conflict.existing.human_label_for(conflict.ctx)),
            theme.style(R::Accent).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(where_line, theme.style(R::TextMuted))),
        Line::from(""),
        Line::from(Span::styled(
            t!("Binding unchanged · press any key", "단축키 변경 안 됨 · 아무 키나 누르세요"),
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
        .title(t!(" ⚠ Reset all settings ", " ⚠ 모든 설정 초기화 "))
        .borders(Borders::ALL)
        .border_style(theme.style(R::Error).add_modifier(Modifier::BOLD))
        .style(theme.style(R::TextPrimary));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let lines = vec![
        Line::from(""),
        Line::from(t!("Restore every setting to its default?", "모든 설정을 기본값으로 되돌릴까요?")),
        Line::from(Span::styled(
            t!("Keybindings, theme, and API key included.", "단축키, 테마, API 키 포함."),
            theme.style(R::TextMuted),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("Enter / y", theme.style(R::Error).add_modifier(Modifier::BOLD)),
            Span::raw(t!(" reset    ", " 초기화    ")),
            Span::styled("Esc", theme.style(R::Accent).add_modifier(Modifier::BOLD)),
            Span::raw(t!(" cancel", " 취소")),
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
