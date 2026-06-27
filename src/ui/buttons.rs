//! Click-target rendering shared across views.
//!
//! These are deliberately *not* drawn as boxed GUI buttons (no fill, no brackets) —
//! that chrome looks out of place in a terminal. Instead they're plain colored glyphs
//! and text that read as part of the player, with invisible hit rects published for
//! the mouse. Wide media glyphs make `chars().count()` the wrong width measure, so we
//! use `unicode-width` (the same crate ratatui lays out with) to keep rects aligned.

use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use crate::app::{App, MouseTarget};
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

/// The footer hint (e.g. "?  keybindings"): a single dim, clickable label — reads as a
/// status-bar hint, not a button. Shared by every screen's footer.
pub fn render_help_button(frame: &mut Frame, app: &App, area: Rect, alignment: Alignment) {
    let label = app.help_footer();
    let segs = [Seg::button(MouseTarget::Global(Action::ToggleHelp), label.as_str())];
    let hint = app.theme.style(R::TextMuted);
    render_segments(frame, app, area, &segs, hint, hint, alignment);
}
