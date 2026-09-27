//! The settings view: a tab bar over a list of editable
//! field rows. State lives in `App.settings`; like the library view, the `ListState`
//! that highlights the focused row is rebuilt fresh each frame from a `usize` index.

mod dialogs;
mod music_server;
mod recording;
mod spotify;
mod sync;
mod sync_wizard;
mod tabs;

pub use dialogs::{render_confirm, render_conflict};
pub(crate) use music_server::render_music_server_wizard;
pub use recording::{render_recording_settings, render_recordings_browser};
pub(super) use spotify::render_spotify_import_mode_dropdown_popup;
pub use spotify::render_spotify_picker;
pub(crate) use sync::render_sync;
pub(crate) use sync_wizard::render_sync_wizard;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, HighlightSpacing, List, ListItem, ListState, Paragraph};
use unicode_width::UnicodeWidthStr;

use crate::app::{App, MouseTarget, ScrollSurface};
use crate::config::{
    FPS_DEFAULT, FPS_MAX, FPS_MIN, SEEK_SECONDS_MAX, SEEK_SECONDS_MIN, SPEED_MAX, SPEED_MIN,
};
use crate::keymap::{self, Action, KeyContext};
use crate::settings::{BAND_GAIN_MAX, BAND_GAIN_MIN};
use crate::settings::{Field, FieldKind, SettingsState, SettingsTab};
use crate::t;
use crate::theme::ThemeConfig;
use crate::theme::ThemeRole as R;
use crate::ui::buttons;
use crate::ui::text::pad_to_width;

