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

use crate::app::{App, Mode, MouseTarget, ScrollSurface};
use crate::t;

/// The nav-bar label for a screen, in the active UI language. (`DJ Gem` is a proper noun, kept
/// as-is in both languages.)
fn nav_label(mode: Mode, radio_mode: bool) -> &'static str {
    match mode {
        Mode::Player if radio_mode => t!("Radio ", "라 디 오"),
        Mode::Player => t!("Player", "플레이어"),
        Mode::Search => t!("Search", "검색"),
        Mode::Library => t!("Library", "라이브러리"),
        Mode::Settings => t!("Settings", "설정"),
        Mode::Ai => t!("DJ Gem", "DJ Gem"),
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
        Self {
            target: Some(target),
            text,
        }
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
    let mut widths = Vec::with_capacity(segments.len());
    let mut total = 0u16;
    for seg in segments {
        let width = text_width(seg.text);
        total = total.saturating_add(width);
        widths.push(width);
    }
    let mut x = match alignment {
        Alignment::Center => area.x + area.width.saturating_sub(total) / 2,
        Alignment::Right => area.x + area.width.saturating_sub(total),
        Alignment::Left => area.x,
    };

    let mut spans = Vec::with_capacity(segments.len());
    for (seg, width) in segments.iter().zip(widths) {
        let style = if let Some(target) = seg.target {
            app.register_mouse_button(
                Rect {
                    x,
                    y: area.y,
                    width,
                    height: area.height.min(1),
                },
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

/// Every screen in nav order. Radio mode swaps only the player tab's *label*
/// (see [`nav_label`]), not the set of screens.
const NAV_ITEMS: [Mode; 5] = [
    Mode::Player,
    Mode::Search,
    Mode::Library,
    Mode::Settings,
    Mode::Ai,
];

/// The screen nav bar shown at the top of every view: `ytm-tui │ Player · Search ·
/// Library · Settings · DJ Gem` (or `Radio` for the player tab in dedicated Radio mode).
/// The `ytm-tui` brand sits at the top-left, set off by a muted
/// `│`; the tabs follow. The active screen is highlighted (selection colors), the rest are
/// muted, and each tab is a click target that switches screens. Left-aligned, no box chrome
/// — it reads like a tab strip, consistent with the in-line "text is the button" controls.
///
/// When the full strip doesn't fit (a narrow terminal, or text zoom shrinking the virtual
/// grid), it degrades to a paged strip: `◀ ▶` arrows that switch to the previous / next
/// screen on click, around a window of tabs centered on the active one — so every screen
/// stays reachable by mouse no matter how little width remains.
pub fn render_nav(frame: &mut Frame, app: &App, area: Rect) {
    const GAP: &str = "  ";
    const BRAND: &str = "ytm-tui";
    const SEP: &str = " │ ";
    // Blank gutters framing the strip: `MARGIN` left of the brand (also nudges the whole bar
    // right, off the corner); `END_PAD` after the last tab. END_PAD plus that tab's own
    // trailing space matches MARGIN, so the gap before the border resumes looks symmetric.
    const MARGIN: &str = "  ";
    const END_PAD: &str = " ";

    let brand = app.theme.style(R::Accent).add_modifier(Modifier::BOLD);
    let sep = app.theme.style(R::BorderMuted);
    let active = Style::default()
        .fg(app.theme.color(R::SelectionFg))
        .bg(app.theme.color(R::SelectionBg))
        .add_modifier(Modifier::BOLD);
    let muted = app.theme.style(R::TextMuted);
    let items: &[Mode] = &NAV_ITEMS;

    // Measure the full strip; when it can't fit, hand over to the paged variant instead
    // of letting the rightmost tabs clip into unreachability.
    let full: u16 = [MARGIN, BRAND, SEP, END_PAD]
        .iter()
        .map(|s| text_width(s))
        .sum::<u16>()
        + items
            .iter()
            .map(|m| text_width(nav_label(*m, app.radio_dedicated_mode)) + 2)
            .sum::<u16>()
        + text_width(GAP) * (items.len() as u16 - 1);
    if full > area.width {
        render_nav_paged(frame, app, area, items, active, muted);
        return;
    }

    // Left-aligned strip starting at the inner edge. Brand + separator are static labels;
    // each tab is clickable, so we walk `x` in step with the spans to keep hit rects on text.
    let mut spans = Vec::with_capacity(items.len() * 4 + 4);
    let mut x = area.x;

    // Left gutter doubles as the global animation toggle: a ✨ when animations are on, two blank
    // cells when off. Both are exactly 2 cells wide (`text_width("✨") == text_width("  ") == 2`),
    // so the brand and tabs never shift between states. The hit rect is published in both states,
    // so clicking the blank slot turns animations back on. Color emoji ignore SGR fg on many
    // terminals, so the on/off signal is the glyph itself (✨ vs blank), not its color.
    let spark = if app.animations().master {
        "✨"
    } else {
        MARGIN
    };
    app.register_mouse_button(
        Rect {
            x,
            y: area.y,
            width: text_width(MARGIN),
            height: area.height.min(1),
        },
        MouseTarget::Global(Action::ToggleAnimations),
    );
    spans.push(Span::styled(spark, brand));
    x = x.saturating_add(text_width(spark));

    // The brand doubles as a click target that opens the About card.
    app.register_mouse_button(
        Rect {
            x,
            y: area.y,
            width: text_width(BRAND),
            height: area.height.min(1),
        },
        MouseTarget::AboutTitle,
    );
    spans.push(Span::styled(BRAND, brand));
    x = x.saturating_add(text_width(BRAND));
    spans.push(Span::styled(SEP, sep));
    x = x.saturating_add(text_width(SEP));

    for (i, mode) in items.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(GAP, muted));
            x = x.saturating_add(text_width(GAP));
        }
        let label = nav_label(*mode, app.radio_dedicated_mode);
        let w = text_width(label).saturating_add(2);
        app.register_mouse_button(
            Rect {
                x,
                y: area.y,
                width: w,
                height: area.height.min(1),
            },
            MouseTarget::Nav(*mode),
        );
        let style = if app.mode == *mode {
            // A brief accent wash on the tab just switched to (identity when off).
            crate::ui::anim::active_tab_style(app, crate::ui::anim::TabPop::Nav, active)
        } else {
            muted
        };
        spans.push(Span::styled(" ", style));
        spans.push(Span::styled(label, style));
        spans.push(Span::styled(" ", style));
        x = x.saturating_add(w);
    }
    // Right gutter, then render into a rect sized to the text (not the full row) so when this
    // strip rides a border line the border keeps drawing in the cells past it.
    spans.push(Span::raw(END_PAD));
    x = x.saturating_add(text_width(END_PAD));
    let used = x.saturating_sub(area.x).min(area.width);
    let strip = Rect {
        width: used,
        ..area
    };
    frame.render_widget(Paragraph::new(Line::from(spans)), strip);
}

/// The narrow-width nav: `◀` and `▶` page to the previous / next screen, and between them
/// sits a window of tabs centered on the active screen, grown outward while width lasts.
/// Clicking an arrow both navigates and (because the window follows the active tab) slides
/// hidden tabs into view — two clicks reach any screen on even the tiniest grid.
fn render_nav_paged(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    items: &[Mode],
    active: Style,
    muted: Style,
) {
    const MARGIN: &str = " ";
    let arrow_on = app.theme.style(R::Accent).add_modifier(Modifier::BOLD);
    let active_idx = items.iter().position(|m| *m == app.mode).unwrap_or(0);

    // Grow the visible window outward from the active tab, right neighbor first (reading
    // direction), while everything — margins, both arrows, gaps — still fits.
    let tab_w = |i: usize| text_width(nav_label(items[i], app.radio_dedicated_mode)) + 2;
    let chrome = text_width(MARGIN) * 2 + 2 + 2; // margins + "◀ " + " ▶"
    let budget = area.width.saturating_sub(chrome);
    let (mut lo, mut hi) = (active_idx, active_idx);
    let mut used = tab_w(active_idx);
    loop {
        let right = (hi + 1 < items.len()).then(|| tab_w(hi + 1) + 1);
        let left = (lo > 0).then(|| tab_w(lo - 1) + 1);
        match (right, left) {
            (Some(r), _) if used + r <= budget => {
                hi += 1;
                used += r;
            }
            (_, Some(l)) if used + l <= budget => {
                lo -= 1;
                used += l;
            }
            _ => break,
        }
    }

    let mut spans = Vec::with_capacity((hi - lo + 1) * 3 + 6);
    let mut x = area.x;
    let push = |spans: &mut Vec<Span>, x: &mut u16, text: &str, style: Style| {
        spans.push(Span::styled(text.to_owned(), style));
        *x = x.saturating_add(text_width(text));
    };

    push(&mut spans, &mut x, MARGIN, muted);
    // `◀`: one screen back. Dimmed (and inert) at the first screen, like a scrollbar end.
    if active_idx > 0 {
        app.register_mouse_button(
            Rect {
                x,
                y: area.y,
                width: 2,
                height: area.height.min(1),
            },
            MouseTarget::Nav(items[active_idx - 1]),
        );
        push(&mut spans, &mut x, "◀ ", arrow_on);
    } else {
        push(&mut spans, &mut x, "◀ ", muted);
    }

    for (i, mode) in items.iter().enumerate().take(hi + 1).skip(lo) {
        if i > lo {
            push(&mut spans, &mut x, " ", muted);
        }
        let label = nav_label(*mode, app.radio_dedicated_mode);
        let w = text_width(label).saturating_add(2);
        app.register_mouse_button(
            Rect {
                x,
                y: area.y,
                width: w,
                height: area.height.min(1),
            },
            MouseTarget::Nav(*mode),
        );
        let style = if app.mode == *mode {
            crate::ui::anim::active_tab_style(app, crate::ui::anim::TabPop::Nav, active)
        } else {
            muted
        };
        push(&mut spans, &mut x, " ", style);
        push(&mut spans, &mut x, label, style);
        push(&mut spans, &mut x, " ", style);
    }

    // `▶`: one screen forward.
    if active_idx + 1 < items.len() {
        app.register_mouse_button(
            Rect {
                x,
                y: area.y,
                width: 2,
                height: area.height.min(1),
            },
            MouseTarget::Nav(items[active_idx + 1]),
        );
        push(&mut spans, &mut x, " ▶", arrow_on);
    } else {
        push(&mut spans, &mut x, " ▶", muted);
    }

    let used = x.saturating_sub(area.x).min(area.width);
    let strip = Rect {
        width: used,
        ..area
    };
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
                Rect {
                    x: area.x,
                    y: area.y + vis,
                    width: area.width,
                    height: 1,
                },
                MouseTarget::ListRow(logical),
            );
        }
    }
}

