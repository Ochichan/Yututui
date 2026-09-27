//! The Settings footer: which key hints the current tab or field shows, and how they fit a
//! terminal of any width (dropping the least important first, then wrapping onto a second row).

use unicode_width::UnicodeWidthStr;

use crate::app::App;
use crate::keymap::{self, Action, KeyContext};
use crate::settings::{Field, SettingsState, SettingsTab};
use crate::t;

/// One footer hint. Essential hints (how to open or edit the focused item, switch area or tab,
/// and leave) are never dropped. On a terminal too narrow for all of them the plain arrow hints
/// go first, then the secondary ones (page scroll, ←/→ change); the `?` key list names them all.
pub(super) struct Hint {
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
pub(super) fn footer_hints(app: &App, st: &SettingsState, overflowing: bool) -> Vec<Hint> {
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
pub(super) const FOOTER_MAX_ROWS: usize = 2;

/// Lay the hints out in `width` cells. Everything on one line when it fits; otherwise the
/// lowest-priority hints drop out, a pointer to the full key list (`help`) leads the line, and
/// the essential hints wrap onto a second line if they must.
pub(super) fn fit_footer(hints: &[Hint], help: &str, width: usize) -> Vec<String> {
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
pub(super) fn help_hint(app: &App) -> String {
    format!(
        "{} {}",
        app.keymap
            .label_for_display(KeyContext::Global, Action::ToggleHelp, app.retro_mode()),
        t!("all keys", "전체 키", "全キー")
    )
}

/// Cells the footer text may use: the inner width minus the docked-bar collapse toggle.
pub(super) fn footer_width(app: &App, width: u16) -> usize {
    let toggle = if app.player_bar_position() == crate::config::PlayerBarPosition::Bottom {
        2
    } else {
        0
    };
    usize::from(width).saturating_sub(toggle)
}
