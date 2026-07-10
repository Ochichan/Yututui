//! The `?` help overlay: a centered cheat-sheet of every keybinding, drawn on top of the
//! active view. Content is generated from the live [`crate::keymap::KeyMap`], so it always
//! reflects the current (possibly remapped) keys. Toggled by `Action::ToggleHelp`.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::app::App;
use crate::keymap::{self, Action, KeyContext, KeyMap};
use crate::t;
use crate::theme::ThemeRole as R;

/// Render the cheat-sheet as a centered popup over `area`.
pub fn render(frame: &mut Frame, app: &App, area: Rect) {
    render_sheet(
        frame,
        app,
        area,
        t!(" Help · keybindings ", " 도움말 · 단축키 "),
        (80, 80),
        SheetStyle::Compact,
        help_groups(app),
    );
}

/// Render the mouse cheat-sheet as a centered popup over `area`.
pub fn render_mouse(frame: &mut Frame, app: &App, area: Rect) {
    render_sheet(
        frame,
        app,
        area,
        t!(" Help · mouse ", " 도움말 · 마우스 "),
        (84, 82),
        SheetStyle::Roomy,
        mouse_help_groups(app),
    );
}

/// How each row of a cheat-sheet is laid out.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SheetStyle {
    /// One line per row — a right-aligned key column then a short label. Used by the
    /// keybindings sheet, whose right-hand labels are short and always fit.
    Compact,
    /// Each item stacked and spaced — the gesture label bold on its own line, the
    /// description word-wrapped and indented on the line(s) below, with a blank line
    /// between items. Used by the mouse sheet, whose descriptions are full sentences.
    Roomy,
}

/// Left indent of a Roomy gesture label; its description sits two cells further in.
const LABEL_INDENT: usize = 2;
const DESC_INDENT: usize = 4;
/// Gap kept between the two columns so wrapped text never collides across the seam.
const COL_GUTTER: usize = 2;

/// Shared cheat-sheet popup. Two columns when there's room (the whole list fits without
/// scrolling on most terminals); on narrow grids (small terminals, or text zoom shrinking
/// the virtual grid) it goes near-full-screen and single-column, and the wheel / arrow
/// keys scroll it (`bridges.help_scroll`) so every row stays reachable.
fn render_sheet(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    title: &str,
    (pct_w, pct_h): (u16, u16),
    style: SheetStyle,
    groups: Vec<(String, Vec<(String, String)>)>,
) {
    // Two ~38-col columns need ~76 usable columns; below that, one column reads better
    // than two crushed ones, and a small screen deserves the whole screen.
    let narrow = area.width < 80;
    let popup = if narrow {
        centered(area, 96, 96)
    } else {
        centered(area, pct_w, pct_h)
    };
    crate::ui::render_popup_background(frame, app, popup);

    let block = Block::default()
        .title(title.to_owned())
        .borders(Borders::ALL)
        .border_style(crate::ui::popup_style(app, R::BorderPrimary))
        .style(crate::ui::popup_style(app, R::TextPrimary));
    let inner = block.inner(popup);

    // Column rects first: wrapping needs the real column width before lines are built.
    let two_col = !narrow;
    let rects: Vec<Rect> = if two_col {
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(inner)
            .to_vec()
    } else {
        vec![inner]
    };
    // Wrap to the narrower column, minus a gutter so the two columns never touch.
    let gutter = if two_col { COL_GUTTER } else { 0 };
    let text_w = rects
        .iter()
        .map(|r| usize::from(r.width))
        .min()
        .unwrap_or(0)
        .saturating_sub(gutter);

    // Build one line-block per group (pre-wrapped, so `len` below counts real rows and the
    // existing scroll math keeps working), then place whole groups into columns.
    let blocks: Vec<Vec<Line<'static>>> = groups
        .into_iter()
        .map(|(gtitle, rows)| build_group(app, style, &gtitle, &rows, text_w))
        .collect();
    let columns: Vec<Vec<Line<'static>>> = if two_col {
        let (left, right) = split_blocks(blocks);
        vec![left, right]
    } else {
        vec![blocks.into_iter().flatten().collect()]
    };

    let len = columns.iter().map(Vec::len).max().unwrap_or(0);
    let offset = app.bridges.help_scroll.view(inner.height, len) as u16;

    // A scroll hint on the bottom border when rows continue below the fold.
    let more = len > usize::from(inner.height) + usize::from(offset);
    let block = if more {
        block.title_bottom(
            Line::from(t!(" ↓ scroll ", " ↓ 스크롤 "))
                .right_aligned()
                .style(crate::ui::popup_style(app, R::TextMuted)),
        )
    } else {
        block
    };
    frame.render_widget(block, popup);

    for (lines, rect) in columns.into_iter().zip(rects) {
        frame.render_widget(
            Paragraph::new(lines)
                .style(crate::ui::popup_style(app, R::TextPrimary))
                .scroll((offset, 0)),
            rect,
        );
    }
    crate::ui::seal_popup_background(frame, app, popup);
    crate::ui::mark_art_rows_for_popup(frame, app, popup);
}

