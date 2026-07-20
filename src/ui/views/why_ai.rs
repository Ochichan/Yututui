//! The per-track WhyGem overlay. It explains the recommendation provenance of the queue row
//! selected by the reducer; model-owned codes cross a localized allow-list before rendering.

use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

use crate::app::App;
use crate::app::MouseTarget;
use crate::i18n::{self, why_gem};
use crate::keymap::{Action, KeyContext};
use crate::theme::ThemeRole as R;
use crate::ui::text::truncate_to_width;

const CARD_WIDTH: u16 = 68;

/// Render the selected/current track's WhyGem card. Missing or stale targets fail closed.
pub fn render(frame: &mut Frame, app: &App, area: Rect) {
    let Some((song, model)) = app.why_gem_target_song() else {
        return;
    };
    if area.is_empty() {
        return;
    }

    let popup_width = CARD_WIDTH.min(area.width);
    let content_width = popup_width.saturating_sub(2).max(1);
    let title = app.display_title(song);
    let artist = app.display_artist(song);
    let safe_title = truncate_to_width(title.as_ref(), usize::from(content_width));
    let safe_artist = truncate_to_width(artist.as_ref(), usize::from(content_width));
    let role = why_gem::role(&model.slot);
    // Lowercase role slots are DJ Gem model output. Streaming origins use unambiguous
    // title-case slots, so `discovery` can never masquerade as the Discovery station here.
    let origin = if role.is_some() {
        why_gem::origin("dj_gem")
    } else {
        why_gem::origin(&model.slot)
    };
    let reasons = safe_reasons(&model.reasons);
    let confidence = model
        .confidence
        .as_ref()
        .and_then(serde_json::Number::as_f64)
        .map(|value| value.clamp(0.0, 1.0));

    let source_text = format!("{}: {}", why_gem::origin_label(), origin);
    let mut body_rows = 2_u16.saturating_add(wrapped_rows(&source_text, content_width));
    if let Some(role) = role {
        let role_text = format!("{}: {role}", why_gem::role_label());
        body_rows = body_rows.saturating_add(wrapped_rows(&role_text, content_width));
    }
    for reason in &reasons {
        body_rows = body_rows.saturating_add(wrapped_rows(reason, content_width.saturating_sub(2)));
    }
    if confidence.is_some() {
        let confidence_text = format!("{}: 100%", why_gem::confidence_label());
        body_rows = body_rows.saturating_add(wrapped_rows(&confidence_text, content_width));
    }
    let popup_height = body_rows.saturating_add(3).min(area.height); // borders + close hint
    let popup = centered_fixed(area, popup_width, popup_height);

    crate::ui::render_popup_background(frame, app, popup);
    let block = Block::default()
        .title(format!(" {} ", why_gem::title()))
        .borders(Borders::ALL)
        .border_style(crate::ui::popup_style(app, R::BorderPrimary))
        .style(crate::ui::popup_style(app, R::TextPrimary));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let hint_height = inner.height.min(1);
    let body = Rect {
        x: inner.x,
        y: inner.y,
        width: inner.width,
        height: inner.height.saturating_sub(hint_height),
    };
    let hint = Rect {
        x: inner.x,
        y: inner.bottom().saturating_sub(hint_height),
        width: inner.width,
        height: hint_height,
    };
    if !body.is_empty() {
        draw_details(
            frame,
            app,
            body,
            &safe_title,
            &safe_artist,
            origin,
            role,
            &reasons,
            confidence,
        );
    }
    if !hint.is_empty() {
        draw_close_hint(frame, app, hint);
    }

    crate::ui::seal_popup_background(frame, app, popup);
    crate::ui::mark_art_rows_for_popup(frame, app, popup);
    // Published last, over the covered player/queue hit map, so clicks inside the modal are
    // consumed while clicks outside can close it without activating anything underneath.
    app.register_mouse_button(popup, MouseTarget::WhyGemCard);
}

