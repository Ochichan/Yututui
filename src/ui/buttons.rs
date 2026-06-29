//! Click-target rendering shared across views.
//!
//! These are deliberately *not* drawn as boxed GUI buttons (no fill, no brackets) —
//! that chrome looks out of place in a terminal. Instead they're plain colored glyphs
//! and text that read as part of the player, with invisible hit rects published for
//! the mouse. Wide media glyphs make `chars().count()` the wrong width measure, so we
//! use `unicode-width` (the same crate ratatui lays out with) to keep rects aligned.

use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState};
use unicode_width::UnicodeWidthStr;

use crate::app::{App, Mode, MouseTarget};
use crate::t;

/// The nav-bar label for a screen, in the active UI language. (`AI` is a proper noun, kept
/// as-is in both languages.)
fn nav_label(mode: Mode) -> &'static str {
    match mode {
        Mode::Player => t!("Player", "플레이어"),
        Mode::Search => t!("Search", "검색"),
        Mode::Library => t!("Library", "라이브러리"),
        Mode::Settings => t!("Settings", "설정"),
        Mode::Ai => t!("AI", "AI"),
    }
}
use crate::keymap::Action;
use crate::theme::ThemeRole as R;

/// One piece of a control strip: either a clickable control (`target` set) or static
/// label/spacing. Clickable segments get the button style and a published hit rect;
/// the rest get the label style.
pub struct Seg<'a> {
    pub target: Option<MouseTarget>,
    pub text: &'a str,
}

impl<'a> Seg<'a> {
    pub fn button(target: MouseTarget, text: &'a str) -> Self {
        Self { target: Some(target), text }
    }

    pub fn label(text: &'a str) -> Self {
        Self { target: None, text }
    }
}

/// Cell width of `s`, matching ratatui's own layout so a hand-computed hit rect lines
/// up with the rendered glyphs even for wide symbols (⏮ ⏯ ⏭).
pub fn text_width(s: &str) -> u16 {
    UnicodeWidthStr::width(s) as u16
}