/// Build the lines for one cheat-sheet group, wrapped to `text_w` display cells. The header
/// is always a bold group title; the row layout follows `style`.
fn build_group(
    app: &App,
    style: SheetStyle,
    title: &str,
    rows: &[(String, String)],
    text_w: usize,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        title.to_owned(),
        crate::ui::popup_style(app, R::HelpGroup).add_modifier(Modifier::BOLD),
    )));

    match style {
        SheetStyle::Compact => {
            for (key, label) in rows {
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{key:>8}  "),
                        crate::ui::popup_style(app, R::HelpKey),
                    ),
                    Span::styled(label.to_owned(), crate::ui::popup_style(app, R::HelpAction)),
                ]));
            }
            // A little padding after each group so the sections breathe.
            lines.push(Line::from(""));
        }
        SheetStyle::Roomy => {
            let key_style = crate::ui::popup_style(app, R::HelpKey).add_modifier(Modifier::BOLD);
            let desc_style = crate::ui::popup_style(app, R::HelpAction);
            // A blank line under the header, then each item stacked and spaced.
            lines.push(Line::from(""));
            for (label, desc) in rows {
                // Gesture label on its own line; wrap it too on the rare over-long label,
                // hanging the continuation two cells past the label indent.
                let label_w = text_w.saturating_sub(LABEL_INDENT);
                for (i, seg) in crate::ui::text::wrap_to_width(label, label_w)
                    .into_iter()
                    .enumerate()
                {
                    let indent = if i == 0 {
                        LABEL_INDENT
                    } else {
                        LABEL_INDENT + 2
                    };
                    lines.push(Line::from(Span::styled(
                        format!("{}{seg}", " ".repeat(indent)),
                        key_style,
                    )));
                }
                // Description word-wrapped and indented under the label.
                let desc_w = text_w.saturating_sub(DESC_INDENT);
                for seg in crate::ui::text::wrap_to_width(desc, desc_w) {
                    lines.push(Line::from(Span::styled(
                        format!("{}{seg}", " ".repeat(DESC_INDENT)),
                        desc_style,
                    )));
                }
                // One blank line between items.
                lines.push(Line::from(""));
            }
        }
    }
    lines
}

/// Place whole group-blocks into two columns, choosing the boundary that best balances the
/// two columns' heights (whole groups stay intact). Mirrors `settings.rs::render_keys`.
fn split_blocks(blocks: Vec<Vec<Line<'static>>>) -> (Vec<Line<'static>>, Vec<Line<'static>>) {
    let heights: Vec<usize> = blocks.iter().map(Vec::len).collect();
    let total: usize = heights.iter().sum();
    let n = heights.len();

    // `best_k` = how many leading groups go in the left column. Try 1..n so neither column
    // is empty; pick the split that minimizes the height difference.
    let mut best_k = 1usize;
    let mut best_diff = usize::MAX;
    let mut left_sum = 0usize;
    for k in 1..n {
        left_sum += heights[k - 1];
        let diff = left_sum.abs_diff(total - left_sum);
        if diff < best_diff {
            best_diff = diff;
            best_k = k;
        }
    }

    let mut left: Vec<Line> = Vec::new();
    let mut right: Vec<Line> = Vec::new();
    for (i, block) in blocks.into_iter().enumerate() {
        if i < best_k {
            left.extend(block);
        } else {
            right.extend(block);
        }
    }
    (left, right)
}

