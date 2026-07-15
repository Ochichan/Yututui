//! Small display-width text helpers, shared across views. `format!`'s `{:<w}` / `{:>w}` and
//! `str::chars().count()` count Unicode scalar values, which misaligns two-cell East-Asian
//! characters (Korean, Japanese, Chinese). These measure with `unicode-width` instead, so columns
//! line up flush in every UI language.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// The visible slices around an editable field's caret. The caret itself is rendered by the
/// caller so it can keep the surface's existing style and blink animation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditableWindow {
    pub before: String,
    pub after: String,
}

/// Keep the graphemes nearest `cursor` visible inside a one-line editor.
///
/// `width` includes the caret's one terminal cell. When the full value does not fit, cells are
/// filled alternately to the left and right of the caret, then naturally backfilled from the side
/// that still has room. This makes the caret visible without keeping another scroll offset and
/// never slices a wide or multi-scalar grapheme.
pub fn editable_window(value: &str, cursor: usize, width: usize) -> EditableWindow {
    let text_width = width.saturating_sub(1);
    if text_width == 0 {
        return EditableWindow {
            before: String::new(),
            after: String::new(),
        };
    }

    let cursor = previous_grapheme_boundary(value, cursor.min(value.len()));
    let before: Vec<&str> = value[..cursor].graphemes(true).collect();
    let after: Vec<&str> = value[cursor..].graphemes(true).collect();
    if UnicodeWidthStr::width(value) <= text_width {
        return EditableWindow {
            before: value[..cursor].to_owned(),
            after: value[cursor..].to_owned(),
        };
    }

    let mut left = before.len();
    let mut right = 0usize;
    let mut left_width = 0usize;
    let mut right_width = 0usize;
    let mut left_blocked = false;
    let mut right_blocked = false;
    while left_width + right_width < text_width && !(left_blocked && right_blocked) {
        let prefer_left = left_width <= right_width;
        let mut added = false;
        for use_left in [prefer_left, !prefer_left] {
            if use_left {
                if left == 0 || left_blocked {
                    continue;
                }
                let candidate_width = UnicodeWidthStr::width(before[left - 1]);
                if left_width + right_width + candidate_width <= text_width {
                    left -= 1;
                    left_width += candidate_width;
                    added = true;
                    break;
                }
                left_blocked = true;
            } else {
                if right == after.len() || right_blocked {
                    continue;
                }
                let candidate_width = UnicodeWidthStr::width(after[right]);
                if left_width + right_width + candidate_width <= text_width {
                    right += 1;
                    right_width += candidate_width;
                    added = true;
                    break;
                }
                right_blocked = true;
            }
        }
        if !added && (left_blocked || left == 0) && (right_blocked || right == after.len()) {
            break;
        }
    }

    EditableWindow {
        before: before[left..].concat(),
        after: after[..right].concat(),
    }
}

/// Render a secret as one bullet per displayed grapheme while retaining the real caret position.
pub fn masked_editable_window(value: &str, cursor: usize, width: usize) -> EditableWindow {
    let cursor = previous_grapheme_boundary(value, cursor.min(value.len()));
    let before_count = value[..cursor].graphemes(true).count();
    let grapheme_count = value.graphemes(true).count();
    let masked = "•".repeat(grapheme_count);
    editable_window(&masked, "•".repeat(before_count).len(), width)
}

/// Compose a one-cell caret into a normal or masked editable window.
pub fn editable_value(
    value: &str,
    cursor: usize,
    width: usize,
    caret: char,
    masked: bool,
) -> String {
    let window = if masked {
        masked_editable_window(value, cursor, width)
    } else {
        editable_window(value, cursor, width)
    };
    format!("{}{caret}{}", window.before, window.after)
}

fn previous_grapheme_boundary(value: &str, cursor: usize) -> usize {
    if cursor == value.len() {
        return cursor;
    }
    value
        .grapheme_indices(true)
        .map(|(index, _)| index)
        .take_while(|&index| index <= cursor)
        .last()
        .unwrap_or(0)
}