/// Render a one-line control strip and register a hit rect for each clickable segment.
/// `alignment` positions the strip in `area`; the rects are computed with the same
/// width math ratatui uses to place the text, so clicks land on the right control.
pub fn render_segments(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    segments: &[Seg<'_>],
    button_style: Style,
    label_style: Style,
    alignment: Alignment,
) {
    let total: u16 = segments.iter().map(|s| text_width(s.text)).sum();
    let mut x = match alignment {
        Alignment::Center => area.x + area.width.saturating_sub(total) / 2,
        Alignment::Right => area.x + area.width.saturating_sub(total),
        Alignment::Left => area.x,
    };

    let mut spans = Vec::with_capacity(segments.len());
    for seg in segments {
        let width = text_width(seg.text);
        let style = if let Some(target) = seg.target {
            app.register_mouse_button(
                Rect { x, y: area.y, width, height: area.height.min(1) },
                target,
            );
            button_style
        } else {
            label_style
        };
        spans.push(Span::styled(seg.text, style));
        x = x.saturating_add(width);
    }

    frame.render_widget(Paragraph::new(Line::from(spans).alignment(alignment)), area);
}

/// The screen nav bar shown at the top of every view: `ytm-tui │ Player · Search ·
/// Library · Settings · AI`. The `ytm-tui` brand sits at the top-left, set off by a muted
/// `│`; the tabs follow. The active screen is highlighted (selection colors), the rest are
/// muted, and each tab is a click target that switches screens. Left-aligned, no box chrome
/// — it reads like a tab strip, consistent with the in-line "text is the button" controls.
pub fn render_nav(frame: &mut Frame, app: &App, area: Rect) {
    const ITEMS: [Mode; 5] =
        [Mode::Player, Mode::Search, Mode::Library, Mode::Settings, Mode::Ai];
    const GAP: &str = "  ";
    const BRAND: &str = "ytm-tui";
    const SEP: &str = " │ ";
    // Blank gutters framing the strip: `MARGIN` left of the brand (also nudges the whole bar
    // right, off the corner); `END_PAD` after the last tab. END_PAD plus that tab's own
    // trailing space matches MARGIN, so the gap before the border resumes looks symmetric.
    const MARGIN: &str = "  ";
    const END_PAD: &str = " ";

    let labels: Vec<String> = ITEMS.iter().map(|mode| format!(" {} ", nav_label(*mode))).collect();

    let brand = app.theme.style(R::Accent).add_modifier(Modifier::BOLD);
    let sep = app.theme.style(R::BorderMuted);
    let active = Style::default()
        .fg(app.theme.color(R::SelectionFg))
        .bg(app.theme.color(R::SelectionBg))
        .add_modifier(Modifier::BOLD);
    let muted = app.theme.style(R::TextMuted);

    // Left-aligned strip starting at the inner edge. Brand + separator are static labels;
    // each tab is clickable, so we walk `x` in step with the spans to keep hit rects on text.
    let mut spans = Vec::with_capacity(ITEMS.len() * 2 + 4);
    let mut x = area.x;

    // Left gutter doubles as the global animation toggle: a ✨ when animations are on, two blank
    // cells when off. Both are exactly 2 cells wide (`text_width("✨") == text_width("  ") == 2`),
    // so the brand and tabs never shift between states. The hit rect is published in both states,
    // so clicking the blank slot turns animations back on. Color emoji ignore SGR fg on many
    // terminals, so the on/off signal is the glyph itself (✨ vs blank), not its color.
    let spark = if app.animations().master { "✨" } else { MARGIN };
    app.register_mouse_button(
        Rect { x, y: area.y, width: text_width(MARGIN), height: area.height.min(1) },
        MouseTarget::Global(Action::ToggleAnimations),
    );
    spans.push(Span::styled(spark, brand));
    x = x.saturating_add(text_width(spark));

    // The brand doubles as a click target that opens the About card.
    app.register_mouse_button(
        Rect { x, y: area.y, width: text_width(BRAND), height: area.height.min(1) },
        MouseTarget::AboutTitle,
    );
    spans.push(Span::styled(BRAND, brand));
    x = x.saturating_add(text_width(BRAND));
    spans.push(Span::styled(SEP, sep));
    x = x.saturating_add(text_width(SEP));

    for (i, (mode, label)) in ITEMS.iter().zip(&labels).enumerate() {
        if i > 0 {
            spans.push(Span::styled(GAP, muted));
            x = x.saturating_add(text_width(GAP));
        }
        let w = text_width(label);
        app.register_mouse_button(
            Rect { x, y: area.y, width: w, height: area.height.min(1) },
            MouseTarget::Nav(*mode),
        );
        let style = if app.mode == *mode { active } else { muted };
        spans.push(Span::styled(label.clone(), style));
        x = x.saturating_add(w);
    }
    // Right gutter, then render into a rect sized to the text (not the full row) so when this
    // strip rides a border line the border keeps drawing in the cells past it.
    spans.push(Span::raw(END_PAD));
    x = x.saturating_add(text_width(END_PAD));
    let used = x.saturating_sub(area.x).min(area.width);
    let strip = Rect { width: used, ..area };
    frame.render_widget(Paragraph::new(Line::from(spans)), strip);
}

/// Register a `ListRow(i)` click target over each visible row of a ratatui `List`. Call
/// after `render_stateful_widget` with the list's `area`, the `ListState::offset()` it
/// produced, and the total item count — so a click maps to the right item even when the
/// list is scrolled. `index_of` maps a visible item position to the logical index the
/// reducer expects (identity for song lists; binding-index for the settings Keys tab).
pub fn register_list_rows(
    app: &App,
    area: Rect,
    offset: usize,
    count: usize,
    index_of: impl Fn(usize) -> Option<usize>,
) {
    for vis in 0..area.height {
        let item = offset + vis as usize;
        if item >= count {
            break;
        }
        if let Some(logical) = index_of(item) {
            app.register_mouse_button(
                Rect { x: area.x, y: area.y + vis, width: area.width, height: 1 },
                MouseTarget::ListRow(logical),
            );
        }
    }
}

/// Draw a vertical scrollbar on the right border of `list_area`, but only when the content
/// overflows the viewport. `position` is the scroll offset (the first visible row), so the
/// thumb tracks the viewport through the list and reaches both ends. A no-op when everything
/// fits.
///
/// `list_area` spans the bordered view's inner width, so `list_area.right()` is the block's
/// right border column — the scrollbar replaces that border segment and never clips a row.
pub fn render_list_scrollbar(
    frame: &mut Frame,
    app: &App,
    list_area: Rect,
    content_len: usize,
    position: usize,
    viewport: usize,
) {
    if content_len <= viewport || list_area.height == 0 {
        return;
    }
    let mut state = ScrollbarState::new(content_len)
        .position(position)
        .viewport_content_length(viewport);
    let bar = Rect { x: list_area.right(), y: list_area.y, width: 1, height: list_area.height };
    frame.render_stateful_widget(
        Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .track_symbol(Some("│"))
            .thumb_symbol("█")
            .track_style(app.theme.style(R::BorderPrimary))
            .thumb_style(app.theme.style(R::Accent)),
        bar,
        &mut state,
    );
}

/// The footer hint (e.g. "?  keybindings"): a single dim, clickable label — reads as a
/// status-bar hint, not a button. Shared by every screen's footer.
pub fn render_help_button(frame: &mut Frame, app: &App, area: Rect) {
    let label = app.help_footer();
    let segs = [Seg::button(MouseTarget::Global(Action::ToggleHelp), label.as_str())];
    let hint = app.theme.style(R::TextMuted);
    render_segments(frame, app, area, &segs, hint, hint, Alignment::Center);
}

#[cfg(test)]
mod tests {
    use super::text_width;

    #[test]
    fn sparkle_and_blank_nav_slot_are_both_two_cells() {
        // The ✨ animation toggle and its blank "off" state share one fixed 2-cell slot in
        // `render_nav`, so the brand and tabs never shift between on/off. This pins the layout
        // assumption against a future `unicode-width` table change.
        assert_eq!(text_width("✨"), 2);
        assert_eq!(text_width("  "), 2);
    }
}
