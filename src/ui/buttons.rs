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
fn nav_label(mode: Mode, radio_mode: bool, local_mode: bool) -> &'static str {
    match mode {
        Mode::Player if radio_mode => t!("Radio ", "라 디 오"),
        Mode::Player => t!("Player", "플레이어"),
        // Keep the Search tab's display width stable while Local Deck swaps its meaning.
        Mode::Search if local_mode => t!("Find  ", "찾기"),
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
    /// Extra cells of flanking air (each side) folded into the hit rect — the rendered
    /// text is untouched. Lets a tiny control annex the dead spacing around it so its
    /// whole word is an easy click.
    pub hit_pad: u16,
}

impl<'a> Seg<'a> {
    pub fn button(target: MouseTarget, text: &'a str) -> Self {
        Self {
            target: Some(target),
            text,
            hit_pad: 0,
        }
    }

    /// A button whose hit rect grows `pad` cells into the air on each side of its text.
    /// The caller must own that air (label spacing, not a neighbouring control's cells):
    /// on overlap the later-registered rect wins (`App::mouse_region_at` scans newest
    /// first), so padding into an earlier neighbour would steal its clicks.
    pub fn padded_button(target: MouseTarget, text: &'a str, pad: u16) -> Self {
        Self {
            target: Some(target),
            text,
            hit_pad: pad,
        }
    }

