//! The settings view: a tab bar over a list of editable
//! field rows. State lives in `App.settings`; like the library view, the `ListState`
//! that highlights the focused row is rebuilt fresh each frame from a `usize` index.

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
use crate::keymap::{self, Action, Conflict, KeyContext};
use crate::settings::{BAND_GAIN_MAX, BAND_GAIN_MIN};
use crate::settings::{Field, FieldKind, SettingsConfirm, SettingsState, SettingsTab};
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
    let k = |a| {
        app.keymap
            .label_for_display(KeyContext::Settings, a, app.retro_mode())
    };
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
    // The footer row doubles as the status/toast surface. An active status message (Spotify
    // connect/import feedback, errors, the browser/clipboard-fallback hint) takes the row so it
    // is visible without leaving Settings; otherwise the keybinding hint shows. Every other view
    // renders `app.status` — Settings must too, or account actions look like silent no-ops.
    if !app.status.text.is_empty() {
        if let Some(line) = crate::ui::anim::status_toast_line(app, rows[3].width) {
            frame.render_widget(Paragraph::new(line), rows[3]);
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
                rows[3],
            );
        }
    } else {
        frame.render_widget(
            Paragraph::new(Line::from(help_text).style(theme.style(R::TextMuted))),
            rows[3],
        );
    }
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
        let (items, display_to_binding, selected) =
            build_keys_column(st, theme, slice, &entries, app.retro_mode());
        // A 2-cell gutter between the columns keeps the left labels off the right block.
        let col = columns[ci];
        let col = if ci == 0 {
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
            // Reserve the marker gutter in both columns (even the one with no selection) so
            // their rows line up — the focused column would otherwise shift 2 cells left.
            .highlight_spacing(HighlightSpacing::Always);
        // Persist each column's offset (like the field list) so clicking a visible binding
        // focuses it in place rather than snapping the column. Only the focused column
        // re-anchors on its cursor (scrolloff=0 → no move for an already-visible row); the
        // unfocused column keeps whatever position it had.
        let offset = match selected {
            Some(sel) => app.bridges.settings_keys_scroll[ci].resolve(sel, col.height, len, 0),
            None => app.bridges.settings_keys_scroll[ci].view(col.height, len),
        };
        let mut state = ListState::default().with_offset(offset);
        // Only select when this column is focused: ratatui's `ListState::select(None)` resets the
        // offset to 0, which would throw away the `with_offset` we just pre-seeded for the
        // unfocused column. `ListState::default()` already carries `selected = None`.
        if let Some(sel) = selected {
            state.select(Some(sel));
        }
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
    retro: bool,
) -> (Vec<ListItem<'static>>, Vec<Option<usize>>, Option<usize>) {
    let mut items: Vec<ListItem> = Vec::new();
    let mut display_to_binding: Vec<Option<usize>> = Vec::new();
    let mut selected = None;
    // Pad every action label to the widest one (+2-cell gutter) by terminal *display* width, so
    // the key column lines up flush across both columns and in any language — long Korean labels
    // like "자동재생 켜기 / 끄기" (~18 cells) would overflow a fixed width and shove their key
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
                st.keymap.chord(c, action).map_or_else(
                    || "—".to_owned(),
                    |chord| keymap::format_chord_for_display(chord, retro),
                )
            };
            let key_role = if focused {
                R::SettingsValueFocused
            } else {
                R::SettingsValue
            };
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