#[allow(clippy::too_many_arguments)]
fn draw_details(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    title: &str,
    artist: &str,
    origin: &str,
    role: Option<&str>,
    reasons: &[&str],
    confidence: Option<f64>,
) {
    let heading = crate::ui::popup_style(app, R::HelpAction).add_modifier(Modifier::BOLD);
    let label = crate::ui::popup_style(app, R::TextMuted);
    let value = crate::ui::popup_style(app, R::TextPrimary);
    let reason_style = crate::ui::popup_style(app, R::HelpAction);
    let mut lines = Vec::with_capacity(5 + reasons.len());
    lines.push(Line::from(Span::styled(title.to_owned(), heading)));
    lines.push(Line::from(Span::styled(artist.to_owned(), label)));
    lines.push(labelled_line(why_gem::origin_label(), origin, label, value));
    if let Some(role) = role {
        lines.push(labelled_line(why_gem::role_label(), role, label, value));
    }
    lines.extend(
        reasons
            .iter()
            .map(|reason| Line::from(Span::styled(format!("- {reason}"), reason_style))),
    );
    if let Some(confidence) = confidence {
        let percent = format!("{:.0}%", (confidence * 100.0).round());
        lines.push(Line::from(vec![
            Span::styled(format!("{}: ", why_gem::confidence_label()), label),
            Span::styled(percent, value),
        ]));
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn labelled_line<'a>(
    label: &'a str,
    value: &'a str,
    label_style: ratatui::style::Style,
    value_style: ratatui::style::Style,
) -> Line<'a> {
    Line::from(vec![
        Span::styled(format!("{label}: "), label_style),
        Span::styled(value, value_style),
    ])
}

/// Bound and deduplicate the model list before allocating frame lines. Only the seven codes in
/// the localization catalog can survive, and a short scan keeps corrupt remote data from making
/// every render walk an arbitrarily large vector.
fn safe_reasons(codes: &[String]) -> Vec<&'static str> {
    let mut reasons = Vec::with_capacity(7);
    for reason in codes
        .iter()
        .take(32)
        .filter_map(|code| why_gem::reason(code))
    {
        if !reasons.contains(&reason) {
            reasons.push(reason);
        }
        if reasons.len() == 7 {
            break;
        }
    }
    reasons
}

fn draw_close_hint(frame: &mut Frame, app: &App, area: Rect) {
    let close_key =
        app.keymap
            .label_for_display(KeyContext::Global, Action::WhyAi, app.retro_mode());
    let text = match i18n::current() {
        i18n::Language::Korean => format!("{close_key} / Esc 닫기"),
        i18n::Language::Japanese => format!("{close_key} / Esc で閉じる"),
        _ => format!("{close_key} / Esc to close"),
    };
    frame.render_widget(
        Paragraph::new(text)
            .alignment(Alignment::Center)
            .style(crate::ui::popup_style(app, R::TextMuted)),
        area,
    );
}

fn wrapped_rows(text: &str, width: u16) -> u16 {
    let width = usize::from(width.max(1));
    let cells = UnicodeWidthStr::width(text);
    u16::try_from(cells.div_ceil(width).max(1)).unwrap_or(u16::MAX)
}

