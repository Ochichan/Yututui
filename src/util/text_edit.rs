//! Shared grapheme-safe text editing for the TUI input buffers.

use unicode_normalization::char::is_combining_mark;
use unicode_segmentation::UnicodeSegmentation;

/// A caret stored as a UTF-8 byte offset.
///
/// Reducers keep this alongside their `String`. Every operation normalizes a stale or invalid
/// offset to the preceding extended-grapheme boundary before using it, so programmatic buffer
/// replacement and Unicode input can never make slicing panic or split a visible character.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TextCursor {
    byte: usize,
}

impl TextCursor {
    /// A cursor at the end of an existing value (used when an editor opens in-place).
    pub fn at_end(text: &str) -> Self {
        Self { byte: text.len() }
    }

    /// A cursor at an explicit byte offset. The offset is normalized when it is read or edited.
    pub fn from_byte_index(byte: usize) -> Self {
        Self { byte }
    }

    /// The safe UTF-8 byte offset for `text`, clamped to the preceding grapheme boundary.
    ///
    /// This read-only form is also the rendering contract: callers never need mutable access to
    /// repair a stale cursor before splitting the displayed string.
    pub fn byte_index(self, text: &str) -> usize {
        preceding_grapheme_boundary(text, self.byte)
    }

    /// Move to the start of the buffer.
    pub fn move_to_start(&mut self) -> bool {
        let changed = self.byte != 0;
        self.byte = 0;
        changed
    }

    /// Move to the end of the buffer.
    pub fn move_to_end(&mut self, text: &str) -> bool {
        let end = text.len();
        let changed = self.byte_index(text) != end;
        self.byte = end;
        changed
    }

    /// Move one extended grapheme to the left.
    pub fn move_left(&mut self, text: &str) -> bool {
        let current = self.byte_index(text);
        let next = previous_grapheme_start(text, current);
        self.byte = next;
        next != current
    }

    /// Move one extended grapheme to the right.
    pub fn move_right(&mut self, text: &str) -> bool {
        let current = self.byte_index(text);
        let next = next_grapheme_end(text, current);
        self.byte = next;
        next != current
    }

    /// Move left over adjacent whitespace, then over the preceding word or symbol run.
    pub fn move_word_left(&mut self, text: &str) -> bool {
        let current = self.byte_index(text);
        let next = previous_word_start(text, current);
        self.byte = next;
        next != current
    }

    /// Move to the start of the next word/symbol run, skipping whitespace between runs.
    pub fn move_word_right(&mut self, text: &str) -> bool {
        let current = self.byte_index(text);
        let next = next_word_start(text, current);
        self.byte = next;
        next != current
    }

    /// Insert a scalar at the caret and leave the caret on a valid grapheme boundary after it.
    pub fn insert_char(&mut self, buffer: &mut String, ch: char) {
        let current = self.byte_index(buffer);
        buffer.insert(current, ch);
        let intended = current + ch.len_utf8();
        // A joiner can merge the inserted scalar with the grapheme on its right. In that rare
        // case the first safe boundary is after the newly formed grapheme, never inside it.
        self.byte = following_grapheme_boundary(buffer, intended);
    }

    /// Delete the extended grapheme immediately before the caret.
    pub fn delete_previous_grapheme(&mut self, buffer: &mut String) -> bool {
        let current = self.byte_index(buffer);
        let start = previous_grapheme_start(buffer, current);
        self.byte = start;
        if start == current {
            return false;
        }
        buffer.replace_range(start..current, "");
        true
    }