/// Draw a vertical scrollbar on `track`, but only when the content
/// overflows the viewport. `position` is the scroll offset (the first visible row), so the
/// thumb tracks the viewport through the list and reaches both ends. A no-op when everything
/// fits. The caller decides whether `track` is a right border column or the final in-list
/// column for borderless list regions.
pub fn render_list_scrollbar(
    frame: &mut Frame,
    app: &App,
    track: Rect,
    surface: ScrollSurface,
    content_len: usize,
    position: usize,
    viewport: usize,
) {
    if track.width == 0 || track.height == 0 {
        return;
    }
    let Some(thumb) =
        crate::ui::scroll::scrollbar_thumb(content_len, viewport, track.height, position)
    else {
        return;
    };
    let bar = Rect {
        x: track.x,
        y: track.y,
        width: 1,
        height: track.height,
    };
    app.register_mouse_button(bar, MouseTarget::Scrollbar(surface));

    let thumb_start = thumb.start;
    let thumb_end = thumb.start.saturating_add(thumb.len);
    for row in 0..bar.height {
        let is_thumb = row >= thumb_start && row < thumb_end;
        let (symbol, style) = if is_thumb {
            ("█", app.theme.style(R::Accent))
        } else {
            ("│", app.theme.style(R::BorderPrimary))
        };
        frame.render_widget(
            Paragraph::new(Line::from(symbol).style(style)),
            Rect {
                x: bar.x,
                y: bar.y + row,
                width: 1,
                height: 1,
            },
        );
    }
}

