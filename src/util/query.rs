pub const MAX_SEARCH_QUERY_BYTES: usize = 2048;
pub const MAX_FILTER_QUERY_BYTES: usize = 512;
pub const MAX_QUERY_PREVIEW_CHARS: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryRejectReason {
    Empty,
    TooLong { max: usize },
    ForbiddenChar,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryLogPreview {
    pub bytes: usize,
    pub chars: usize,
    pub preview: String,
    pub truncated: bool,
}

pub fn try_push_query_char(
    buf: &mut String,
    ch: char,
    max_bytes: usize,
) -> Result<(), QueryRejectReason> {
    if forbidden_query_char(ch) {
        return Err(QueryRejectReason::ForbiddenChar);
    }
    if buf.len().saturating_add(ch.len_utf8()) > max_bytes {
        return Err(QueryRejectReason::TooLong { max: max_bytes });
    }
    buf.push(ch);
    Ok(())
}

pub fn sanitize_query_for_submit(raw: &str, max_bytes: usize) -> Result<String, QueryRejectReason> {
    let query = raw.trim();
    if query.is_empty() {
        return Err(QueryRejectReason::Empty);
    }
    if query.len() > max_bytes {
        return Err(QueryRejectReason::TooLong { max: max_bytes });
    }
    if query.chars().any(forbidden_query_char) {
        return Err(QueryRejectReason::ForbiddenChar);
    }
    Ok(query.to_owned())
}

pub fn query_log_preview(query: &str) -> QueryLogPreview {
    let chars = query.chars().count();
    let mut preview = String::new();
    for ch in query.chars().filter(|ch| !forbidden_query_char(*ch)) {
        if preview.chars().count() >= MAX_QUERY_PREVIEW_CHARS {
            break;
        }
        preview.push(ch);
    }
    QueryLogPreview {
        bytes: query.len(),
        chars,
        preview,
        truncated: chars > MAX_QUERY_PREVIEW_CHARS,
    }
}

pub fn forbidden_query_char(ch: char) -> bool {
    ch == '\0' || ch.is_control() || is_bidi_control(ch) || is_zero_width_control(ch)
}

fn is_bidi_control(ch: char) -> bool {
    matches!(
        ch,
        '\u{061c}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
    )
}

fn is_zero_width_control(ch: char) -> bool {
    matches!(ch, '\u{200b}'..='\u{200d}' | '\u{2060}' | '\u{feff}')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_query_char_caps_by_utf8_bytes() {
        let mut query = "a".repeat(MAX_FILTER_QUERY_BYTES - 1);
        try_push_query_char(&mut query, 'b', MAX_FILTER_QUERY_BYTES).unwrap();
        assert_eq!(query.len(), MAX_FILTER_QUERY_BYTES);
        assert_eq!(
            try_push_query_char(&mut query, 'c', MAX_FILTER_QUERY_BYTES),
            Err(QueryRejectReason::TooLong {
                max: MAX_FILTER_QUERY_BYTES
            })
        );
    }

    #[test]
    fn submit_query_rejects_control_and_bidi_chars() {
        assert_eq!(
            sanitize_query_for_submit("abc\u{202e}def", MAX_SEARCH_QUERY_BYTES),
            Err(QueryRejectReason::ForbiddenChar)
        );
        assert_eq!(
            sanitize_query_for_submit("abc\ndef", MAX_SEARCH_QUERY_BYTES),
            Err(QueryRejectReason::ForbiddenChar)
        );
    }

    #[test]
    fn log_preview_keeps_lengths_and_safe_prefix() {
        let preview = query_log_preview(&format!("{}{}", "x".repeat(40), "\u{202e}"));
        assert_eq!(preview.bytes, 43);
        assert_eq!(preview.chars, 41);
        assert_eq!(preview.preview, "x".repeat(MAX_QUERY_PREVIEW_CHARS));
        assert!(preview.truncated);
    }
}
