//! The body of a track row shared by Search, the filter popup, Artist, Library, and the queue:
//! the title, the artist in a quieter color, and the duration in its own right-aligned column.
//! Keeping the duration apart means a long title clips (with `…`) before the length does.

use ratatui::style::Style;
use ratatui::text::Span;
use unicode_width::UnicodeWidthStr;

use crate::ui::text::{pad_to_width, spaces, truncate_str_to_width, truncate_to_width};

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

/// Render one track-row body directly into `buf` at `(x, y)` filling `width` cells without
/// allocating a `Vec<Span>` or intermediate `String`s on the common non-marquee path.
/// Caller should have already filled the row background with `base`.
#[allow(clippy::too_many_arguments)]
pub fn render_to_buf(
    buf: &mut ratatui::buffer::Buffer,
    mut x: u16,
    y: u16,
    width: usize,
    title: &str,
    artist: &str,
    duration: &str,
    marquee: Option<&str>,
    base: Style,
    muted: Style,
) {
    let muted_style = base.patch(muted);
    let column = duration_column(duration, width);
    let body = width.saturating_sub(column);
    let start_x = x;
    let put = |buf: &mut ratatui::buffer::Buffer,
               cx: &mut u16,
               s: &str,
               max_cells: usize,
               style: Style| {
        if !s.is_empty() && max_cells > 0 {
            let (nx, _) = buf.set_stringn(*cx, y, s, max_cells, style);
            *cx = nx;
        }
    };
    match marquee {
        Some(text) => {
            let (head, head_w) = truncate_str_to_width(text, body);
            put(buf, &mut x, head, body, base);
            let pad = body.saturating_sub(head_w);
            if pad > 0 {
                put(buf, &mut x, spaces(pad).as_ref(), pad, base);
            }
        }
        None => {
            let title_w = UnicodeWidthStr::width(title);
            let sep_w = if artist.is_empty() {
                0
            } else {
                UnicodeWidthStr::width(ARTIST_SEPARATOR)
            };
            let artist_w = if artist.is_empty() {
                0
            } else {
                UnicodeWidthStr::width(artist)
            };
            let full_w = title_w + sep_w + artist_w;
            let used = if full_w <= body {
                put(buf, &mut x, title, title_w, base);
                if !artist.is_empty() {
                    put(buf, &mut x, ARTIST_SEPARATOR, sep_w, muted_style);
                    put(buf, &mut x, artist, artist_w, muted_style);
                }
                full_w
            } else if body == 0 {
                0
            } else {
                let budget = body - 1;
                if title_w > budget {
                    let (head, head_w) = truncate_str_to_width(title, budget);
                    let ellipsis_style = if head.len() + '…'.len_utf8() <= title.len() {
                        base
                    } else {
                        muted_style
                    };
                    put(buf, &mut x, head, head_w, base);
                    put(buf, &mut x, "…", 1, ellipsis_style);
                    head_w + 1
                } else {
                    put(buf, &mut x, title, title_w, base);
                    let rem = budget - title_w;
                    let used_after_title = if !artist.is_empty() && rem > 0 {
                        if sep_w > rem {
                            let (sep_part, sep_part_w) =
                                truncate_str_to_width(ARTIST_SEPARATOR, rem);
                            put(buf, &mut x, sep_part, sep_part_w, muted_style);
                            sep_part_w
                        } else {
                            put(buf, &mut x, ARTIST_SEPARATOR, sep_w, muted_style);
                            let (art_part, art_part_w) = truncate_str_to_width(artist, rem - sep_w);
                            put(buf, &mut x, art_part, art_part_w, muted_style);
                            sep_w + art_part_w
                        }
                    } else {
                        0
                    };
                    put(buf, &mut x, "…", 1, muted_style);
                    title_w + used_after_title + 1
                }
            };
            let pad = body.saturating_sub(used);
            if pad > 0 {
                put(buf, &mut x, spaces(pad).as_ref(), pad, base);
            }
        }
    }
    if column > 0 {
        // Align duration flush to `start_x + body` even if wide glyphs stopped short.
        let mut dur_x = start_x.saturating_add(body as u16);
        put(
            buf,
            &mut dur_x,
            spaces(DURATION_GAP).as_ref(),
            DURATION_GAP,
            muted_style,
        );
        put(
            buf,
            &mut dur_x,
            duration,
            column.saturating_sub(DURATION_GAP),
            muted_style,
        );
    }
}

