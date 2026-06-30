//! Small display-width text helpers, shared across views. `format!`'s `{:<w}` / `{:>w}` and
//! `str::chars().count()` count Unicode scalar values, which misaligns two-cell East-Asian
//! characters (Korean, Japanese, Chinese). These measure with `unicode-width` instead, so columns
//! line up flush in every UI language.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

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
    fn truncate_owned_reuses_fitting_string() {
        assert_eq!(truncate_owned_to_width("abcdef".to_owned(), 6), "abcdef");
        assert_eq!(truncate_owned_to_width("가나다".to_owned(), 5), "가나");
    }
}