/// A `w`×`h` rect centered in `area`, clamped so it never exceeds the available space.
fn centered_fixed(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::api::Song;
    use crate::remote::proto::WhyGemModel;

    fn card_app(model: WhyGemModel) -> App {
        let mut app = App::new(50);
        app.queue.set(
            vec![Song::remote("track", "Test Track", "Test Artist", "3:00")],
            0,
        );
        app.why_gem.upsert("track".to_owned(), model);
        app.overlays.why_gem_video_id = Some("track".to_owned());
        app.overlays.why_gem_queue_index = Some(0);
        app.overlays.why_gem_queue_revision = Some(app.queue.rev());
        app
    }

    fn render_text(app: &App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, app, frame.area()))
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn wrapping_is_nonzero_and_saturating_for_tiny_widths() {
        assert_eq!(wrapped_rows("", 0), 1);
        assert_eq!(wrapped_rows("abcd", 0), 4);
        assert_eq!(wrapped_rows("한글", 2), 2);
    }

    #[test]
    fn centered_card_never_escapes_tiny_area() {
        for area in [
            Rect::new(0, 0, 0, 0),
            Rect::new(4, 7, 1, 1),
            Rect::new(u16::MAX - 2, u16::MAX - 2, 2, 2),
        ] {
            let card = centered_fixed(area, CARD_WIDTH, u16::MAX);
            assert_eq!(card.intersection(area), card);
        }
    }

    #[test]
    fn reasons_drop_unknown_codes_and_duplicates() {
        let _guard = crate::i18n::lock_for_test();
        let codes = vec![
            "tr".to_owned(),
            "model-output".to_owned(),
            "tr".to_owned(),
            "u".to_owned(),
        ];
        let reasons = safe_reasons(&codes);
        assert_eq!(reasons.len(), 2);
        assert!(reasons[0].contains("transition"));
        assert!(reasons[1].contains("preferences"));
    }

    #[test]
    fn source_only_card_omits_confidence_and_reason_copy() {
        let _guard = crate::i18n::lock_for_test();
        let app = card_app(WhyGemModel {
            slot: "Balanced".to_owned(),
            reasons: Vec::new(),
            confidence: None,
        });
        let text = render_text(&app, 80, 24);
        assert!(text.contains("Test Track"));
        assert!(text.contains("Balanced station"));
        assert!(!text.contains("Confidence"));
        assert!(!text.contains("- "));
    }

    #[test]
    fn detailed_card_localizes_known_model_codes_and_clamps_confidence() {
        let _guard = crate::i18n::lock_for_test();
        let app = card_app(WhyGemModel {
            slot: "bridge".to_owned(),
            reasons: vec!["tr".to_owned(), "raw-control\u{1b}".to_owned()],
            confidence: serde_json::Number::from_f64(1.4),
        });
        let text = render_text(&app, 80, 24);
        assert!(text.contains("Bridge"));
        assert!(text.contains("smooth transition"));
        assert!(text.contains("100%"));
        assert!(!text.contains("raw-control"));
    }

    #[test]
    fn discovery_source_and_discovery_role_are_not_confused() {
        let _guard = crate::i18n::lock_for_test();
        let source = card_app(WhyGemModel {
            slot: "Discovery".to_owned(),
            reasons: Vec::new(),
            confidence: None,
        });
        let source_text = render_text(&source, 80, 24);
        assert!(source_text.contains("Source: Discovery station"));
        assert!(!source_text.contains("Role: Discovery"));

        let role = card_app(WhyGemModel {
            slot: "discovery".to_owned(),
            reasons: vec!["nov".to_owned()],
            confidence: serde_json::Number::from_f64(0.8),
        });
        let role_text = render_text(&role, 80, 24);
        assert!(role_text.contains("Source: DJ Gem"));
        assert!(role_text.contains("Role: Discovery"));
        assert!(!role_text.contains("Source: Discovery station"));
    }

    #[test]
    fn duplicate_video_card_uses_the_selected_queue_occurrence_metadata() {
        let _guard = crate::i18n::lock_for_test();
        let mut app = App::new(50);
        app.queue.set(
            vec![
                Song::remote("dup", "First Occurrence", "First Artist", "3:00"),
                Song::remote("dup", "Second Occurrence", "Second Artist", "3:00"),
            ],
            0,
        );
        app.why_gem.upsert(
            "dup".to_owned(),
            WhyGemModel {
                slot: "Balanced".to_owned(),
                reasons: Vec::new(),
                confidence: None,
            },
        );
        app.overlays.why_gem_video_id = Some("dup".to_owned());
        app.overlays.why_gem_queue_index = Some(1);
        app.overlays.why_gem_queue_revision = Some(app.queue.rev());

        let text = render_text(&app, 80, 24);
        assert!(text.contains("Second Occurrence"));
        assert!(text.contains("Second Artist"));
        assert!(!text.contains("First Occurrence"));
    }

    #[test]
    fn card_render_does_not_panic_on_tiny_surfaces() {
        let app = card_app(WhyGemModel {
            slot: "Balanced".to_owned(),
            reasons: vec!["co".to_owned(), "tr".to_owned()],
            confidence: None,
        });
        for (width, height) in [(1, 1), (2, 2), (12, 4), (34, 10)] {
            let _ = render_text(&app, width, height);
        }
    }
}