/// Push spans filling exactly `width` cells into `out`, borrowing `title`, `artist`, and
/// `duration` directly so non-marquee rows perform zero heap allocations.
#[allow(clippy::too_many_arguments)]
pub fn push_spans<'a>(
    out: &mut Vec<Span<'a>>,
    title: &'a str,
    artist: &'a str,
    duration: &'a str,
    width: usize,
    marquee: Option<String>,
    title_style: Style,
    muted: Style,
) {
    let column = duration_column(duration, width);
    let body = width.saturating_sub(column);
    match marquee {
        Some(text) => out.push(Span::styled(
            pad_to_width(&truncate_to_width(&text, body), body),
            title_style,
        )),
        None => {
            let title_w = UnicodeWidthStr::width(title);
            let sep_w = if artist.is_empty() {
                0
            } else {
                UnicodeWidthStr::width(ARTIST_SEPARATOR)
            };
            let artist_w = if artist.is_empty() {
                0
            } else {
                UnicodeWidthStr::width(artist)
            };
            let full_w = title_w + sep_w + artist_w;
            let used = if full_w <= body {
                out.push(Span::styled(title, title_style));
                if !artist.is_empty() {
                    out.push(Span::styled(ARTIST_SEPARATOR, muted));
                    out.push(Span::styled(artist, muted));
                }
                full_w
            } else if body == 0 {
                0
            } else {
                let budget = body - 1;
                if title_w > budget {
                    let (head, head_w) = truncate_str_to_width(title, budget);
                    // Preserve the legacy style boundary when `…` falls past `title.len()`.
                    let ellipsis_style = if head.len() + '…'.len_utf8() <= title.len() {
                        title_style
                    } else {
                        muted
                    };
                    out.push(Span::styled(head, title_style));
                    out.push(Span::styled("…", ellipsis_style));
                    head_w + 1
                } else {
                    out.push(Span::styled(title, title_style));
                    let rem = budget - title_w;
                    let used_after_title = if !artist.is_empty() && rem > 0 {
                        if sep_w > rem {
                            let (sep_part, sep_part_w) =
                                truncate_str_to_width(ARTIST_SEPARATOR, rem);
                            if !sep_part.is_empty() {
                                out.push(Span::styled(sep_part, muted));
                            }
                            sep_part_w
                        } else {
                            out.push(Span::styled(ARTIST_SEPARATOR, muted));
                            let (art_part, art_part_w) = truncate_str_to_width(artist, rem - sep_w);
                            if !art_part.is_empty() {
                                out.push(Span::styled(art_part, muted));
                            }
                            sep_w + art_part_w
                        }
                    } else {
                        0
                    };
                    out.push(Span::styled("…", muted));
                    title_w + used_after_title + 1
                }
            };
            let pad = body.saturating_sub(used);
            if pad > 0 {
                out.push(Span::raw(spaces(pad)));
            }
        }
    }
    if column > 0 {
        out.push(Span::styled(spaces(DURATION_GAP), muted));
        out.push(Span::styled(duration, muted));
    }
}

/// Spans filling exactly `width` cells: the body, padding, then the duration. `marquee` is the
/// already-scrolled body text for a clipped cursor row; it replaces the title/artist split
/// because its visible window can start anywhere. `title_style` colors the title (and the
/// marquee), `muted` the artist and duration; pass the same style twice on a highlighted row.
#[cfg(test)]
pub fn spans(
    title: &str,
    artist: &str,
    duration: &str,
    width: usize,
    marquee: Option<String>,
    title_style: Style,
    muted: Style,
) -> Vec<Span<'static>> {
    let mut borrowed = Vec::with_capacity(6);
    push_spans(
        &mut borrowed,
        title,
        artist,
        duration,
        width,
        marquee,
        title_style,
        muted,
    );
    borrowed
        .into_iter()
        .map(|s| Span::styled(s.content.into_owned(), s.style))
        .collect()
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
        assert_eq!(spans[1].content, " — ");
        assert_eq!(spans[2].content, "Billie");
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
