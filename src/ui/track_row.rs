//! The body of a track row shared by Search, the filter popup, Artist, Library, and the queue:
//! the title, the artist in a quieter color, and the duration in its own right-aligned column.
//! Keeping the duration apart means a long title clips (with `…`) before the length does.

use ratatui::style::Style;
use ratatui::text::Span;
use unicode_width::UnicodeWidthStr;

use crate::ui::text::{pad_to_width, truncate_to_width};

/// Separator between the title and the artist.
pub const ARTIST_SEPARATOR: &str = " — ";
/// Cells kept between the body and the duration column.
const DURATION_GAP: usize = 2;
/// Below this many body cells the duration column is dropped, so the title stays readable.
const MIN_BODY: usize = 12;

/// `title — artist`, or the bare title when there is no artist (artist and playlist rows).
pub fn body_text(title: &str, artist: &str) -> String {
    if artist.is_empty() {
        title.to_owned()
    } else {
        format!("{title}{ARTIST_SEPARATOR}{artist}")
    }
}

/// Cells the title/artist body gets in a row `width` cells wide, after the duration column.
/// Callers that marquee the cursor row pass this as the marquee width.
pub fn body_width(duration: &str, width: usize) -> usize {
    let column = duration_column(duration, width);
    width.saturating_sub(column)
}

fn duration_column(duration: &str, width: usize) -> usize {
    if duration.is_empty() {
        return 0;
    }
    let column = UnicodeWidthStr::width(duration) + DURATION_GAP;
    if width < column + MIN_BODY { 0 } else { column }
}

/// Spans filling exactly `width` cells: the body, padding, then the duration. `marquee` is the
/// already-scrolled body text for a clipped cursor row; it replaces the title/artist split
/// because its visible window can start anywhere. `title_style` colors the title (and the
/// marquee), `muted` the artist and duration; pass the same style twice on a highlighted row.
pub fn spans(
    title: &str,
    artist: &str,
    duration: &str,
    width: usize,
    marquee: Option<String>,
    title_style: Style,
    muted: Style,
) -> Vec<Span<'static>> {
    let column = duration_column(duration, width);
    let body = width.saturating_sub(column);
    let mut out = Vec::with_capacity(4);
    match marquee {
        Some(text) => out.push(Span::styled(
            pad_to_width(&truncate_to_width(&text, body), body),
            title_style,
        )),
        None => {
            let full = body_text(title, artist);
            let clipped = if UnicodeWidthStr::width(full.as_str()) > body && body > 0 {
                format!("{}…", truncate_to_width(&full, body - 1))
            } else {
                full
            };
            // The title keeps its color up to where it ends in the (possibly clipped) text.
            let split = clipped
                .char_indices()
                .map(|(i, _)| i)
                .chain(std::iter::once(clipped.len()))
                .take_while(|&i| i <= title.len())
                .last()
                .unwrap_or(0);
            let (head, tail) = clipped.split_at(split.min(clipped.len()));
            let used = UnicodeWidthStr::width(clipped.as_str());
            out.push(Span::styled(head.to_owned(), title_style));
            if !tail.is_empty() {
                out.push(Span::styled(tail.to_owned(), muted));
            }
            out.push(Span::raw(" ".repeat(body.saturating_sub(used))));
        }
    }
    if column > 0 {
        out.push(Span::styled(
            format!("{}{duration}", " ".repeat(DURATION_GAP)),
            muted,
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(spans: &[Span<'_>]) -> String {
        spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn duration_stays_whole_while_the_body_clips() {
        let spans = spans(
            "A very long title indeed",
            "Artist",
            "3:05",
            24,
            None,
            Style::default(),
            Style::default(),
        );
        let line = text(&spans);
        assert_eq!(UnicodeWidthStr::width(line.as_str()), 24);
        assert!(line.ends_with("  3:05"), "{line}");
        assert!(line.contains('…'), "{line}");
    }

    #[test]
    fn fitting_rows_split_title_and_artist_and_pad_to_the_column() {
        let spans = spans(
            "Lovely",
            "Billie",
            "0:10",
            30,
            None,
            Style::default(),
            Style::default(),
        );
        assert_eq!(spans[0].content, "Lovely");
        assert_eq!(spans[1].content, " — Billie");
        assert_eq!(UnicodeWidthStr::width(text(&spans).as_str()), 30);
        assert!(text(&spans).ends_with("  0:10"));
    }

    #[test]
    fn narrow_rows_drop_the_duration_before_the_title() {
        let line = text(&spans(
            "한국어 제목",
            "가수",
            "12:34",
            14,
            None,
            Style::default(),
            Style::default(),
        ));
        assert!(!line.contains("12:34"), "{line}");
        assert!(UnicodeWidthStr::width(line.as_str()) <= 14);
    }
}