/// The keymap currently visible to the user. While Settings is open, its draft is the source of
/// truth so both cheat-sheets update immediately after capture, before the user saves or closes.
fn visible_keymap(app: &App) -> &KeyMap {
    app.settings
        .as_deref()
        .map_or(&app.keymap, |settings| &settings.keymap)
}

fn help_groups(app: &App) -> Vec<(String, Vec<(String, String)>)> {
    let keymap = visible_keymap(app);
    let mut out = Vec::new();
    for (ctx, actions) in keymap::groups() {
        let mut rows: Vec<(String, String)> = actions
            .iter()
            .map(|action| {
                let key = keymap.chord(ctx, *action).map_or_else(
                    || "—".to_owned(),
                    |chord| keymap::format_chord_for_display(chord, app.retro_mode()),
                );
                (key, action.human_label_for(ctx).to_owned())
            })
            .collect();
        // The text-input / results groups submit on Enter directly in their key handlers
        // (not via a keymap binding), so list that as a fixed row at the top of the group.
        if matches!(
            ctx,
            KeyContext::SearchInput | KeyContext::SearchResults | KeyContext::AiInput
        ) {
            rows.insert(0, fixed_enter_row(Action::Confirm.human_label_for(ctx)));
        }
        out.push((ctx.title().to_owned(), rows));
    }
    out
}

fn fixed_enter_row(label: &str) -> (String, String) {
    ("Enter".to_owned(), label.to_owned())
}