/// The value text drawn to the right of a (non-color) field's label, exactly as [`field_row`]
/// renders it. Shared so the click-target math never drifts from the on-screen glyphs (the
/// blinking edit caret swaps `▏`↔space — both one cell, so the math holds mid-blink too).
fn field_value_text(app: &App, st: &SettingsState, field: Field, focused: bool) -> String {
    match (field, field.kind()) {
        // Secret fields (API key) are never shown in clear text; while editing, render a
        // masked buffer of *that field's* typed length so keystrokes still register visibly.
        (f, _) if f.is_secret() && focused && st.editing_text => {
            let len = st.draft.text_value(field).map_or(0, |s| s.chars().count());
            format!("{}{}", "•".repeat(len), crate::ui::anim::caret_char(app))
        }
        // Show the live edit buffer with a caret for the focused path field.
        (
            Field::CookiesFile
            | Field::DownloadDir
            | Field::LocalMusicRoot
            | Field::AudiusAppName
            | Field::JamendoClientId,
            _,
        ) if focused && st.editing_text => {
            format!(
                "{}{}",
                st.draft.text_value(field).unwrap_or_default(),
                crate::ui::anim::caret_char(app)
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
            let row = items.len();
            items.push(field_row(app, st, field, i == focused_field, row));
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
                items.push(field_row(app, st, fields[i], i == focused_field, row));
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
            // The swatch + hex value opens the inline hex editor.
            let vx = area.x + gutter + color_lw + 1; // gutter + label + leading space
            put(vx, 2 + 2 + 9, y, MouseTarget::SettingsActivate(i)); // swatch + gap + hex
            continue;
        }

        let vx = area.x + gutter + other_lw;
        // Slider/Select widths don't depend on `focused` (no edit mode), so the ‹ › / < >
        // arrow positions below are stable. Only Text/Button rows vary with `editing_text`, and
        // those register a single whole-value activate rect where exact width is non-critical.
        let value = field_value_text(app, st, field, focused);
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
/// `display_row` is the row's position in the rendered list, used by the cascade reveal —
/// every span's style passes through `stagger_style`, an identity outside its window.
fn field_row<'a>(
    app: &App,
    st: &SettingsState,
    field: Field,
    focused: bool,
    display_row: usize,
) -> ListItem<'a> {
    let theme = &st.draft.theme;
    let fade =
        |s: Style| crate::ui::anim::stagger_style(app, crate::app::Mode::Settings, display_row, s);
    if let Field::ThemeColor(role) = field {
        let label = pad_to_width(role.label(), color_label_width());
        let value = if focused && st.editing_text {
            st.draft.text_value(field).unwrap_or_default().to_owned()
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
            Span::styled(format!("{:<9}", value), fade(theme.style(value_role))),
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
    let value_role = if focused {
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
        // The cell where the 30-fps thumb sits; every cell past it is the red zone.
        let width = track.chars().count().max(1);
        let mark = ((f64::from(FPS_DEFAULT - FPS_MIN) / f64::from(FPS_MAX - FPS_MIN))
            * (width - 1) as f64)
            .round() as usize;
        let normal: String = track.chars().take(mark + 1).collect();
        let hot: String = track.chars().skip(mark + 1).collect();
        let val_style = fade(theme.style(value_role));
        // A literal red — the request is specifically "red", and theme roles like Warning are a
        // muted gray in the monochrome presets. Keeps the value role's background so the red cells
        // don't seam against the rest of the track. (On the *focused* row the List highlight
        // repaints the whole row uniformly, flattening this like every other per-span colour;
        // the zone reads red on the row at rest, which is the usual viewing state.)
        let hot_style = fade(theme.style(value_role).fg(Color::Red));
        let num = format!("{fps} fps");
        let num_style = if fps > FPS_DEFAULT {
            hot_style
        } else {
            val_style
        };
        // Reassembles `slider_str(track, num)` = "‹ {track}  {num} ›" span-by-span.
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
    let value = field_value_text(app, st, field, focused);
    ListItem::new(Line::from(vec![
        Span::styled(label, fade(theme.style(R::SettingsLabel))),
        Span::styled(value, fade(theme.style(value_role))),
    ]))
}

/// A modal warning shown when a rebind on the Keys tab collides with an existing binding.
/// Names the offending chord, the action already holding it, and where it lives, so the
/// failure is loud and actionable rather than a silently-dropped remap.
pub fn render_conflict(frame: &mut Frame, app: &App, area: Rect, conflict: &Conflict) {
    let popup = centered_fixed(area, 54, 9);
    crate::ui::render_popup_background(frame, app, popup);

    let block = Block::default()
        .title(t!(" ⚠ Keybinding conflict ", " ⚠ 단축키 충돌 "))
        .borders(Borders::ALL)
        .border_style(crate::ui::popup_style(app, R::Warning).add_modifier(Modifier::BOLD))
        .style(crate::ui::popup_style(app, R::TextPrimary));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let chord = keymap::format_chord_for_display(conflict.chord, app.retro_mode());
    let where_line = if crate::i18n::is_korean() {
        format!("{} 화면", conflict.ctx.title())
    } else {
        format!("in {}", conflict.ctx.title())
    };
    let lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled(
                chord,
                crate::ui::popup_style(app, R::Warning).add_modifier(Modifier::BOLD),
            ),
            Span::raw(t!(" is already bound to", " 은(는) 이미 사용 중")),
        ]),
        Line::from(Span::styled(
            format!(
                "\u{201c}{}\u{201d}",
                conflict.existing.human_label_for(conflict.ctx)
            ),
            crate::ui::popup_style(app, R::Accent).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            where_line,
            crate::ui::popup_style(app, R::TextMuted),
        )),
        Line::from(""),
        Line::from(Span::styled(
            t!(
                "Binding unchanged · press any key",
                "단축키 변경 안 됨 · 아무 키나 누르세요"
            ),
            crate::ui::popup_style(app, R::TextMuted),
        )),
    ];
    frame.render_widget(
        Paragraph::new(lines)
            .alignment(Alignment::Center)
            .style(crate::ui::popup_style(app, R::TextPrimary)),
        inner,
    );
    crate::ui::seal_popup_background(frame, app, popup);
    crate::ui::mark_art_rows_for_popup(frame, app, popup);
}

/// A modal confirmation for settings actions that have broad side effects.
pub fn render_confirm(frame: &mut Frame, app: &App, area: Rect, confirm: SettingsConfirm) {
    let popup = centered_fixed(area, 56, 9);
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
    let enabling_retro = app.settings.as_ref().is_some_and(|s| !s.draft.retro_mode);
    frame.render_widget(
        Paragraph::new(confirm.prompt(enabling_retro))
            .alignment(Alignment::Center)
            .style(crate::ui::popup_style(app, R::TextPrimary)),
        rows[1],
    );
    frame.render_widget(
        Paragraph::new(confirm.detail(enabling_retro))
            .alignment(Alignment::Center)
            .style(crate::ui::popup_style(app, R::TextMuted)),
        rows[2],
    );

    let segs = [
        buttons::Seg::button(
            MouseTarget::ConfirmSettings,
            t!(" Confirm (Enter) ", " 확인 (Enter) "),
        ),
        buttons::Seg::label("    "),
        buttons::Seg::button(
            MouseTarget::CancelSettings,
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

/// The Spotify playlist picker overlay (Import from Spotify…): ↑/↓ select, Enter
/// imports into a local Library playlist, Esc closes.
pub fn render_spotify_picker(frame: &mut Frame, app: &App, area: Rect) {
    let Some(picker) = app.overlays.spotify_picker.as_ref() else {
        return;
    };
    let h = (picker.items.len() as u16 + 4).clamp(7, 18);
    let popup = centered_fixed(area, 62, h);
    crate::ui::render_popup_background(frame, app, popup);

    let block = Block::default()
        .title(t!(" Import from Spotify ", " Spotify에서 가져오기 "))
        .borders(Borders::ALL)
        .border_style(crate::ui::confirm_border_style(app))
        .style(crate::ui::popup_style(app, R::TextPrimary));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let rows = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);
    let visible = rows[0].height as usize;
    // Keep the selection in view for long playlist lists.
    let first = picker
        .selected
        .saturating_sub(visible.saturating_sub(1))
        .min(picker.items.len().saturating_sub(visible.max(1)));
    let lines: Vec<ratatui::text::Line> = picker
        .items
        .iter()
        .enumerate()
        .skip(first)
        .take(visible)
        .map(|(i, item)| {
            let marker = if i == picker.selected { "▸ " } else { "  " };
            let count = if item.total > 0 {
                format!("  ({})", item.total)
            } else {
                String::new()
            };
            let style = if i == picker.selected {
                crate::ui::popup_style(app, R::TextPrimary)
                    .add_modifier(ratatui::style::Modifier::BOLD)
            } else {
                crate::ui::popup_style(app, R::TextMuted)
            };
            ratatui::text::Line::styled(format!("{marker}{}{count}", item.label), style)
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), rows[0]);
    // One click target per visible row: click selects, clicking the selected row (or a
    // double-click) imports. Indices match the `.skip(first).take(visible)` render above.
    for (vis, i) in (first..picker.items.len()).take(visible).enumerate() {
        app.register_mouse_button(
            Rect {
                x: rows[0].x,
                y: rows[0].y + vis as u16,
                width: rows[0].width,
                height: 1,
            },
            MouseTarget::SpotifyPickRow(i),
        );
    }
    frame.render_widget(
        Paragraph::new(t!(
            "↑/↓ select · Enter import · Esc close · or click",
            "↑/↓ 선택 · Enter 가져오기 · Esc 닫기 · 클릭 가능"
        ))
        .alignment(Alignment::Center)
        .style(crate::ui::popup_style(app, R::TextMuted)),
        rows[1],
    );
    crate::ui::seal_popup_background(frame, app, popup);
    crate::ui::mark_art_rows_for_popup(frame, app, popup);
}

/// The radio-recording settings popup (opened from the Playback tab's one radio item). Rows:
/// mode · min · max · output folder · keep-recent · notifications · browse.
pub fn render_recording_settings(frame: &mut Frame, app: &App, area: Rect) {
    use crate::config::{
        RECORDING_MAX_SECONDS_MAX, RECORDING_MAX_SECONDS_MIN, RECORDING_MIN_SECONDS_MAX,
        RECORDING_MIN_SECONDS_MIN, RECORDING_PAST_TRACKS_MAX, RECORDING_PAST_TRACKS_MIN,
    };
    let Some(popup) = app.overlays.recording_settings.as_ref() else {
        return;
    };
    let Some(st) = app.settings.as_deref() else {
        return;
    };
    let d = &st.draft;
    let popup_rect = centered_fixed(area, 60, 13);
    crate::ui::render_popup_background(frame, app, popup_rect);

    let block = Block::default()
        .title(t!(" Radio recording ", " 라디오 녹음 "))
        .borders(Borders::ALL)
        .border_style(crate::ui::confirm_border_style(app))
        .style(crate::ui::popup_style(app, R::TextPrimary));
    let inner = block.inner(popup_rect);
    frame.render_widget(block, popup_rect);
    let rows = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);

    let dir_display = if popup.editing_dir {
        format!("{}\u{258f}", d.recording_dir) // trailing caret
    } else if d.recording_dir.trim().is_empty() {
        t!("(default)", "(기본값)").to_owned()
    } else {
        d.recording_dir.clone()
    };
    let entries: Vec<(String, String)> = vec![
        (
            t!("Mode", "모드").to_owned(),
            format!("< {} >", d.recording_mode.label()),
        ),
        (
            t!("Min duration", "최소 길이").to_owned(),
            slider_str(
                &bar(
                    d.recording_min_seconds as f64,
                    RECORDING_MIN_SECONDS_MIN as f64,
                    RECORDING_MIN_SECONDS_MAX as f64,
                ),
                &format!("{}s", d.recording_min_seconds),
            ),
        ),
        (
            t!("Max duration", "최대 길이").to_owned(),
            slider_str(
                &bar(
                    d.recording_max_seconds as f64,
                    RECORDING_MAX_SECONDS_MIN as f64,
                    RECORDING_MAX_SECONDS_MAX as f64,
                ),
                &format!("{} min", d.recording_max_seconds / 60),
            ),
        ),
        (t!("Output folder", "저장 폴더").to_owned(), dir_display),
        (
            t!("Keep recent", "최근 보관").to_owned(),
            slider_str(
                &bar(
                    d.recording_past_tracks as f64,
                    RECORDING_PAST_TRACKS_MIN as f64,
                    RECORDING_PAST_TRACKS_MAX as f64,
                ),
                &format!("{}", d.recording_past_tracks),
            ),
        ),
        (
            t!("Notifications", "알림").to_owned(),
            if d.recording_notify {
                "[x]".to_owned()
            } else {
                "[ ]".to_owned()
            },
        ),
        (
            t!("Browse recordings…", "녹음 목록 보기…").to_owned(),
            "\u{21b5}".to_owned(),
        ),
    ];

    let label_w = 16usize;
    let lines: Vec<Line> = entries
        .iter()
        .enumerate()
        .map(|(i, (label, value))| {
            let marker = if i == popup.row { "\u{25b8} " } else { "  " };
            let style = if i == popup.row {
                crate::ui::popup_style(app, R::TextPrimary).add_modifier(Modifier::BOLD)
            } else {
                crate::ui::popup_style(app, R::TextMuted)
            };
            Line::styled(format!("{marker}{label:<label_w$}{value}"), style)
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), rows[0]);
    // Publish the popup rect (a click outside closes it) and, per row, a whole-row focus/
    // activate rect plus finer control rects layered on top: the `‹`/`›` (or mode `< >`) arrows
    // nudge the value both ways, and the 11-cell bar of a numeric row is a drag track. Because
    // `mouse_region_at` scans in reverse, the later, finer rects win over the whole-row rect —
    // so a bare row click only focuses (or activates folder/notify/browse), never fires a slider.
    popup.rect.set(Some(popup_rect));
    let right = rows[0].right();
    for (i, (label, value)) in entries.iter().enumerate() {
        let y = rows[0].y + i as u16;
        if y >= rows[0].bottom() {
            break;
        }
        app.register_mouse_button(
            Rect {
                x: rows[0].x,
                y,
                width: rows[0].width,
                height: 1,
            },
            MouseTarget::RecordingRow(i),
        );
        // Value column origin = display width of the rendered `"{marker}{label:<16}"` prefix, so
        // the arrow/track rects line up with the glyphs even when a CJK label is wider than its
        // char count.
        let marker = if i == popup.row { "\u{25b8} " } else { "  " };
        let vx = rows[0].x
            + buttons::text_width(marker)
            + buttons::text_width(&format!("{label:<label_w$}"));
        let vw = buttons::text_width(value);
        let put = |x: u16, w: u16, target: MouseTarget| {
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
        // Rows 0 (mode) / 1 (min) / 2 (max) / 4 (keep) carry `‹ ›` arrows; the three numeric
        // ones also expose their bar as a drag track. Folder / notify / browse activate on the
        // whole-row rect above, so they get no finer rects here.
        if matches!(i, 0 | 1 | 2 | 4) {
            put(vx, 1, MouseTarget::RecordingChange { row: i, delta: -1 });
            let last = vx.saturating_add(vw.saturating_sub(1));
            put(last, 1, MouseTarget::RecordingChange { row: i, delta: 1 });
            if i != 0 {
                // The 11-cell bar sits just after the leading `‹ ` (2 cells).
                put(vx + 2, 11, MouseTarget::RecordingSlider(i));
            }
        }
    }

    let help = if popup.editing_dir {
        t!(
            "Type a folder \u{b7} Enter done \u{b7} Esc cancel",
            "폴더 입력 \u{b7} Enter 완료 \u{b7} Esc 취소"
        )
    } else {
        t!(
            "\u{2191}/\u{2193} \u{b7} \u{2190}/\u{2192} change \u{b7} Enter \u{b7} Esc close",
            "\u{2191}/\u{2193} \u{b7} \u{2190}/\u{2192} 변경 \u{b7} Enter \u{b7} Esc 닫기"
        )
    };
    frame.render_widget(
        Paragraph::new(help)
            .alignment(Alignment::Center)
            .style(crate::ui::popup_style(app, R::TextMuted)),
        rows[1],
    );
    crate::ui::seal_popup_background(frame, app, popup_rect);
    crate::ui::mark_art_rows_for_popup(frame, app, popup_rect);
}

/// The recordings browser: the in-progress track (if any) plus recent finished tracks, with
/// per-row save/discard/play. Reads `app.recorder`.
pub fn render_recordings_browser(frame: &mut Frame, app: &App, area: Rect) {
    let Some(browser) = app.overlays.recordings_browser.as_ref() else {
        return;
    };
    let mut items: Vec<(String, String)> = Vec::new();
    if let Some(seg) = app.recorder.current.as_ref() {
        items.push((
            format!("\u{25cf} {}", seg_display(seg)),
            t!("Recording\u{2026}", "녹음 중\u{2026}").to_owned(),
        ));
    }
    for t in app.recorder.history.iter() {
        let glyph = if matches!(t.state, crate::recorder::RecordingState::Saved) {
            "\u{25a3}"
        } else if t.state.is_recorded() {
            "\u{2713}"
        } else {
            "\u{b7}"
        };
        items.push((
            format!("{glyph} {}", t.display()),
            format!("{}  {}", fmt_dur(t.duration_secs), t.state.label()),
        ));
    }

    let n = items.len();
    let h = ((n as u16) + 4).clamp(7, 20);
    let popup = centered_fixed(area, 64, h);
    // Publish the browser rect so a click outside closes it (mirrors the queue window).
    browser.rect.set(Some(popup));
    crate::ui::render_popup_background(frame, app, popup);
    let block = Block::default()
        .title(t!(" Radio recordings ", " 라디오 녹음 목록 "))
        .borders(Borders::ALL)
        .border_style(crate::ui::confirm_border_style(app))
        .style(crate::ui::popup_style(app, R::TextPrimary));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let rows = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);
    let visible = rows[0].height as usize;

    if items.is_empty() {
        frame.render_widget(
            Paragraph::new(t!("No recordings yet.", "아직 녹음이 없어요."))
                .alignment(Alignment::Center)
                .style(crate::ui::popup_style(app, R::TextMuted)),
            rows[0],
        );
    } else {
        let sel = browser.selected.min(n.saturating_sub(1));
        let first = sel
            .saturating_sub(visible.saturating_sub(1))
            .min(n.saturating_sub(visible.max(1)));
        let lines: Vec<Line> = items
            .iter()
            .enumerate()
            .skip(first)
            .take(visible)
            .map(|(i, (title, sub))| {
                let marker = if i == sel { "\u{25b8} " } else { "  " };
                let style = if i == sel {
                    crate::ui::popup_style(app, R::TextPrimary).add_modifier(Modifier::BOLD)
                } else {
                    crate::ui::popup_style(app, R::TextMuted)
                };
                Line::styled(format!("{marker}{title}   {sub}"), style)
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), rows[0]);
        // One clickable rect per visible row → single-click selects that item.
        for (vis_idx, i) in (first..n).take(visible).enumerate() {
            app.register_mouse_button(
                Rect {
                    x: rows[0].x,
                    y: rows[0].y + vis_idx as u16,
                    width: rows[0].width,
                    height: 1,
                },
                MouseTarget::RecordingBrowseRow(i),
            );
        }
    }
    frame.render_widget(
        Paragraph::new(t!(
            "\u{2191}/\u{2193} \u{b7} s save \u{b7} d discard \u{b7} Enter play \u{b7} Esc close",
            "\u{2191}/\u{2193} \u{b7} s 저장 \u{b7} d 삭제 \u{b7} Enter 재생 \u{b7} Esc 닫기"
        ))
        .alignment(Alignment::Center)
        .style(crate::ui::popup_style(app, R::TextMuted)),
        rows[1],
    );
    crate::ui::seal_popup_background(frame, app, popup);
    crate::ui::mark_art_rows_for_popup(frame, app, popup);
}

fn seg_display(seg: &crate::recorder::OpenSegment) -> String {
    let base = seg.raw.trim();
    if base.is_empty() {
        seg.title.clone().unwrap_or_else(|| "\u{2014}".to_owned())
    } else {
        base.to_owned()
    }
}

fn fmt_dur(secs: u32) -> String {
    format!("{}:{:02}", secs / 60, secs % 60)
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