    pub fn label(text: &'a str) -> Self {
        Self {
            target: None,
            text,
            hit_pad: 0,
        }
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
    render_segments_inner(
        frame,
        app,
        area,
        segments,
        SegmentRenderOptions {
            button_style,
            label_style,
            alignment,
            hit_height: 1,
            center_vertically: false,
        },
    );
}

/// Render a control strip in `area`, vertically centering its text while allowing clickable
/// segments to publish a taller hit rect. Used by confirmation popups whose buttons need
/// more vertical target area without changing normal one-line controls.
pub fn render_segments_with_hit_height(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    segments: &[Seg<'_>],
    styles: (Style, Style),
    alignment: Alignment,
    hit_height: u16,
) {
    let (button_style, label_style) = styles;
    render_segments_inner(
        frame,
        app,
        area,
        segments,
        SegmentRenderOptions {
            button_style,
            label_style,
            alignment,
            hit_height,
            center_vertically: true,
        },
    );
}

struct SegmentRenderOptions {
    button_style: Style,
    label_style: Style,
    alignment: Alignment,
    hit_height: u16,
    center_vertically: bool,
}

// Every in-tree control strip fits comfortably in this budget. Keeping the measured
// display widths inline avoids one heap allocation on each render; synthetic/plugin-like
// strips beyond the budget still get an exact-length backing slice.
const INLINE_SEGMENT_WIDTHS: usize = 32;

enum SegmentWidths {
    Inline {
        values: [u16; INLINE_SEGMENT_WIDTHS],
        len: usize,
    },
    Heap(Box<[u16]>),
}

impl SegmentWidths {
    fn measure(segments: &[Seg<'_>]) -> (Self, u16) {
        let mut widths = if segments.len() <= INLINE_SEGMENT_WIDTHS {
            Self::Inline {
                values: [0; INLINE_SEGMENT_WIDTHS],
                len: segments.len(),
            }
        } else {
            Self::Heap(vec![0; segments.len()].into_boxed_slice())
        };
        let mut total = 0u16;
        for (slot, seg) in widths.as_mut_slice().iter_mut().zip(segments) {
            let width = text_width(seg.text);
            *slot = width;
            total = total.saturating_add(width);
        }
        (widths, total)
    }

    fn as_slice(&self) -> &[u16] {
        match self {
            Self::Inline { values, len } => &values[..*len],
            Self::Heap(values) => values,
        }
    }

    fn as_mut_slice(&mut self) -> &mut [u16] {
        match self {
            Self::Inline { values, len } => &mut values[..*len],
            Self::Heap(values) => values,
        }
    }

    #[cfg(test)]
    fn is_heap(&self) -> bool {
        matches!(self, Self::Heap(_))
    }
}

fn render_segments_inner(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    segments: &[Seg<'_>],
    opts: SegmentRenderOptions,
) {
    let (widths, total) = SegmentWidths::measure(segments);
    let mut x = match opts.alignment {
        Alignment::Center => area.x + area.width.saturating_sub(total) / 2,
        Alignment::Right => area.x + area.width.saturating_sub(total),
        Alignment::Left => area.x,
    };

    let mut spans = Vec::with_capacity(segments.len());
    for (seg, width) in segments.iter().zip(widths.as_slice().iter().copied()) {
        let style = if let Some(target) = seg.target.clone() {
            // The rect hugs the text, plus any `hit_pad` cells of flanking air —
            // clamped to `area` so padding never escapes the strip's row.
            let x0 = x.saturating_sub(seg.hit_pad).max(area.x);
            let end = x
                .saturating_add(width)
                .saturating_add(seg.hit_pad)
                .min(area.right());
            let hit_h = opts.hit_height.min(area.height);
            let hit_y = if opts.center_vertically {
                area.y + area.height.saturating_sub(hit_h) / 2
            } else {
                area.y
            };
            app.register_mouse_button(
                Rect {
                    x: x0,
                    y: hit_y,
                    width: end.saturating_sub(x0),
                    height: hit_h,
                },
                target,
            );
            opts.button_style
        } else {
            opts.label_style
        };
        spans.push(Span::styled(seg.text, style));
        x = x.saturating_add(width);
    }

    let text_area = if opts.center_vertically {
        Rect {
            y: area.y + area.height.saturating_sub(1) / 2,
            height: area.height.min(1),
            ..area
        }
    } else {
        area
    };
    frame.render_widget(
        Paragraph::new(Line::from(spans).alignment(opts.alignment)),
        text_area,
    );
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

/// The screen nav bar shown at the top of every view: `yututui │ Player · Search ·
/// Library · Settings · DJ Gem` (or `Radio` for the player tab in dedicated Radio mode).
/// The `yututui` brand sits at the top-left, set off by a muted
/// `│`; the tabs follow. The active screen is highlighted (selection colors), the rest are
/// muted, and each tab is a click target that switches screens. Left-aligned, no box chrome
/// — it reads like a tab strip, consistent with the in-line "text is the button" controls.
///
/// When the full strip doesn't fit (a narrow terminal, or text zoom shrinking the virtual
/// grid), it degrades to a paged strip: `◀ ▶` arrows that switch to the previous / next
/// screen on click, around a window of tabs centered on the active one — so every screen
/// stays reachable by mouse no matter how little width remains.
///
/// Returns the strip's used width in cells, so a caller sharing the row (the DJ Gem
/// model label rides the same border line) can tell how much space remains to its right.
pub fn render_nav(frame: &mut Frame, app: &App, area: Rect) -> u16 {
    const GAP: &str = "  ";
    const BRAND: &str = "yututui";
    const SEP: &str = " │ ";
    // Blank gutters framing the strip: `MARGIN` left of the brand (also nudges the whole bar
    // right, off the corner); `END_PAD` after the last tab. END_PAD plus that tab's own
    // trailing space matches MARGIN, so the gap before the border resumes looks symmetric.
    const MARGIN: &str = "  ";
    const END_PAD: &str = " ";

    let brand = app.theme.style(R::TextPrimary).add_modifier(Modifier::BOLD);
    let sep = app.theme.style(R::BorderMuted);
    let active = Style::default()
        .fg(app.theme.color(R::SelectionFg))
        .bg(app.theme.color(R::SelectionBg))
        .add_modifier(Modifier::BOLD);
    let muted = app.theme.style(R::TextMuted);
    let items: &[Mode] = &NAV_ITEMS;

    // A small ● after the brand when a newer release is available — an always-visible hint
    // that pairs with the About card's notice. Two cells (` ●`) so it's measured like any
    // other static label; empty (zero width) when up to date, so the layout is unchanged.
    let update_dot = if app
        .overlays
        .update_status
        .as_ref()
        .is_some_and(|s| s.available)
    {
        " ●"
    } else {
        ""
    };

    // Measure the full strip; when it can't fit, hand over to the paged variant instead
    // of letting the rightmost tabs clip into unreachability.
    let full: u16 = [MARGIN, BRAND, update_dot, SEP, END_PAD]
        .iter()
        .map(|s| text_width(s))
        .sum::<u16>()
        + items
            .iter()
            .map(|m| {
                text_width(nav_label(
                    *m,
                    app.radio_dedicated_mode,
                    app.local_dedicated_mode,
                )) + 2
            })
            .sum::<u16>()
        + text_width(GAP) * (items.len() as u16 - 1);
    if full > area.width {
        return render_nav_paged(frame, app, area, items, active, muted);
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
    if !update_dot.is_empty() {
        spans.push(Span::styled(
            update_dot,
            app.theme.style(R::AccentAlt).add_modifier(Modifier::BOLD),
        ));
        x = x.saturating_add(text_width(update_dot));
    }
    spans.push(Span::styled(SEP, sep));
    x = x.saturating_add(text_width(SEP));

    for (i, mode) in items.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(GAP, muted));
            x = x.saturating_add(text_width(GAP));
        }
        let label = nav_label(*mode, app.radio_dedicated_mode, app.local_dedicated_mode);
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
    used
}

/// The narrow-width nav: `◀` and `▶` page to the previous / next screen, and between them
/// sits a window of tabs centered on the active screen, grown outward while width lasts.
/// Clicking an arrow both navigates and (because the window follows the active tab) slides
/// hidden tabs into view — two clicks reach any screen on even the tiniest grid.
/// Returns the strip's used width, like [`render_nav`].
fn render_nav_paged(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    items: &[Mode],
    active: Style,
    muted: Style,
) -> u16 {
    const MARGIN: &str = " ";
    let arrow_on = app.theme.style(R::Accent).add_modifier(Modifier::BOLD);
    let active_idx = items.iter().position(|m| *m == app.mode).unwrap_or(0);

    // Grow the visible window outward from the active tab, right neighbor first (reading
    // direction), while everything — margins, both arrows, gaps — still fits.
    let tab_w = |i: usize| {
        text_width(nav_label(
            items[i],
            app.radio_dedicated_mode,
            app.local_dedicated_mode,
        )) + 2
    };
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
    // `◀`: one screen back. Hollow (`◁`) and inert at the first screen, like a scrollbar
    // end — shape, not color, carries the on/off signal, since Accent vs TextMuted are
    // near-identical in some themes.
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
        push(&mut spans, &mut x, "◁ ", muted);
    }

    for (i, mode) in items.iter().enumerate().take(hi + 1).skip(lo) {
        if i > lo {
            push(&mut spans, &mut x, " ", muted);
        }
        let label = nav_label(*mode, app.radio_dedicated_mode, app.local_dedicated_mode);
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

    // `▶`: one screen forward. Hollow (`▷`) and inert at the last screen.
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
        push(&mut spans, &mut x, " ▷", muted);
    }

    let used = x.saturating_sub(area.x).min(area.width);
    let strip = Rect {
        width: used,
        ..area
    };
    frame.render_widget(Paragraph::new(Line::from(spans)), strip);
    used
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
    let target = if surface == ScrollSurface::LocalFind {
        MouseTarget::LocalFindScrollbar {
            stamp: app.local_find_pointer_stamp(),
        }
    } else {
        MouseTarget::Scrollbar(surface)
    };
    app.register_mouse_button(bar, target);

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
    let mut segs = vec![
        Seg::button(MouseTarget::Global(Action::ToggleHelp), key_label.as_str()),
        Seg::label("   "),
        Seg::button(MouseTarget::MouseHelp, mouse_label.as_str()),
    ];
    // The docked-bar collapse/expand toggle, right of the mouse hint — only where it can
    // act (Bottom bar mode, off the Player screen), so the legacy Top footer stays
    // byte-identical. Retro stand-ins are chosen at the source, like the mouse label.
    if app.player_bar_position() == crate::config::PlayerBarPosition::Bottom
        && app.mode != Mode::Player
    {
        let glyph = match (app.config.control_box_collapsed(), app.retro_mode()) {
            (false, false) => "▼",
            (true, false) => "▲",
            (false, true) => "v",
            (true, true) => "^",
        };
        segs.push(Seg::label("   "));
        segs.push(Seg::button(
            MouseTarget::Global(Action::ToggleControlBox),
            glyph,
        ));
    }
    let hint = app.theme.style(R::TextMuted);
    render_segments(frame, app, area, &segs, hint, hint, Alignment::Center);
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::{Alignment, Rect};
    use ratatui::style::Style;

    use super::{
        INLINE_SEGMENT_WIDTHS, Seg, SegmentWidths, nav_label, render_segments, text_width,
    };
    use crate::app::{App, Mode, MouseTarget};
    use crate::keymap::Action;

    #[test]
    fn segment_width_scratch_uses_heap_only_beyond_inline_budget() {
        let inline = (0..INLINE_SEGMENT_WIDTHS)
            .map(|_| Seg::label("한"))
            .collect::<Vec<_>>();
        let (widths, total) = SegmentWidths::measure(&inline);
        assert!(!widths.is_heap());
        assert_eq!(widths.as_slice(), vec![2; INLINE_SEGMENT_WIDTHS]);
        assert_eq!(total, (INLINE_SEGMENT_WIDTHS as u16) * 2);

        let heap = (0..=INLINE_SEGMENT_WIDTHS)
            .map(|_| Seg::label("✨"))
            .collect::<Vec<_>>();
        let (widths, total) = SegmentWidths::measure(&heap);
        assert!(widths.is_heap());
        assert_eq!(widths.as_slice(), vec![2; INLINE_SEGMENT_WIDTHS + 1]);
        assert_eq!(total, ((INLINE_SEGMENT_WIDTHS + 1) as u16) * 2);
    }

    #[test]
    fn heap_backed_unicode_strip_keeps_rendered_cells_and_hit_rects_aligned() {
        let mut segments = (0..30).map(|_| Seg::label("a")).collect::<Vec<_>>();
        segments.push(Seg::button(MouseTarget::Global(Action::ToggleHelp), "한"));
        segments.push(Seg::label("·"));
        segments.push(Seg::button(MouseTarget::MouseHelp, "✨"));
        assert!(segments.len() > INLINE_SEGMENT_WIDTHS);

        let app = App::new(100);
        let backend = TestBackend::new(40, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_segments(
                    frame,
                    &app,
                    Rect::new(2, 1, 38, 1),
                    &segments,
                    Style::default(),
                    Style::default(),
                    Alignment::Left,
                );
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        for x in 2..32 {
            assert_eq!(buffer[(x, 1)].symbol(), "a");
        }
        assert_eq!(buffer[(32, 1)].symbol(), "한");
        assert_eq!(buffer[(34, 1)].symbol(), "·");
        assert_eq!(buffer[(35, 1)].symbol(), "✨");

        let help = Some(MouseTarget::Global(Action::ToggleHelp));
        assert_eq!(app.hits.target_at(32, 1), help);
        assert_eq!(app.hits.target_at(33, 1), help);
        assert_eq!(app.hits.target_at(34, 1), None);
        assert_eq!(app.hits.target_at(35, 1), Some(MouseTarget::MouseHelp));
        assert_eq!(app.hits.target_at(36, 1), Some(MouseTarget::MouseHelp));
    }

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

        assert_eq!(nav_label(Mode::Player, false, false), "Player");
        assert_eq!(nav_label(Mode::Player, true, false), "Radio ");
        assert_eq!(
            text_width(nav_label(Mode::Player, true, false)),
            text_width("Player")
        );

        crate::i18n::set_language(crate::i18n::Language::Korean);
        assert_eq!(nav_label(Mode::Player, false, false), "플레이어");
        assert_eq!(nav_label(Mode::Player, true, false), "라 디 오");
        assert_eq!(
            text_width(nav_label(Mode::Player, true, false)),
            text_width("플레이어")
        );
        crate::i18n::set_language(crate::i18n::Language::English);
    }

    #[test]
    fn local_find_nav_label_keeps_search_width() {
        let _guard = crate::i18n::lock_for_test();

        assert_eq!(nav_label(Mode::Search, false, false), "Search");
        assert_eq!(nav_label(Mode::Search, false, true), "Find  ");
        assert_eq!(
            text_width(nav_label(Mode::Search, false, true)),
            text_width("Search")
        );

        crate::i18n::set_language(crate::i18n::Language::Korean);
        assert_eq!(nav_label(Mode::Search, false, false), "검색");
        assert_eq!(nav_label(Mode::Search, false, true), "찾기");
        assert_eq!(
            text_width(nav_label(Mode::Search, false, true)),
            text_width("검색")
        );
        crate::i18n::set_language(crate::i18n::Language::English);
    }
}
