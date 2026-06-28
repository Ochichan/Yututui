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
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use crate::app::{App, Mode, MouseTarget};
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

/// The screen nav bar shown at the top of every view: Player · Search · Library ·
/// Settings · AI. The active screen is highlighted (selection colors); the rest are muted.
/// Each item is a click target that switches screens. Centered, no box chrome — it reads
/// like a tab strip, consistent with the in-line "text is the button" controls elsewhere.
pub fn render_nav(frame: &mut Frame, app: &App, area: Rect) {
    const ITEMS: [(Mode, &str); 5] = [
        (Mode::Player, "Player"),
        (Mode::Search, "Search"),
        (Mode::Library, "Library"),
        (Mode::Settings, "Settings"),
        (Mode::Ai, "AI"),
    ];
    const GAP: &str = "  ";

    let labels: Vec<String> = ITEMS.iter().map(|(_, l)| format!(" {l} ")).collect();
    let total: u16 = labels.iter().map(|s| text_width(s)).sum::<u16>()
        + text_width(GAP) * (ITEMS.len() as u16 - 1);
    // Same centering math ratatui uses, so the hit rects line up with the rendered text.
    let mut x = area.x + area.width.saturating_sub(total) / 2;

    let active = Style::default()
        .fg(app.theme.color(R::SelectionFg))
        .bg(app.theme.color(R::SelectionBg))
        .add_modifier(Modifier::BOLD);
    let muted = app.theme.style(R::TextMuted);

    let mut spans = Vec::with_capacity(ITEMS.len() * 2);
    for (i, ((mode, _), label)) in ITEMS.iter().zip(&labels).enumerate() {
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
    frame.render_widget(Paragraph::new(Line::from(spans).alignment(Alignment::Center)), area);
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

/// The footer hint (e.g. "?  keybindings"): a single dim, clickable label — reads as a
/// status-bar hint, not a button. Shared by every screen's footer.
pub fn render_help_button(frame: &mut Frame, app: &App, area: Rect) {
    let label = app.help_footer();
    let segs = [Seg::button(MouseTarget::Global(Action::ToggleHelp), label.as_str())];
    let hint = app.theme.style(R::TextMuted);
    render_segments(frame, app, area, &segs, hint, hint, Alignment::Center);
}
