//! Width-aware Settings tab strip.

use std::ops::Range;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::{App, MouseTarget};
use crate::settings::{SettingsState, SettingsTab};
use crate::theme::ThemeRole as R;
use crate::ui::buttons;

const DIVIDER: &str = " ";
const BEFORE: &str = "‹ ";
const AFTER: &str = " ›";

pub(super) fn render(frame: &mut Frame, app: &App, settings: &SettingsState, area: Rect) {
    if area.is_empty() {
        return;
    }
    let theme = &settings.draft.theme;
    let active_style = Style::default()
        .fg(theme.color(R::SelectionFg))
        .bg(theme.color(R::SelectionBg))
        .add_modifier(Modifier::BOLD);
    let muted = theme.style(R::TextMuted);
    let visible = visible_range(settings.tab.index(), area.width);
    let mut spans = Vec::new();
    let mut x = area.x;

    if visible.start > 0 {
        spans.push(Span::styled(BEFORE, muted));
        x = x.saturating_add(buttons::text_width(BEFORE));
    }
    for index in visible.clone() {
        if index > visible.start {
            spans.push(Span::styled(DIVIDER, muted));
            x = x.saturating_add(buttons::text_width(DIVIDER));
        }
        let tab = SettingsTab::ALL[index];
        let label = padded_label(tab);
        let width = buttons::text_width(&label);
        let available = area.right().saturating_sub(x);
        if available > 0 {
            app.register_mouse_button(
                Rect {
                    x,
                    y: area.y,
                    width: width.min(available),
                    height: 1,
                },
                MouseTarget::SettingsTab(index),
            );
        }
        x = x.saturating_add(width);
        let style = if settings.tab == tab {
            crate::ui::anim::active_tab_style(app, crate::ui::anim::TabPop::Inner, active_style)
        } else {
            muted
        };
        spans.push(Span::styled(label, style));
    }
    if visible.end < SettingsTab::ALL.len() {
        spans.push(Span::styled(AFTER, muted));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn visible_range(active: usize, width: u16) -> Range<usize> {
    let len = SettingsTab::ALL.len();
    if len == 0 {
        return 0..0;
    }
    let active = active.min(len - 1);
    let mut best: Option<(usize, usize, usize, usize)> = None;
    for start in 0..=active {
        for end in active + 1..=len {
            let used = range_width(start..end);
            if used > width {
                continue;
            }
            let count = end - start;
            let imbalance = (active - start).abs_diff(end - active - 1);
            let candidate = (count, usize::MAX - imbalance, start, end);
            if best.is_none_or(|current| candidate > current) {
                best = Some(candidate);
            }
        }
    }
    best.map_or(active..active + 1, |(_, _, start, end)| start..end)
}

fn range_width(range: Range<usize>) -> u16 {
    let tabs = range
        .clone()
        .map(|index| buttons::text_width(&padded_label(SettingsTab::ALL[index])))
        .fold(0u16, u16::saturating_add);
    let dividers = range
        .len()
        .saturating_sub(1)
        .saturating_mul(buttons::text_width(DIVIDER) as usize) as u16;
    let before = if range.start > 0 {
        buttons::text_width(BEFORE)
    } else {
        0
    };
    let after = if range.end < SettingsTab::ALL.len() {
        buttons::text_width(AFTER)
    } else {
        0
    };
    tabs.saturating_add(dividers)
        .saturating_add(before)
        .saturating_add(after)
}

fn padded_label(tab: SettingsTab) -> String {
    format!(" {} ", tab.label())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::Language;

    #[test]
    fn narrow_ranges_always_fit_and_keep_the_active_tab() {
        let _guard = crate::i18n::lock_for_test();
        for language in [Language::English, Language::Korean, Language::Japanese] {
            crate::i18n::set_language(language);
            for width in [28, 30, 32] {
                for active in 0..SettingsTab::ALL.len() {
                    let range = visible_range(active, width);
                    assert!(range.contains(&active), "{language:?} {width} {active}");
                    assert!(
                        range_width(range.clone()) <= width,
                        "{language:?} {width} {active}: {range:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn wide_range_shows_every_tab() {
        let _guard = crate::i18n::lock_for_test();
        crate::i18n::set_language(Language::English);
        assert_eq!(
            visible_range(SettingsTab::Sync.index(), 200),
            0..SettingsTab::ALL.len()
        );
    }
}