fn mouse_help_groups(app: &App) -> Vec<(String, Vec<(String, String)>)> {
    let context_menu_key = visible_keymap(app)
        .chord(KeyContext::Global, Action::OpenContextMenu)
        .map_or_else(
            || "—".to_owned(),
            |chord| keymap::format_chord_for_display(chord, app.retro_mode()),
        );
    let context_menu_fallback = (
        format!(
            "{} · {context_menu_key}",
            t!("Open context menu", "문맥 메뉴 열기")
        ),
        t!(
            "Open the selected row's actions without right click. The shortcut shown here updates immediately from Settings › Keys.",
            "우클릭 없이 선택한 행의 동작 메뉴를 엽니다. 여기에 표시되는 단축키는 설정 › 단축키의 변경값을 즉시 반영합니다."
        )
        .to_owned(),
    );
    vec![
        (
            t!("Common", "공통").to_owned(),
            vec![
                mouse_row(
                    "Left click nav",
                    "상단 탭 클릭",
                    "Switch Player, Search, Library, Settings, DJ Gem.",
                    "Player, Search, Library, Settings, DJ Gem 화면으로 이동합니다.",
                ),
                mouse_row(
                    "Footer ?",
                    "하단 ?",
                    "Open the keybinding cheat-sheet.",
                    "단축키 치트시트를 엽니다.",
                ),
                mouse_row(
                    "Footer mouse",
                    "하단 마우스",
                    "Open this mouse cheat-sheet; no keyboard shortcut is bound.",
                    "이 마우스 치트시트를 엽니다. 별도 단축키는 없습니다.",
                ),
                mouse_row(
                    "Wheel / scrollbar",
                    "휠 / 스크롤바",
                    "Scroll the visible list; on Player volume, wheel changes volume.",
                    "보이는 목록을 스크롤합니다. Player 볼륨 영역에서는 볼륨을 조절합니다.",
                ),
                mouse_row(
                    "Ctrl + wheel",
                    "Ctrl + 휠",
                    "Zoom the text like a browser (also Ctrl+-/=; kitty, Windows Terminal, …).",
                    "브라우저처럼 글자를 확대/축소합니다 (Ctrl+-/= 도 동일, kitty·Windows Terminal 등).",
                ),
                mouse_row(
                    "Picker row click",
                    "피커 행 클릭",
                    "In the add-to-playlist popup, add the pending song(s) to that playlist.",
                    "플레이리스트 추가 팝업에서 해당 플레이리스트로 곡을 추가합니다.",
                ),
                mouse_row(
                    "Right click / double",
                    "우클릭 / 우더블클릭",
                    "On supported rows, right click opens actions and right double-click activates quickly; customize each surface in Settings › Keys.",
                    "지원하는 행에서 우클릭은 동작 메뉴를 열고 우더블클릭은 빠르게 실행합니다. 설정 › 단축키에서 화면별로 바꿀 수 있습니다.",
                ),
                context_menu_fallback,
                mouse_row(
                    "Terminal-owned menu",
                    "터미널 자체 메뉴",
                    "If the host terminal consumes right click, the TUI cannot intercept it. Disable/rebind that terminal action or use the keyboard fallback shown above.",
                    "호스트 터미널이 우클릭을 먼저 소비하면 TUI는 가로챌 수 없습니다. 터미널 동작을 끄거나 바꾸고 위의 키보드 대체 키를 사용하세요.",
                ),
            ],
        ),
        (
            t!("Player", "플레이어").to_owned(),
            vec![
                mouse_row(
                    "Transport",
                    "재생 컨트롤",
                    "Click previous, play/pause, next, volume -/+. ",
                    "이전, 재생/일시정지, 다음, 볼륨 -/+ 를 클릭합니다.",
                ),
                mouse_row(
                    "Seek bar",
                    "탐색 막대",
                    "Click a position to seek there.",
                    "원하는 위치를 클릭해 그 시점으로 이동합니다.",
                ),
                mouse_row(
                    "Queue count",
                    "대기열 숫자",
                    "Click N/M to open the queue window.",
                    "N/M 표시를 클릭해 대기열 창을 엽니다.",
                ),
                mouse_row(
                    "Rating / menus",
                    "평가 / 메뉴",
                    "Click heart/thumb, shuffle, repeat, eq, or streaming status labels.",
                    "하트/싫어요, 셔플, 반복, eq, streaming 상태 라벨을 클릭합니다.",
                ),
            ],
        ),
        (
            t!("Queue window", "대기열 창").to_owned(),
            vec![
                mouse_row(
                    "Left click row",
                    "행 좌클릭",
                    "Select a row and start a drag range there.",
                    "행을 선택하고 드래그 범위의 시작점으로 둡니다.",
                ),
                mouse_row(
                    "Double click row",
                    "행 더블클릭",
                    "Jump playback to that queue entry.",
                    "해당 대기열 곡으로 바로 이동해 재생합니다.",
                ),
                mouse_row(
                    "Drag rows",
                    "행 드래그",
                    "Extend the selected queue range.",
                    "대기열 선택 범위를 확장합니다.",
                ),
                mouse_row(
                    "Right click row",
                    "행 우클릭",
                    "Open actions for the selected queue row or range (play from here, remove).",
                    "선택한 대기열 행 또는 범위의 동작 메뉴를 엽니다 (여기서 재생, 삭제).",
                ),
                mouse_row(
                    "Click x",
                    "x 클릭",
                    "Remove that one queue entry.",
                    "해당 대기열 항목 하나를 삭제합니다.",
                ),
                mouse_row(
                    "Outside click",
                    "바깥 클릭",
                    "Close the queue window.",
                    "대기열 창을 닫습니다.",
                ),
            ],
        ),
        (
            t!("Search", "검색").to_owned(),
            vec![
                mouse_row(
                    "Search box",
                    "검색 상자",
                    "Click to focus typing.",
                    "클릭해 입력 포커스를 둡니다.",
                ),
                mouse_row(
                    "Source chip",
                    "소스 칩",
                    "Open the search-source menu; click a menu row to choose it.",
                    "검색 소스 메뉴를 열고, 메뉴 행을 클릭해 선택합니다.",
                ),
                mouse_row(
                    "Search button",
                    "검색 버튼",
                    "Submit the current query.",
                    "현재 검색어로 검색합니다.",
                ),
                mouse_row(
                    "Filter button",
                    "필터 버튼",
                    "Open the results-filter popup (keyboard: `/`).",
                    "결과 필터 팝업을 엽니다 (키보드는 `/`).",
                ),
                mouse_row(
                    "Result click",
                    "결과 클릭",
                    "Single-click selects; double-click plays now.",
                    "한 번 클릭은 선택, 더블클릭은 바로 재생입니다.",
                ),
                mouse_row(
                    "Result right click",
                    "결과 우클릭",
                    "Open result actions such as play, enqueue, favorite, playlist, or download.",
                    "재생, 큐 추가, 즐겨찾기, 플레이리스트, 다운로드 등의 결과 동작을 엽니다.",
                ),
            ],
        ),
        (
            t!("Library", "라이브러리").to_owned(),
            vec![
                mouse_row(
                    "Tab click",
                    "탭 클릭",
                    "Switch All, Favorites, History, Radio, Downloads, Playlists.",
                    "All, Favorites, History, Radio, Downloads, Playlists 탭을 전환합니다.",
                ),
                mouse_row(
                    "Row click",
                    "행 클릭",
                    "Single-click selects; double-click plays now (opens a playlist).",
                    "한 번 클릭은 선택, 더블클릭은 바로 재생(플레이리스트는 열기)입니다.",
                ),
                mouse_row(
                    "Row right click",
                    "행 우클릭",
                    "Open actions for that song, playlist, or selected range.",
                    "해당 곡, 플레이리스트 또는 선택 범위의 동작 메뉴를 엽니다.",
                ),
                mouse_row(
                    "Drag rows",
                    "행 드래그",
                    "Select a range for play, enqueue, or delete actions.",
                    "재생, 큐 추가, 삭제에 쓸 범위를 선택합니다.",
                ),
                mouse_row(
                    "Click x",
                    "x 클릭",
                    "Remove/unfavorite/forget/download-delete by the active tab.",
                    "현재 탭 의미에 맞게 제거, 즐겨찾기 해제, 기록 삭제, 파일 삭제를 합니다.",
                ),
                mouse_row(
                    "Breadcrumb click",
                    "브레드크럼 클릭",
                    "Inside a playlist, go back to the playlist list.",
                    "플레이리스트 안에서 목록으로 돌아갑니다.",
                ),
            ],
        ),
        (
            t!("Local Deck", "로컬 덱").to_owned(),
            vec![
                mouse_row(
                    "Row right click",
                    "행 우클릭",
                    "Open the safe actions available for that local row.",
                    "해당 로컬 행에서 사용할 수 있는 안전한 동작 메뉴를 엽니다.",
                ),
                mouse_row(
                    "Row right double-click",
                    "행 우더블클릭",
                    "Activate that local row by default; customize it in Settings › Keys.",
                    "기본값은 해당 로컬 행을 실행합니다. 설정 › 단축키에서 바꿀 수 있습니다.",
                ),
            ],
        ),
        (
            t!("Settings", "설정").to_owned(),
            vec![
                mouse_row(
                    "Tab click",
                    "탭 클릭",
                    "Switch Settings sections.",
                    "설정 섹션을 전환합니다.",
                ),
                mouse_row(
                    "Field row",
                    "필드 행",
                    "Click a row or value to focus/activate it; Keys includes safe mouse presets.",
                    "행이나 값을 클릭해 포커스하거나 실행합니다. 단축키 탭에는 안전한 마우스 프리셋도 있습니다.",
                ),
                mouse_row(
                    "Checkbox / arrows",
                    "체크박스 / 화살표",
                    "Toggle booleans or step select/slider values.",
                    "토글을 바꾸거나 선택/슬라이더 값을 한 단계 조절합니다.",
                ),
                mouse_row(
                    "Confirm buttons",
                    "확인 버튼",
                    "Click Delete/Cancel or Save/Cancel in confirmation popups.",
                    "확인 팝업의 Delete/Cancel 또는 Save/Cancel을 클릭합니다.",
                ),
            ],
        ),
        (
            t!("DJ Gem", "DJ Gem").to_owned(),
            vec![
                mouse_row(
                    "Suggestions wheel",
                    "추천 휠",
                    "Wheel or drag the scrollbar when suggestions overflow.",
                    "추천 목록이 넘치면 휠이나 스크롤바로 이동합니다.",
                ),
                mouse_row(
                    "Footer",
                    "하단",
                    "Use the same keybinding and mouse cheat-sheet buttons.",
                    "같은 단축키/마우스 치트시트 버튼을 씁니다.",
                ),
            ],
        ),
        (
            t!("Popups", "팝업").to_owned(),
            vec![
                mouse_row(
                    "Dropdown row",
                    "드롭다운 행",
                    "Click a row to select it; click elsewhere to close.",
                    "행을 클릭해 선택하고, 바깥을 클릭해 닫습니다.",
                ),
                mouse_row(
                    "Filter popup",
                    "필터 팝업",
                    "Click selects; double-click plays; right-click opens actions; wheel scrolls.",
                    "클릭 선택, 더블클릭 재생, 우클릭 동작 메뉴, 휠 스크롤.",
                ),
                mouse_row(
                    "Help/About",
                    "도움말/About",
                    "Click anywhere outside special links to dismiss.",
                    "특수 링크 외 아무 곳이나 클릭해 닫습니다.",
                ),
            ],
        ),
    ]
}