/// Footer hints: the keybinding cheat-sheet plus a mouse-only cheat-sheet icon. They read as
/// status-bar hints, not boxed buttons. Shared by every screen's footer.
pub fn render_help_button(frame: &mut Frame, app: &App, area: Rect) {
    let key_label = app.help_footer();
    // CP437 has no mouse glyph, and a `?` icon reads as a second help hint — retro mode
    // drops the icon and keeps the plain word.
    let mouse_label = if app.retro_mode() {
        t!("mouse", "마우스").to_owned()
    } else {
        format!("🖱 {}", t!("mouse", "마우스"))
    };
    let segs = [
        Seg::button(MouseTarget::Global(Action::ToggleHelp), key_label.as_str()),
        Seg::label("   "),
        Seg::button(MouseTarget::MouseHelp, mouse_label.as_str()),
    ];
    let hint = app.theme.style(R::TextMuted);
    render_segments(frame, app, area, &segs, hint, hint, Alignment::Center);
}

#[cfg(test)]
mod tests {
    use super::{nav_label, text_width};
    use crate::app::Mode;

    #[test]
    fn sparkle_and_blank_nav_slot_are_both_two_cells() {
        // The ✨ animation toggle and its blank "off" state share one fixed 2-cell slot in
        // `render_nav`, so the brand and tabs never shift between on/off. This pins the layout
        // assumption against a future `unicode-width` table change.
        assert_eq!(text_width("✨"), 2);
        assert_eq!(text_width("  "), 2);
    }

    #[test]
    fn radio_nav_label_keeps_player_width() {
        let _guard = crate::i18n::lock_for_test();

        assert_eq!(nav_label(Mode::Player, false), "Player");
        assert_eq!(nav_label(Mode::Player, true), "Radio ");
        assert_eq!(
            text_width(nav_label(Mode::Player, true)),
            text_width("Player")
        );

        crate::i18n::set_language(crate::i18n::Language::Korean);
        assert_eq!(nav_label(Mode::Player, false), "플레이어");
        assert_eq!(nav_label(Mode::Player, true), "라 디 오");
        assert_eq!(
            text_width(nav_label(Mode::Player, true)),
            text_width("플레이어")
        );
        crate::i18n::set_language(crate::i18n::Language::English);
    }
}
