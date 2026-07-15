//! Shared, cursor-at-end text editing helpers for the TUI input buffers.

use unicode_normalization::char::is_combining_mark;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Segment {
    Word,
    Symbol,
}

fn segment(ch: char) -> Option<Segment> {
    if ch.is_whitespace() {
        None
    } else if ch.is_alphanumeric() || ch == '_' || is_combining_mark(ch) {
        Some(Segment::Word)
    } else {
        Some(Segment::Symbol)
    }
}

/// Delete trailing whitespace plus the preceding word or symbol run.
///
/// TUI editors currently keep their caret at the end, so returning whether the buffer changed
/// is enough for each reducer to trigger its own filtering, validation, and redraw side effects.
pub fn delete_previous_word(buffer: &mut String) -> bool {
    let original_len = buffer.len();
    let mut end = original_len;

    while let Some((idx, ch)) = buffer[..end].char_indices().next_back() {
        if !ch.is_whitespace() {
            break;
        }
        end = idx;
    }

    let Some((_, last)) = buffer[..end].char_indices().next_back() else {
        buffer.clear();
        return original_len != 0;
    };
    let wanted = segment(last).expect("non-whitespace segment");
    let mut start = end;
    for (idx, ch) in buffer[..end].char_indices().rev() {
        if segment(ch) != Some(wanted) {
            break;
        }
        start = idx;
    }

    buffer.truncate(start);
    buffer.len() != original_len
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deleted(input: &str) -> String {
        let mut value = input.to_owned();
        delete_previous_word(&mut value);
        value
    }

    #[test]
    fn deletes_word_and_trailing_space_runs() {
        assert_eq!(deleted("lofi hip hop"), "lofi hip ");
        assert_eq!(deleted("lofi hip   "), "lofi ");
        assert_eq!(deleted("one"), "");
        assert_eq!(deleted("   "), "");
        assert_eq!(deleted(""), "");
    }

    #[test]
    fn distinguishes_words_from_symbols() {
        assert_eq!(deleted("foo/bar"), "foo/");
        assert_eq!(deleted("foo/"), "foo");
        assert_eq!(deleted("snake_case"), "");
        assert_eq!(deleted("🙂🙂"), "");
    }

    #[test]
    fn keeps_unicode_boundaries_valid() {
        assert_eq!(deleted("비 오는 밤"), "비 오는 ");
        assert_eq!(deleted("日本語 検索"), "日本語 ");
        assert_eq!(deleted("cafe\u{301}"), "");
    }

    #[test]
    fn reports_whether_the_buffer_changed() {
        let mut empty = String::new();
        assert!(!delete_previous_word(&mut empty));
        let mut value = "word".to_owned();
        assert!(delete_previous_word(&mut value));
    }
}