/// Right-pad `s` with spaces to a target terminal *display* width (CJK-aware). Returns `s`
/// unchanged when it is already at least `width` cells wide.
pub fn pad_to_width(s: &str, width: usize) -> String {
    let pad = width.saturating_sub(UnicodeWidthStr::width(s));
    format!("{s}{}", " ".repeat(pad))
}

/// Truncate `s` to at most `max` display cells, never splitting a wide character. Returns the
/// owned prefix that fits (the whole string when it already fits).
pub fn truncate_to_width(s: &str, max: usize) -> String {
    if UnicodeWidthStr::width(s) <= max {
        return s.to_owned();
    }
    let mut out = String::new();
    let mut w = 0usize;
    for ch in s.chars() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if w + cw > max {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out
}

/// Keep the right edge of editable text visible, reserving one leading cell for an ellipsis when
/// truncation is necessary. This is CJK-aware and never splits a wide character.
pub fn tail_to_width(s: &str, max: usize) -> String {
    if UnicodeWidthStr::width(s) <= max {
        return s.to_owned();
    }
    if max == 0 {
        return String::new();
    }
    let available = max.saturating_sub(1);
    let mut reversed = Vec::new();
    let mut width = 0usize;
    for ch in s.chars().rev() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + ch_width > available {
            break;
        }
        reversed.push(ch);
        width += ch_width;
    }
    let mut out = String::from("…");
    out.extend(reversed.into_iter().rev());
    out
}

/// Word-wrap `text` to at most `width` display cells per line (CJK-aware). Splits on
/// whitespace, collapses runs of whitespace to a single break, and hard-breaks a single
/// word longer than `width` at the cell boundary so nothing ever overflows. `width` is
/// floored at 1. Empty / whitespace-only input yields a single empty line.
pub fn wrap_to_width(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0usize;
    for word in text.split_whitespace() {
        let word_w = UnicodeWidthStr::width(word);
        if cur.is_empty() {
            push_word(&mut out, &mut cur, &mut cur_w, word, width);
        } else if cur_w + 1 + word_w <= width {
            cur.push(' ');
            cur.push_str(word);
            cur_w += 1 + word_w;
        } else {
            out.push(std::mem::take(&mut cur));
            cur_w = 0;
            push_word(&mut out, &mut cur, &mut cur_w, word, width);
        }
    }
    if !cur.is_empty() || out.is_empty() {
        out.push(cur);
    }
    out
}

/// Append `word` to the current line, hard-breaking it across lines when it alone exceeds
/// `width` (never splitting a wide character).
fn push_word(out: &mut Vec<String>, cur: &mut String, cur_w: &mut usize, word: &str, width: usize) {
    for ch in word.chars() {
        let ch_w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if *cur_w > 0 && *cur_w + ch_w > width {
            out.push(std::mem::take(cur));
            *cur_w = 0;
        }
        cur.push(ch);
        *cur_w += ch_w;
    }
}

