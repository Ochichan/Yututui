//! The "Why DJ Gem" overlay: a small, centered card explaining the last autoplay-streaming refill that
//! went through the Gemini reranker. For each pick it shows the slot role the model assigned
//! (core / bridge / discovery / …), the track it landed on, and the evidence codes the model
//! said it leaned on — so the DJ Gem's choices are inspectable rather than opaque. Opened with `w`
//! ([`crate::keymap::Action::WhyAi`]); dismissed by `w` / Esc / Back. Styled with the same theme
//! roles as the `?` cheat-sheet and About card so it matches whatever preset is active.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::app::App;
use crate::keymap::{Action, KeyContext};
use crate::t;
use crate::theme::ThemeRole as R;
use crate::ui::text::truncate_to_width;

/// Render the "Why DJ Gem" card as a centered popup over `area`.
pub fn render(frame: &mut Frame, app: &App, area: Rect) {
    let Some(explain) = app.streaming.last_explain.as_ref() else {
        return;
    };

    // One row per pick (role + track) plus a muted reasons row beneath it; clamp to the screen.
    let body_rows = (explain.picks.len() as u16) * 2;
    let h = (body_rows + 5).clamp(7, area.height);
    let popup = centered_fixed(area, 72, h);
    crate::ui::render_popup_background(frame, app, popup);

    let title = match explain.conf {
        Some(c) => format!(
            " {} · {:.0}% ",
            t!("Why these DJ Gem picks", "DJ Gem 선곡 이유"),
            (c * 100.0).round()
        ),
        None => format!(" {} ", t!("Why these DJ Gem picks", "DJ Gem 선곡 이유")),
    };
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(crate::ui::popup_style(app, R::BorderPrimary))
        .style(crate::ui::popup_style(app, R::TextPrimary));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let rows = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);
    draw_picks(frame, app, rows[0], explain);

    // The close key respects rebinds (it's the chord bound to `WhyAi`, not a hardcoded `w`).
    let close_key =
        app.keymap
            .label_for_display(KeyContext::Global, Action::WhyAi, app.retro_mode());
    let hint = if crate::i18n::is_korean() {
        format!("{close_key} / Esc 닫기")
    } else {
        format!("{close_key} / Esc to close")
    };
    frame.render_widget(
        Paragraph::new(Line::from(hint))
            .alignment(Alignment::Center)
            .style(crate::ui::popup_style(app, R::TextMuted)),
        rows[1],
    );
    crate::ui::seal_popup_background(frame, app, popup);
    crate::ui::mark_art_rows_for_popup(frame, app, popup);
}

/// Draw the numbered pick list: each pick is a role badge + track line, with a muted reason line
/// beneath it.
fn draw_picks(frame: &mut Frame, app: &App, area: Rect, explain: &crate::app::StreamingAiExplain) {
    let width = area.width.saturating_sub(2) as usize;
    let role_style = crate::ui::popup_style(app, R::Accent).add_modifier(Modifier::BOLD);
    let track_style = crate::ui::popup_style(app, R::HelpAction);
    let reason_style = crate::ui::popup_style(app, R::TextMuted);

    let mut lines: Vec<Line> = Vec::with_capacity(explain.picks.len() * 2);
    for (i, p) in explain.picks.iter().enumerate() {
        let role = p.role.as_deref().map(role_label).unwrap_or("·");
        let track = truncate_to_width(
            &format!("{} — {}", p.title, p.artist),
            width.saturating_sub(12),
        );
        lines.push(Line::from(vec![
            Span::styled(format!("{:>2}. ", i + 1), reason_style),
            Span::styled(format!("[{role}] "), role_style),
            Span::styled(track, track_style),
        ]));
        let reasons = if p.reasons.is_empty() {
            t!("(no reasons given)", "(이유 없음)").to_owned()
        } else {
            p.reasons
                .iter()
                .map(|c| reason_label(c))
                .collect::<Vec<_>>()
                .join(", ")
        };
        lines.push(Line::from(Span::styled(
            format!(
                "      {}",
                truncate_to_width(&reasons, width.saturating_sub(6))
            ),
            reason_style,
        )));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

/// Human-readable slot-role label (the model returns terse codes).
fn role_label(role: &str) -> &str {
    match role {
        "core" => t!("core", "핵심"),
        "bridge" => t!("bridge", "연결"),
        "adjacent" => t!("adjacent", "인접"),
        "discovery" => t!("discovery", "발견"),
        "stabilizer" => t!("stabilizer", "안정"),
        "recovery" => t!("recovery", "회복"),
        other => other,
    }
}

/// Human-readable evidence-code label (matches the score codes in the candidate pack).
fn reason_label(code: &str) -> &str {
    match code {
        "co" => t!("co-occurrence", "동시청취"),
        "tr" => t!("transition", "전환"),
        "u" => t!("your affinity", "선호"),
        "nov" => t!("novelty", "새로움"),
        "cont" => t!("continuation", "연속성"),
        "comp" => t!("completion", "완청"),
        "m" => t!("official music", "공식음원"),
        other => other,
    }
}

/// A `w`×`h` rect centered in `area`, clamped so it never exceeds the available space.
fn centered_fixed(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect {
        x: area.x + area.width.saturating_sub(w) / 2,
        y: area.y + area.height.saturating_sub(h) / 2,
        width: w,
        height: h,
    }
}