    /// Delete adjacent whitespace plus the preceding word or symbol run.
    pub fn delete_previous_word(&mut self, buffer: &mut String) -> bool {
        let current = self.byte_index(buffer);
        let start = previous_word_start(buffer, current);
        self.byte = start;
        if start == current {
            return false;
        }
        buffer.replace_range(start..current, "");
        true
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Segment {
    Word,
    Symbol,
}

fn grapheme_segment(grapheme: &str) -> Option<Segment> {
    let ch = grapheme.chars().next()?;
    if ch.is_whitespace() {
        None
    } else if ch.is_alphanumeric() || ch == '_' || is_combining_mark(ch) {
        Some(Segment::Word)
    } else {
        Some(Segment::Symbol)
    }
}

fn preceding_grapheme_boundary(text: &str, requested: usize) -> usize {
    let requested = requested.min(text.len());
    if requested == text.len() {
        return requested;
    }
    text.grapheme_indices(true)
        .map(|(index, _)| index)
        .take_while(|index| *index <= requested)
        .last()
        .unwrap_or(0)
}

fn following_grapheme_boundary(text: &str, requested: usize) -> usize {
    let requested = requested.min(text.len());
    if requested == 0 || requested == text.len() {
        return requested;
    }
    text.grapheme_indices(true)
        .map(|(index, _)| index)
        .find(|index| *index >= requested)
        .unwrap_or(text.len())
}

fn previous_grapheme_start(text: &str, current: usize) -> usize {
    if current == 0 {
        return 0;
    }
    text[..current]
        .grapheme_indices(true)
        .next_back()
        .map_or(0, |(index, _)| index)
}

fn next_grapheme_end(text: &str, current: usize) -> usize {
    if current >= text.len() {
        return text.len();
    }
    text[current..]
        .graphemes(true)
        .next()
        .map_or(text.len(), |grapheme| current + grapheme.len())
}

fn previous_word_start(text: &str, current: usize) -> usize {
    let mut cursor = current;
    while cursor > 0 {
        let previous = previous_grapheme_start(text, cursor);
        if grapheme_segment(&text[previous..cursor]).is_some() {
            break;
        }
        cursor = previous;
    }
    if cursor == 0 {
        return 0;
    }
    let previous = previous_grapheme_start(text, cursor);
    let wanted = grapheme_segment(&text[previous..cursor]);
    cursor = previous;
    while cursor > 0 {
        let previous = previous_grapheme_start(text, cursor);
        if grapheme_segment(&text[previous..cursor]) != wanted {
            break;
        }
        cursor = previous;
    }
    cursor
}

fn next_word_start(text: &str, current: usize) -> usize {
    let mut cursor = current;
    if cursor >= text.len() {
        return text.len();
    }
    let end = next_grapheme_end(text, cursor);
    let wanted = grapheme_segment(&text[cursor..end]);
    if wanted.is_none() {
        while cursor < text.len() {
            let end = next_grapheme_end(text, cursor);
            if grapheme_segment(&text[cursor..end]).is_some() {
                break;
            }
            cursor = end;
        }
        return cursor;
    }
    while cursor < text.len() {
        let end = next_grapheme_end(text, cursor);
        if grapheme_segment(&text[cursor..end]) != wanted {
            break;
        }
        cursor = end;
    }
    while cursor < text.len() {
        let end = next_grapheme_end(text, cursor);
        if grapheme_segment(&text[cursor..end]).is_some() {
            break;
        }
        cursor = end;
    }
    cursor
}

/// Compatibility wrapper for callers that do not store a cursor: delete from the buffer's end.
pub fn delete_previous_word(buffer: &mut String) -> bool {
    TextCursor::at_end(buffer).delete_previous_word(buffer)
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

    #[test]
    fn moves_and_edits_at_grapheme_boundaries() {
        let mut value = "a\u{301}🙂‍🚀한".to_owned();
        let mut cursor = TextCursor::at_end(&value);
        assert!(cursor.move_left(&value));
        assert_eq!(cursor.byte_index(&value), "a\u{301}🙂‍🚀".len());
        assert!(cursor.move_left(&value));
        assert_eq!(cursor.byte_index(&value), "a\u{301}".len());
        assert!(cursor.delete_previous_grapheme(&mut value));
        assert_eq!(value, "🙂‍🚀한");
        assert_eq!(cursor.byte_index(&value), 0);
        cursor.insert_char(&mut value, '日');
        assert_eq!(value, "日🙂‍🚀한");
        assert_eq!(cursor.byte_index(&value), "日".len());
    }

    #[test]
    fn word_movement_distinguishes_words_symbols_and_spaces() {
        let value = "foo/bar  baz";
        let mut cursor = TextCursor::default();
        assert!(cursor.move_word_right(value));
        assert_eq!(cursor.byte_index(value), 3); // slash run
        assert!(cursor.move_word_right(value));
        assert_eq!(cursor.byte_index(value), 4); // bar run
        assert!(cursor.move_word_right(value));
        assert_eq!(cursor.byte_index(value), 9); // baz, after spaces
        assert!(cursor.move_word_left(value));
        assert_eq!(cursor.byte_index(value), 4);
        assert!(cursor.move_word_left(value));
        assert_eq!(cursor.byte_index(value), 3);
    }

    #[test]
    fn symbol_graphemes_with_variation_selectors_or_joiners_stay_symbols() {
        for symbol in ["❤️", "👩‍🚀"] {
            let value = format!("{symbol}go");
            let mut cursor = TextCursor::default();
            assert!(cursor.move_word_right(&value));
            assert_eq!(cursor.byte_index(&value), symbol.len());
            assert!(cursor.move_word_right(&value));
            assert_eq!(cursor.byte_index(&value), value.len());
        }
    }

    #[test]
    fn middle_word_delete_preserves_the_suffix() {
        let mut value = "one two three".to_owned();
        let mut cursor = TextCursor::from_byte_index("one two".len());
        assert!(cursor.delete_previous_word(&mut value));
        assert_eq!(value, "one  three");
        assert_eq!(cursor.byte_index(&value), "one ".len());
    }

    #[test]
    fn stale_offsets_clamp_to_the_previous_grapheme_boundary() {
        let value = "a\u{301}🙂";
        assert_eq!(TextCursor::from_byte_index(1).byte_index(value), 0);
        assert_eq!(
            TextCursor::from_byte_index(usize::MAX).byte_index(value),
            value.len()
        );
    }
}