/// Truncate an owned string in place when needed, avoiding a second allocation for the common
/// already-fitting path.
pub fn truncate_owned_to_width(mut s: String, max: usize) -> String {
    if UnicodeWidthStr::width(s.as_str()) <= max {
        return s;
    }
    let mut w = 0usize;
    let mut end = 0usize;
    for (idx, ch) in s.char_indices() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if w + cw > max {
            break;
        }
        w += cw;
        end = idx + ch.len_utf8();
    }
    s.truncate(end);
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editable_window_keeps_a_middle_caret_and_both_sides_visible() {
        assert_eq!(
            editable_window("abcdefghij", 5, 6),
            EditableWindow {
                before: "cde".to_owned(),
                after: "fg".to_owned(),
            }
        );
        assert_eq!(
            editable_window("abcdef", 3, 7),
            EditableWindow {
                before: "abc".to_owned(),
                after: "def".to_owned(),
            }
        );
    }

    #[test]
    fn editable_window_uses_display_width_and_grapheme_boundaries() {
        let value = "가나e\u{301}다라";
        let cursor = "가나e\u{301}".len();
        let window = editable_window(value, cursor, 7);
        assert_eq!(window.before, "나e\u{301}");
        assert_eq!(window.after, "다");
        assert!(UnicodeWidthStr::width(window.before.as_str()) <= 4);
        assert!(UnicodeWidthStr::width(window.after.as_str()) <= 2);

        let family = "👨‍👩‍👧‍👦!";
        let window = editable_window(family, "👨‍👩‍👧‍👦".len(), 4);
        assert_eq!(window.before, "👨‍👩‍👧‍👦");
        assert_eq!(window.after, "!");
    }

    #[test]
    fn editable_window_reserves_the_only_cell_for_the_caret() {
        assert_eq!(
            editable_window("abc", 2, 1),
            EditableWindow {
                before: String::new(),
                after: String::new(),
            }
        );
    }

    #[test]
    fn masked_window_never_exposes_secret_graphemes() {
        let window = masked_editable_window("a\u{301}비밀", "a\u{301}비".len(), 5);
        assert_eq!(window.before, "••");
        assert_eq!(window.after, "•");
        assert!(!window.before.contains('비'));
    }

    #[test]
    fn pad_measures_display_width_not_scalar_count() {
        // Two Korean syllables are 4 display cells, not 2 — padding to 10 must add 6 spaces so the
        // result is exactly 10 cells wide (a `{:<10}` would wrongly add 8 and overrun by 2).
        let p = pad_to_width("가나", 10);
        assert!(p.starts_with("가나"));
        assert_eq!(UnicodeWidthStr::width(p.as_str()), 10);
        // ASCII keeps the obvious behaviour, and an already-wide string is returned unchanged.
        assert_eq!(pad_to_width("ab", 5), "ab   ");
        assert_eq!(pad_to_width("hello", 3), "hello");
    }

    #[test]
    fn truncate_never_splits_a_wide_char() {
        // "가나다" is 6 cells; capped at 5 only "가나" (4) fits — the third would overrun to 6.
        assert_eq!(truncate_to_width("가나다", 5), "가나");
        assert_eq!(truncate_to_width("가나다", 6), "가나다");
        assert_eq!(truncate_to_width("abcdef", 3), "abc");
        assert_eq!(truncate_to_width("", 4), "");
    }

    #[test]
    fn tail_window_keeps_the_editable_cjk_suffix_visible() {
        assert_eq!(tail_to_width("abcdef", 4), "…def");
        assert_eq!(tail_to_width("가나다라마", 5), "…라마");
        assert_eq!(
            UnicodeWidthStr::width(tail_to_width("가나다라마", 4).as_str()),
            3
        );
        assert_eq!(tail_to_width("anything", 0), "");
    }

    #[test]
    fn truncate_owned_reuses_fitting_string() {
        assert_eq!(truncate_owned_to_width("abcdef".to_owned(), 6), "abcdef");
        assert_eq!(truncate_owned_to_width("가나다".to_owned(), 5), "가나");
    }

    #[test]
    fn wrap_breaks_on_word_boundaries() {
        // Wraps between words, packing as many as fit in `width`.
        assert_eq!(
            wrap_to_width("the quick brown fox", 10),
            vec!["the quick", "brown fox"]
        );
        // No line ever exceeds the width.
        for line in wrap_to_width("the quick brown fox jumps over", 12) {
            assert!(UnicodeWidthStr::width(line.as_str()) <= 12);
        }
    }

    #[test]
    fn wrap_hard_breaks_an_overlong_word() {
        // A single word longer than the width splits at the cell boundary.
        assert_eq!(
            wrap_to_width("supercalifragilistic", 5),
            vec!["super", "calif", "ragil", "istic"]
        );
    }

    #[test]
    fn wrap_measures_display_width_for_cjk() {
        // Each Korean syllable is 2 cells, so 3 fit in a width of 6, not 5.
        let wrapped = wrap_to_width("가나다라마", 6);
        assert_eq!(wrapped, vec!["가나다", "라마"]);
        for line in &wrapped {
            assert!(UnicodeWidthStr::width(line.as_str()) <= 6);
        }
        // Empty input still yields one (empty) line.
        assert_eq!(wrap_to_width("", 8), vec![String::new()]);
    }
}