fn mouse_row(action_en: &str, action_ko: &str, desc_en: &str, desc_ko: &str) -> (String, String) {
    (
        t!(action_en, action_ko).to_owned(),
        t!(desc_en, desc_ko).to_owned(),
    )
}

/// A rect centered in `area`, sized to the given width/height percentages.
fn centered(area: Rect, pct_w: u16, pct_h: u16) -> Rect {
    let w = area.width * pct_w / 100;
    let h = area.height * pct_h / 100;
    Rect {
        x: area.x + area.width.saturating_sub(w) / 2,
        y: area.y + area.height.saturating_sub(h) / 2,
        width: w,
        height: h,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Flatten a rendered line's spans back into plain text for width/prefix assertions.
    fn line_text(line: &Line) -> String {
        line.spans.iter().map(|sp| sp.content.as_ref()).collect()
    }

    #[test]
    fn roomy_group_wraps_long_description_and_spaces_items() {
        use unicode_width::UnicodeWidthStr;
        let _guard = crate::i18n::lock_for_test();
        let app = App::new(100);
        let rows = vec![
            (
                "Ctrl + wheel".to_owned(),
                "Zoom the text like a browser (also Ctrl+-/=; kitty, Windows Terminal, …)."
                    .to_owned(),
            ),
            (
                "Seek bar".to_owned(),
                "Click a position to seek there.".to_owned(),
            ),
        ];
        let text_w = 28;
        let lines = build_group(&app, SheetStyle::Roomy, "Player", &rows, text_w);

        // Nothing overflows the wrap width (display cells, not scalar count).
        for line in &lines {
            assert!(
                UnicodeWidthStr::width(line_text(line).as_str()) <= text_w,
                "line exceeds width: {:?}",
                line_text(line)
            );
        }
        // The long description wrapped onto more than one indented line.
        let desc_lines = lines
            .iter()
            .filter(|l| line_text(l).starts_with("    ") && !line_text(l).trim().is_empty())
            .count();
        assert!(
            desc_lines > rows.len(),
            "descriptions should wrap: {desc_lines} lines"
        );
        // A blank under the header plus one after each of the two items.
        let blanks = lines
            .iter()
            .filter(|l| line_text(l).trim().is_empty())
            .count();
        assert_eq!(blanks, 3, "header spacer + one blank between each item");
    }

    #[test]
    fn compact_group_keeps_one_line_rows() {
        let _guard = crate::i18n::lock_for_test();
        let app = App::new(100);
        let rows = vec![("Space".to_owned(), "Play / pause".to_owned())];
        let lines = build_group(&app, SheetStyle::Compact, "Player", &rows, 40);
        // Header, one packed key+label row, one trailing blank — the pre-existing shape.
        assert_eq!(lines.len(), 3);
        assert_eq!(line_text(&lines[0]), "Player");
        assert_eq!(line_text(&lines[1]), "   Space  Play / pause");
        assert_eq!(line_text(&lines[2]), "");
    }

    #[test]
    fn search_enter_rows_are_listed_as_fixed_help_rows() {
        let _guard = crate::i18n::lock_for_test();
        let app = App::new(100);
        let groups = help_groups(&app);

        let search_box = groups
            .iter()
            .find_map(|(title, rows)| (title == "Search box").then_some(rows))
            .expect("search box group");
        // Enter→Search is a fixed row at the top; the keymap-driven rows follow it.
        assert_eq!(
            search_box.first(),
            Some(&("Enter".to_owned(), "Search".to_owned()))
        );
        assert!(search_box.contains(&("^a".to_owned(), "Select all".to_owned())));
        assert!(search_box.contains(&("Tab".to_owned(), "Open source menu".to_owned())));
        assert!(search_box.contains(&("⇧Tab".to_owned(), "Focus search results".to_owned())));

        let ai_box = groups
            .iter()
            .find_map(|(title, rows)| (title == "DJ Gem box").then_some(rows))
            .expect("ai box group");
        assert_eq!(
            ai_box.first(),
            Some(&("Enter".to_owned(), "Send".to_owned()))
        );
        assert!(ai_box.contains(&("^a".to_owned(), "Select all".to_owned())));

        let search_results = groups
            .iter()
            .find_map(|(title, rows)| (title == "Search results").then_some(rows))
            .expect("search results group");
        assert_eq!(
            search_results.first(),
            Some(&("Enter".to_owned(), "Play selected".to_owned()))
        );
        assert!(search_results.contains(&("⇧Tab".to_owned(), "Focus search box".to_owned())));
        assert!(search_results.contains(&("Tab".to_owned(), "Open source menu".to_owned())));
        // `\` adds to the queue.
        assert!(search_results.contains(&("\\".to_owned(), "Add to queue".to_owned())));
    }

    #[test]
    fn playlists_context_gets_its_own_cheat_sheet_group() {
        let _guard = crate::i18n::lock_for_test();
        let app = App::new(100);
        let groups = help_groups(&app);

        let playlists = groups
            .iter()
            .find_map(|(title, rows)| (title == "Playlists").then_some(rows))
            .expect("playlists group");
        assert!(playlists.contains(&("Enter".to_owned(), "Open / play selected".to_owned())));
        assert!(playlists.contains(&("a".to_owned(), "Play playlist".to_owned())));
        assert!(playlists.contains(&("n".to_owned(), "New playlist".to_owned())));
        assert!(
            playlists.contains(&("Del".to_owned(), "Delete playlist / remove song".to_owned()))
        );
        assert!(playlists.contains(&("q".to_owned(), "Back / close".to_owned())));
        assert!(playlists.contains(&("p".to_owned(), "Add to playlist".to_owned())));

        let library = groups
            .iter()
            .find_map(|(title, rows)| (title == "Library").then_some(rows))
            .expect("library group");
        assert!(library.contains(&("p".to_owned(), "Add to playlist".to_owned())));
    }

    #[test]
    fn retro_help_uses_word_key_labels() {
        let _guard = crate::i18n::lock_for_test();
        let mut app = App::new(100);
        app.config.retro_mode = true;
        let groups = help_groups(&app);

        let player = groups
            .iter()
            .find_map(|(title, rows)| (title == "Player").then_some(rows))
            .expect("player group");
        assert!(player.contains(&("Space".to_owned(), "Play / pause".to_owned())));
        assert!(player.contains(&("Alt+Shift+R".to_owned(), "Radio/Normal mode".to_owned())));
        assert!(player.contains(&("Left".to_owned(), "Seek backward".to_owned())));
        assert!(player.contains(&("Right".to_owned(), "Seek forward".to_owned())));

        let nav = groups
            .iter()
            .find_map(|(title, rows)| (title == "Navigation (all screens)").then_some(rows))
            .expect("navigation group");
        assert!(nav.contains(&("Up".to_owned(), "Move up".to_owned())));
        assert!(nav.contains(&("Down".to_owned(), "Move down".to_owned())));
        assert!(nav.contains(&("Shift+Tab".to_owned(), "Previous tab / focus".to_owned())));

        let global = groups
            .iter()
            .find_map(|(title, rows)| (title == "Global").then_some(rows))
            .expect("global group");
        assert!(global.contains(&("Ctrl+R".to_owned(), "Toggle autoplay".to_owned())));
        assert!(!global.iter().any(|(_, label)| label == "Radio/Normal mode"));
    }

    #[test]
    fn library_lists_play_add_to_queue_and_play_whole_tab() {
        let _guard = crate::i18n::lock_for_test();
        let app = App::new(100);
        let library = help_groups(&app)
            .into_iter()
            .find_map(|(title, rows)| (title == "Library").then_some(rows))
            .expect("library group");
        for row in [
            ("Enter".to_owned(), "Play selected".to_owned()),
            ("M-⇧l".to_owned(), "Enter / exit Local Deck".to_owned()),
            ("\\".to_owned(), "Add to queue".to_owned()),
            ("a".to_owned(), "Play whole tab".to_owned()),
        ] {
            assert!(library.contains(&row), "cheat-sheet should list {row:?}");
        }
    }

    #[test]
    fn local_deck_group_lists_accept_all_import_candidates() {
        let _guard = crate::i18n::lock_for_test();
        let app = App::new(100);
        let local_deck = help_groups(&app)
            .into_iter()
            .find_map(|(title, rows)| (title == "Local Deck").then_some(rows))
            .expect("local deck group");
        assert!(local_deck.contains(&("A".to_owned(), "Accept all import candidates".to_owned())));
    }

    #[test]
    fn player_lists_delete_current_queue_binding() {
        let _guard = crate::i18n::lock_for_test();
        let app = App::new(100);
        let player = help_groups(&app)
            .into_iter()
            .find_map(|(title, rows)| (title == "Player").then_some(rows))
            .expect("player group");

        assert!(player.contains(&("Del".to_owned(), "Remove current from queue".to_owned())));
    }

    #[test]
    fn page_and_jump_keys_appear_in_the_cheatsheet() {
        let _guard = crate::i18n::lock_for_test();
        let app = App::new(100);
        let nav = help_groups(&app)
            .into_iter()
            .find_map(|(title, rows)| (title == "Navigation (all screens)").then_some(rows))
            .expect("navigation group");
        for row in [
            ("PgUp".to_owned(), "Page up".to_owned()),
            ("PgDn".to_owned(), "Page down".to_owned()),
            ("Home".to_owned(), "Jump to top".to_owned()),
            ("End".to_owned(), "Jump to bottom".to_owned()),
        ] {
            assert!(nav.contains(&row), "cheat-sheet should list {row:?}");
        }
    }

    #[test]
    fn text_zoom_appears_in_both_cheat_sheets() {
        let _guard = crate::i18n::lock_for_test();
        let app = App::new(100);

        // Keyboard sheet: the Global group lists both zoom chords via the keymap table.
        let global = help_groups(&app)
            .into_iter()
            .find_map(|(title, rows)| (title == "Global").then_some(rows))
            .expect("global group");
        for row in [
            ("^=".to_owned(), "Text size up".to_owned()),
            ("^-".to_owned(), "Text size down".to_owned()),
        ] {
            assert!(global.contains(&row), "cheat-sheet should list {row:?}");
        }

        // Mouse sheet: the Common group explains Ctrl+wheel.
        let common = mouse_help_groups(&app)
            .into_iter()
            .find_map(|(title, rows)| (title == "Common").then_some(rows))
            .expect("common group");
        assert!(
            common
                .iter()
                .any(|(gesture, what)| gesture == "Ctrl + wheel" && what.contains("Zoom")),
            "mouse cheat-sheet should explain Ctrl+wheel zoom"
        );
    }

    #[test]
    fn context_menu_fallback_is_editable_and_draft_live_in_both_cheat_sheets() {
        let _guard = crate::i18n::lock_for_test();
        crate::i18n::set_language(crate::i18n::Language::English);
        assert!(
            keymap::editable_entries().contains(&(KeyContext::Global, Action::OpenContextMenu)),
            "the keyboard Settings list must expose the context-menu fallback"
        );

        let mut app = App::new(100);
        app.update(crate::app::Msg::Key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('o'),
            crossterm::event::KeyModifiers::NONE,
        )));
        let f8 = keymap::parse_chord("f8").expect("F8 chord");
        app.settings
            .as_mut()
            .expect("Settings draft")
            .keymap
            .rebind(KeyContext::Global, Action::OpenContextMenu, f8)
            .expect("unused Global chord");

        assert_ne!(
            app.keymap
                .chord(KeyContext::Global, Action::OpenContextMenu),
            Some(f8),
            "the committed keymap remains unchanged until Settings closes"
        );
        let global = help_groups(&app)
            .into_iter()
            .find_map(|(title, rows)| (title == "Global").then_some(rows))
            .expect("Global keyboard group");
        assert!(global.contains(&("F8".to_owned(), "Open context menu".to_owned())));

        let common = mouse_help_groups(&app)
            .into_iter()
            .find_map(|(title, rows)| (title == "Common").then_some(rows))
            .expect("Common mouse group");
        assert!(
            common
                .iter()
                .any(|(label, _)| label == "Open context menu · F8")
        );
        assert!(
            common
                .iter()
                .all(|(label, description)| !label.contains("F10") && !description.contains("F10")),
            "mouse help must not retain the old hardcoded default"
        );
    }
}