/// One footer hint. Essential hints (how to open or edit the focused item, switch area or tab,
/// and leave) are never dropped. On a terminal too narrow for all of them the plain arrow hints
/// go first, then the secondary ones (page scroll, ←/→ change); the `?` key list names them all.
struct Hint {
    text: String,
    priority: Priority,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Priority {
    Optional,
    Secondary,
    Essential,
}

fn hint(text: String, essential: bool) -> Hint {
    Hint {
        text,
        priority: if essential {
            Priority::Essential
        } else {
            Priority::Optional
        },
    }
}

/// Footer hints for the Settings screen. Reflects the *committed* keymap, since that's what
/// operates the screen until the edits are saved. `overflowing` adds the page-scroll hint for
/// a list or pane that runs past the screen.
fn footer_hints(app: &App, st: &SettingsState, overflowing: bool) -> Vec<Hint> {
    let k = |a| {
        app.keymap
            .label_for_display(KeyContext::Settings, a, app.retro_mode())
    };
    let save_quit = || {
        hint(
            format!(
                "{} {}",
                k(Action::SettingsCancel),
                t!("save + quit", "저장하고 닫기", "保存して閉じる")
            ),
            true,
        )
    };
    let switch_tab = || {
        hint(
            format!(
                "{} {}",
                k(Action::FocusNext),
                t!("switch tab", "탭 전환", "タブ切替")
            ),
            true,
        )
    };
    let reset = || {
        hint(
            format!(
                "{} {}",
                k(Action::DeleteChar),
                t!("reset", "초기화", "リセット")
            ),
            false,
        )
    };
    // The page keys scroll the view without moving the selection, which is how detail and
    // Sync text past the edge are read.
    let page_keys = format!("{}/{}", k(Action::PageUp), k(Action::PageDown));
    let scroll = t!("scroll", "스크롤", "スクロール");
    let scroll_hint = || {
        overflowing.then(|| Hint {
            text: format!("{page_keys} {scroll}"),
            priority: Priority::Secondary,
        })
    };
    let fixed = |text: &str| hint(text.to_owned(), true);

    if st.editing_text && matches!(st.current_field(), Some(Field::ThemeColor(_))) {
        vec![
            fixed(t!(
                "type #RRGGBB or none",
                "#RRGGBB 또는 none 입력",
                "#RRGGBB または none を入力"
            )),
            fixed(t!("Enter save", "Enter 저장", "Enter 保存")),
            fixed(t!("Backspace delete", "Backspace 삭제", "Backspace 削除")),
        ]
    } else if st.editing_text {
        // While typing a path/key, Enter or Esc both commit *and* persist it immediately,
        // so the value can't be lost by leaving the screen later.
        vec![
            fixed(t!("type value", "값 입력", "値を入力")),
            fixed(t!(
                "Enter or Esc save",
                "Enter 또는 Esc 저장",
                "Enter または Esc 保存"
            )),
            fixed(t!("Backspace delete", "Backspace 삭제", "Backspace 削除")),
        ]
    } else if matches!(st.current_field(), Some(Field::ExportPersonalData)) {
        vec![
            hint(
                format!(
                    "{} {}",
                    k(Action::Confirm),
                    t!("export", "내보내기", "エクスポート")
                ),
                true,
            ),
            // One hint, so the privacy warning never splits or loses half of itself.
            fixed(t!(
                "unencrypted JSON · includes private listening history",
                "암호화되지 않은 JSON · 개인 감상 기록 포함",
                "暗号化されないJSON · 個人の再生履歴を含む"
            )),
        ]
    } else if matches!(st.current_field(), Some(Field::LocalCrossfade))
        && !crate::crossfade::overlap_support().is_available()
    {
        vec![
            fixed(t!(
                "saved, but this build cannot overlap two files",
                "저장되지만 이 빌드는 두 파일을 겹쳐 재생할 수 없어요",
                "保存されますがこのビルドは2つのファイルを重ねられません"
            )),
            fixed(t!(
                "transitions stay as today",
                "전환은 지금과 같아요",
                "切替は今のままです"
            )),
        ]
    } else if st.tab == SettingsTab::Sync {
        let mut hints = if app.server.settings.area == crate::app::SyncArea::Status {
            // Nothing to select here: the arrows and page keys all scroll the text.
            vec![hint(
                format!(
                    "{}/{} {page_keys} {scroll}",
                    k(Action::MoveUp),
                    k(Action::MoveDown)
                ),
                true,
            )]
        } else {
            let mut hints = vec![hint(
                format!(
                    "{}/{} {}",
                    k(Action::MoveUp),
                    k(Action::MoveDown),
                    t!("select", "선택", "選択")
                ),
                false,
            )];
            hints.extend(scroll_hint());
            hints.push(hint(
                format!("{} {}", k(Action::Confirm), t!("open", "열기", "開く")),
                true,
            ));
            hints
        };
        hints.push(hint(
            format!(
                "{}/{} {}",
                k(Action::ChangeDecrease),
                k(Action::ChangeIncrease),
                t!("area", "영역", "エリア")
            ),
            true,
        ));
        hints.push(switch_tab());
        hints.push(hint(
            format!(
                "{} {}",
                k(Action::SettingsCancel),
                t!("close", "닫기", "閉じる")
            ),
            true,
        ));
        hints
    } else if st.tab == SettingsTab::Keys {
        let mouse_row = st.row >= keymap::editable_entries().len();
        let rebind = if mouse_row {
            format!(
                "{}/{} {} {} {}",
                k(Action::ChangeDecrease),
                k(Action::ChangeIncrease),
                t!("or", "또는", "または"),
                k(Action::Confirm),
                t!("change", "변경", "変更"),
            )
        } else {
            format!(
                "{} {}",
                k(Action::Confirm),
                t!("rebind", "재설정", "再割り当て")
            )
        };
        vec![
            hint(
                format!(
                    "{}/{} {}",
                    k(Action::MoveUp),
                    k(Action::MoveDown),
                    t!("select", "선택", "選択")
                ),
                false,
            ),
            hint(rebind, true),
            reset(),
            switch_tab(),
            save_quit(),
        ]
    } else if matches!(st.current_field(), Some(Field::ThemeColor(_))) {
        let mut hints = vec![hint(
            format!(
                "{}/{} {}",
                k(Action::MoveUp),
                k(Action::MoveDown),
                t!("color", "색상", "カラー")
            ),
            false,
        )];
        hints.extend(scroll_hint());
        hints.extend([
            hint(
                format!("{} {}", k(Action::Confirm), t!("edit", "편집", "編集")),
                true,
            ),
            reset(),
            switch_tab(),
            save_quit(),
        ]);
        hints
    } else {
        let mut hints = vec![hint(
            format!(
                "{}/{} {}",
                k(Action::MoveUp),
                k(Action::MoveDown),
                t!("field", "이동", "移動")
            ),
            false,
        )];
        hints.extend(scroll_hint());
        hints.extend([
            Hint {
                text: format!(
                    "{}/{} {}",
                    k(Action::ChangeDecrease),
                    k(Action::ChangeIncrease),
                    t!("change", "변경", "変更")
                ),
                priority: Priority::Secondary,
            },
            hint(
                format!(
                    "{} {}",
                    k(Action::Confirm),
                    t!("edit/toggle", "편집/전환", "編集/切替")
                ),
                true,
            ),
            switch_tab(),
            save_quit(),
        ]);
        hints
    }
}

/// Footer rows the hints need at `width`: one when they fit, two when even the essential
/// hints must wrap. Hints never take more than two rows.
const FOOTER_MAX_ROWS: usize = 2;

/// Lay the hints out in `width` cells. Everything on one line when it fits; otherwise the
/// lowest-priority hints drop out, a pointer to the full key list (`help`) leads the line, and
/// the essential hints wrap onto a second line if they must.
fn fit_footer(hints: &[Hint], help: &str, width: usize) -> Vec<String> {
    const SEP: &str = "  ·  ";
    const TIGHT: &str = " · ";
    let fits = |line: &str| UnicodeWidthStr::width(line) <= width;
    let all: Vec<&str> = hints.iter().map(|h| h.text.as_str()).collect();
    let full = all.join(SEP);
    if fits(&full) {
        return vec![full];
    }
    // Drop the lowest priority first. The key-list pointer leads once anything is gone, so it
    // is the one hint that survives even the narrowest wrap.
    let keep_from = |floor: Priority| {
        let kept: Vec<&str> = hints
            .iter()
            .filter(|h| h.priority >= floor)
            .map(|h| h.text.as_str())
            .collect();
        if kept.len() < hints.len() {
            std::iter::once(help).chain(kept).collect()
        } else {
            kept
        }
    };
    for floor in [Priority::Secondary, Priority::Essential] {
        let kept = keep_from(floor);
        for sep in [SEP, TIGHT] {
            let line = kept.join(sep);
            if fits(&line) {
                return vec![line];
            }
        }
    }
    let kept = keep_from(Priority::Essential);
    // Two ways to wrap: whole hints per line (cleaner), or word by word (tighter, for one long
    // hint such as the export warning). Take whichever needs fewer lines.
    let mut by_hint: Vec<String> = Vec::new();
    let mut current = String::new();
    for text in &kept {
        let candidate = if current.is_empty() {
            (*text).to_owned()
        } else {
            format!("{current}{TIGHT}{text}")
        };
        if fits(&candidate) {
            current = candidate;
        } else {
            if !current.is_empty() {
                by_hint.push(std::mem::take(&mut current));
            }
            let mut parts = crate::ui::text::wrap_to_width(text, width.max(1));
            current = parts.pop().unwrap_or_default();
            by_hint.extend(parts);
        }
    }
    if !current.is_empty() {
        by_hint.push(current);
    }
    let joined = kept.join(TIGHT);
    let by_word = crate::ui::text::wrap_to_width(&joined, width.max(1));
    // Japanese copy has no spaces to break at, where breaking between any two characters is
    // the normal rule; this fills every line and so needs the fewest.
    let by_char = wrap_by_char(&joined, width.max(1));
    let mut lines = [by_word, by_char].into_iter().fold(by_hint, |best, next| {
        if next.len() < best.len() { next } else { best }
    });
    lines.truncate(FOOTER_MAX_ROWS);
    lines
}

/// Break `text` into lines of at most `width` cells between any two characters.
fn wrap_by_char(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if used + w > width && !current.is_empty() {
            lines.push(std::mem::take(&mut current).trim_end().to_owned());
            used = 0;
            if c == ' ' {
                continue;
            }
        }
        current.push(c);
        used += w;
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

/// The `?` key-list pointer shown when the footer had to drop hints.
fn help_hint(app: &App) -> String {
    format!(
        "{} {}",
        app.keymap
            .label_for_display(KeyContext::Global, Action::ToggleHelp, app.retro_mode()),
        t!("all keys", "전체 키", "全キー")
    )
}

/// Cells the footer text may use: the inner width minus the docked-bar collapse toggle.
fn footer_width(app: &App, width: u16) -> usize {
    let toggle = if app.player_bar_position() == crate::config::PlayerBarPosition::Bottom {
        2
    } else {
        0
    };
    usize::from(width).saturating_sub(toggle)
}

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

    // The footer takes a second row only when even its essential hints cannot share one line.
    // Sized with the optional scroll hint assumed, which never changes how the essential hints
    // wrap, so the hints drawn below always fit the rows reserved here.
    let text_width = footer_width(app, inner.width);
    let help = help_hint(app);
    let footer_rows = if crate::ui::status_band_active(app) {
        1
    } else {
        fit_footer(&footer_hints(app, st, true), &help, text_width)
            .len()
            .clamp(1, FOOTER_MAX_ROWS) as u16
    };
    let rows = Layout::vertical([
        Constraint::Length(1),                                        // tab bar
        Constraint::Length(1),                                        // spacer
        Constraint::Min(0),                                           // field list
        Constraint::Length(crate::ui::control_box::docked_rows(app)), // docked player bar
        Constraint::Length(footer_rows),                              // help
    ])
    .split(inner);

    // Each list renderer below records its length and focus; the Keys tab records neither.
    app.bridges.settings_list_len.set(None);
    app.bridges.settings_focus.set(None);
    render_tabs(frame, app, st, rows[0]);
    if st.tab == SettingsTab::Sync {
        music_server::render_sync_area_selector(frame, app, st, rows[1]);
    }
    if st.tab == SettingsTab::Keys {
        render_keys(frame, app, st, rows[2]);
    } else if st.tab == SettingsTab::Sync {
        // The area selector takes the spacer row, so the pane leaves one blank row under it.
        // Each pane is a list whose marker gutter lines its text up with the field tabs.
        let pane = Rect {
            y: rows[2].y.saturating_add(1),
            height: rows[2].height.saturating_sub(1),
            ..rows[2]
        };
        match app.server.settings.area {
            crate::app::SyncArea::Status => {
                music_server::render_status(frame, app, st, pane);
            }
            crate::app::SyncArea::PersonalState | crate::app::SyncArea::DevicesRecovery => {
                let model = app.sync_settings_model();
                render_sync(frame, app, st, &model, pane);
            }
            crate::app::SyncArea::MusicServer => {
                music_server::render_music_server(frame, app, st, pane);
            }
        }
    } else {
        render_fields(frame, app, st, rows[2]);
    }
    crate::ui::control_box::render_docked(frame, app, rows[3]);

    // The list was drawn above, so the overflow check reads this frame's numbers.
    let overflowing = app
        .bridges
        .settings_list_len
        .get()
        .is_some_and(|len| len > app.bridges.settings_scroll.viewport());
    let footer: Vec<Line> = fit_footer(&footer_hints(app, st, overflowing), &help, text_width)
        .into_iter()
        .map(Line::from)
        .collect();
    // The footer row doubles as the status/toast surface. An active status message (Spotify
    // connect/import feedback, errors, the browser/clipboard-fallback hint) takes the row so it
    // is visible without leaving Settings; otherwise the keybinding hint shows. Every other view
    // renders `app.status` — Settings must too, or account actions look like silent no-ops.
    // With the docked control box on screen its title row already shows the same status, so
    // the footer keeps the keybinding hint instead of doubling the message.
    if crate::ui::status_band_active(app) {
        crate::ui::render_status_band(frame, app, rows[4]);
    } else {
        frame.render_widget(
            Paragraph::new(footer).style(theme.style(R::TextMuted)),
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

fn render_tabs(frame: &mut Frame, app: &App, st: &SettingsState, area: Rect) {
    tabs::render(frame, app, st, area);
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

/// A toggle's state, read back from its display value so every toggle (including the Atlas
/// rows) shares the one source of truth in `value_display`.
fn toggle_on(st: &SettingsState, field: Field) -> bool {
    st.draft.value_display(field) == crate::settings::toggle_str(true)
}

/// A toggle drawn as a 3-cell switch: the knob sits right and the track is heavy when on.
/// The shape alone carries the state, because the focused row repaints every span in one
/// color. Retro keeps the `[x]` checkbox, which reads better on a console font.
fn switch_str(on: bool, retro: bool) -> String {
    match (on, retro) {
        (true, false) => "━━●".to_owned(),
        (false, false) => "○──".to_owned(),
        (on, true) => crate::settings::toggle_str(on),
    }
}

/// Value text shared by rendering and click-target measurement.
fn field_value_text(
    app: &App,
    st: &SettingsState,
    field: Field,
    focused: bool,
    width: usize,
) -> String {
    let retro = app.retro_mode();
    let bar = |value: f64, min: f64, max: f64| bar(value, min, max, retro);
    match (field, field.kind()) {
        (Field::ExportPersonalData, _) => st.personal_data_export.value_display(),
        (Field::AudioOutput, _) => app.audio_output_display_label(&st.draft.audio_mpv_device),
        (_, FieldKind::Toggle) => switch_str(toggle_on(st, field), retro),
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
        (Field::LocalCrossfade, _) => slider_str(
            &bar(
                f64::from(st.draft.local_crossfade.tenths()),
                0.0,
                f64::from(crate::crossfade::CrossfadeSecs::MAX.tenths()),
            ),
            &st.draft.local_crossfade.label(),
        ),
        (Field::Band(i), _) => slider_str(
            &centered_bar(st.draft.eq_bands[i], BAND_GAIN_MIN, BAND_GAIN_MAX, retro),
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
    let value_width = (area.width as usize)
        .saturating_sub(UnicodeWidthStr::width(HL_SYMBOL) + other_label_width(st.tab));

    // Build fields plus unselectable section headers/spacers and retain their index mapping.
    let mut items: Vec<ListItem> = Vec::new();
    let mut display_to_field: Vec<Option<usize>> = Vec::new();
    let mut selected = 0usize;

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
            // The header runs a light rule to the right edge so each group reads as its own
            // block. The list reserves the marker gutter on this row too.
            let fade = |s: Style| {
                crate::ui::anim::stagger_style(app, crate::app::Mode::Settings, items.len(), s)
            };
            let rule_width = (area.width as usize)
                .saturating_sub(UnicodeWidthStr::width(HL_SYMBOL))
                .saturating_sub(UnicodeWidthStr::width(*title) + 1);
            items.push(ListItem::new(Line::from(vec![
                Span::styled(
                    (*title).to_owned(),
                    fade(theme.style(R::SettingsGroup).add_modifier(Modifier::BOLD)),
                ),
                Span::styled(
                    format!(" {}", "─".repeat(rule_width)),
                    fade(theme.style(R::BorderMuted)),
                ),
            ])));
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

    // The focused row expands in place: its whole value (when the row cuts it off), its cost
    // meter, and its description follow it as unselectable rows. They scroll with the list, so
    // the wheel, the scrollbar, and keyboard navigation reach every line on any screen size.
    let detail = fields
        .get(focused_field)
        .map(|&field| detail_rows(app, st, field, area.width, value_width))
        .unwrap_or_default();
    let detail_len = detail.len();
    let at = (selected + 1).min(items.len());
    for (index, line) in detail.into_iter().enumerate() {
        items.insert(at + index, ListItem::new(line));
        display_to_field.insert(at + index, None);
    }

    let len = items.len();
    app.bridges.settings_list_len.set(Some(len));
    let first = display_to_field
        .iter()
        .position(Option::is_some)
        .unwrap_or(0);
    app.bridges
        .settings_focus
        .set(Some((first, selected, selected + detail_len)));
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
    // An already-visible cursor never moves the offset. Moving onto a row scrolls just far
    // enough to show its detail rows too (`resolve` caps that margin at half the viewport, and
    // the wheel reaches the rest). A wheel scroll may carry the focused row off-screen to read
    // long detail rows, so the row is selected only while it is inside the window: ratatui
    // would otherwise scroll it straight back into view.
    let offset = resolve_keeping_cursor_on_resize(
        &app.bridges.settings_scroll,
        selected,
        area.height,
        len,
        detail_len,
    );
    let mut state = ListState::default().with_offset(offset);
    if (offset..offset + usize::from(area.height)).contains(&selected) {
        state.select(Some(selected));
    }
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

/// [`ScrollState::resolve`](crate::ui::scroll::ScrollState::resolve), plus one rule: when the
/// viewport height changed since the last frame (a resize, zoom, or the docked bar toggling)
/// and the cursor ended up outside the window, scroll it back in. Only a wheel or scrollbar
/// move may leave the cursor off-screen, and the Settings lists rely on that to reach long
/// detail text.
pub(super) fn resolve_keeping_cursor_on_resize(
    scroll: &crate::ui::scroll::ScrollState,
    selected: usize,
    height: u16,
    len: usize,
    scrolloff: usize,
) -> usize {
    let resized = scroll.viewport() != usize::from(height);
    let offset = scroll.resolve(selected, height, len, scrolloff);
    let height = usize::from(height);
    if !resized || height == 0 || (offset..offset + height).contains(&selected) {
        return offset;
    }
    let offset = if selected < offset {
        selected
    } else {
        selected + 1 - height
    };
    scroll.set_offset(offset, len);
    scroll.offset()
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
    // An unset text field shows its default or "(none)" as a placeholder, muted so it does not
    // read as a value the user typed. Buttons take an action color; the resets that wipe
    // settings are red.
    let placeholder = field.kind() == FieldKind::Text
        && !(focused && st.editing_text)
        && st
            .draft
            .text_value(field)
            .is_none_or(|value| value.trim().is_empty());
    let value_role = if field == Field::ExportPersonalData && st.personal_data_export.is_busy() {
        R::TextMuted
    } else if focused {
        R::SettingsValueFocused
    } else if placeholder {
        R::TextMuted
    } else if matches!(field, Field::ResetAll | Field::ResetKeybindings) {
        R::Error
    } else if field.kind() == FieldKind::Button
        || (field.kind() == FieldKind::Toggle && toggle_on(st, field))
    {
        R::Accent
    } else if field.kind() == FieldKind::Toggle {
        R::TextMuted
    } else {
        R::SettingsValue
    };
    // The frame-rate slider is the one row whose value isn't a single flat span: the track cells
    // above the 30-fps mark are always red (a "this is heavy" danger zone) and the number reddens
    // once fps > 30. The glyphs are byte-identical to `field_value_text`'s `AnimFps` arm, so the
    // arrow hit-rects from `register_field_controls` stay in lockstep.
    if field == Field::AnimFps {
        let fps = st.draft.animations.effective_fps();
        let track = bar(
            f64::from(fps),
            f64::from(FPS_MIN),
            f64::from(FPS_MAX),
            app.retro_mode(),
        );
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

/// The focused field's detail rows, indented under its label: the whole value when the row had
/// to cut it off (a long path, a wide select label, the text being edited), the render-cost
/// meter for an animation effect, then the whole description. Everything wraps to the list
/// width, so no line runs past the edge.
fn detail_rows(
    app: &App,
    st: &SettingsState,
    field: Field,
    width: u16,
    value_width: usize,
) -> Vec<Line<'static>> {
    const INDENT: &str = "  ";
    let theme = &st.draft.theme;
    let retro = app.retro_mode();
    let inner = usize::from(width)
        .saturating_sub(UnicodeWidthStr::width(HL_SYMBOL) + INDENT.len())
        .max(1);
    let mut lines = Vec::new();
    if !matches!(field, Field::ThemeColor(_)) {
        // `usize::MAX` asks the editor window for the whole buffer; secrets stay masked
        // because `field_value_text` masks them at every width.
        let value = field_value_text(app, st, field, true, usize::MAX);
        if UnicodeWidthStr::width(value.as_str()) > value_width {
            let marker = if retro { "> " } else { "› " };
            for (i, part) in crate::ui::text::wrap_to_width(&value, inner.saturating_sub(2))
                .into_iter()
                .enumerate()
            {
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{INDENT}{}", if i == 0 { marker } else { "  " }),
                        theme.style(R::TextMuted),
                    ),
                    Span::styled(part, theme.style(R::SettingsValue)),
                ]));
            }
        }
    }
    if let Some(cost) = crate::settings::anim_cost(field) {
        let (full, empty) = if retro { ('#', '.') } else { ('■', '□') };
        let meter: String = (1..=5)
            .map(|i| if i <= cost { full } else { empty })
            .collect();
        lines.push(Line::from(vec![
            Span::styled(
                format!("{INDENT}{} ", t!("cost", "부하", "負荷")),
                theme.style(R::TextMuted),
            ),
            Span::styled(meter, theme.style(R::Warning)),
        ]));
    }
    for part in crate::ui::text::wrap_to_width(field.description(), inner) {
        lines.push(Line::from(Span::styled(
            format!("{INDENT}{part}"),
            theme.style(R::TextMuted),
        )));
    }
    lines
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

/// Cell count of every slider track.
const BAR_WIDTH: usize = 11;

/// The knob cell for `value` in `[min, max]` on a [`BAR_WIDTH`] track.
fn bar_pos(value: f64, min: f64, max: f64) -> usize {
    let frac = if max > min {
        ((value - min) / (max - min)).clamp(0.0, 1.0)
    } else {
        0.0
    };
    (frac * (BAR_WIDTH - 1) as f64).round() as usize
}

/// Draw a track whose cells in `filled` are heavy and the rest light, with the knob at `pos`.
/// Retro uses `=`/`-` because the console scrub folds both line weights into `-`.
fn track(pos: usize, filled: std::ops::RangeInclusive<usize>, retro: bool) -> String {
    let (heavy, light) = if retro { ('=', '-') } else { ('━', '─') };
    (0..BAR_WIDTH)
        .map(|i| {
            if i == pos {
                '●'
            } else if filled.contains(&i) {
                heavy
            } else {
                light
            }
        })
        .collect()
}

/// A compact slider track filled from the left edge up to `value`'s knob.
fn bar(value: f64, min: f64, max: f64, retro: bool) -> String {
    let pos = bar_pos(value, min, max);
    track(pos, 0..=pos, retro)
}

/// A slider track for a signed range (EQ gain): the fill runs from the centre to the knob, so a
/// boost and a cut read as opposite directions at a glance.
fn centered_bar(value: f64, min: f64, max: f64, retro: bool) -> String {
    let pos = bar_pos(value, min, max);
    let mid = bar_pos((min + max) / 2.0, min, max);
    track(pos, pos.min(mid)..=pos.max(mid), retro)
}
