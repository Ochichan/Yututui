//! The AI assistant view: a chat transcript over an optional pickable suggestions list
//! and a prompt input box. Mirrors the search view's focus-accent convention.
//!
//! When no API key is configured the transcript area shows an onboarding block instead;
//! the input still works (submitting yields an inline error pointing at settings).

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};

use crate::app::{AiFocus, AiRole, App};
use crate::theme::ThemeRole as R;
use crate::ui::buttons;

/// Max rows the suggestions list takes when present.
const SUGGESTIONS_HEIGHT: u16 = 7;

pub fn render(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .title(format!(" AI Assistant · {} ", app.gemini_model.label()))
        .borders(Borders::ALL)
        .border_style(app.theme.style(R::BorderPrimary))
        .style(app.theme.style(R::TextPrimary));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let has_suggestions = !app.ai_suggestions.is_empty();
    let suggestions_rows = if has_suggestions {
        SUGGESTIONS_HEIGHT.min(app.ai_suggestions.len() as u16 + 1)
    } else {
        0
    };

    let rows = Layout::vertical([
        Constraint::Length(1),                // nav bar
        Constraint::Min(0),                   // transcript
        Constraint::Length(suggestions_rows), // suggestions (0 when none)
        Constraint::Length(3),                // input box
        Constraint::Length(1),                // help
    ])
    .split(inner);

    buttons::render_nav(frame, app, rows[0]);
    render_transcript(frame, app, rows[1]);
    if has_suggestions {
        render_suggestions(frame, app, rows[2]);
    }
    render_input(frame, app, rows[3]);

    buttons::render_help_button(frame, app, rows[4]);
}

fn render_transcript(frame: &mut Frame, app: &App, area: Rect) {
    // Onboarding when no key is set and nothing has been said yet.
    if !app.ai_available && app.ai_messages.is_empty() {
        let lines = vec![
            Line::from("AI assistant — control playback in plain language.".bold()),
            Line::from(""),
            Line::from("No Gemini API key is configured.").style(app.theme.style(R::Warning)),
            Line::from("Add one in Settings (press , then the AI tab),").style(app.theme.style(R::TextPrimary)),
            Line::from("or set the GEMINI_API_KEY environment variable.").style(app.theme.style(R::TextPrimary)),
            Line::from(""),
            Line::from("Then ask things like:").style(app.theme.style(R::TextMuted)),
            Line::from("  \"play some lo-fi beats\"").style(app.theme.style(R::TextMuted)),
            Line::from("  \"queue three upbeat songs\"").style(app.theme.style(R::TextMuted)),
            Line::from("  \"start a radio based on what's playing\"").style(app.theme.style(R::TextMuted)),
        ];
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
        return;
    }

    let mut lines: Vec<Line> = Vec::new();
    for m in &app.ai_messages {
        let (prefix, role) = match m.role {
            AiRole::User => ("you ", R::AiUser),
            AiRole::Ai => ("ai  ", R::AiAssistant),
            AiRole::Error => ("err ", R::AiError),
        };
        // First visual line carries the role prefix; wrapping handles the rest.
        lines.push(Line::from(format!("{prefix}{}", m.text)).style(app.theme.style(role)));
    }
    if app.ai_thinking {
        lines.push(Line::from("ai  …thinking".to_owned()).style(app.theme.style(R::AiThinking)));
    }
    if lines.is_empty() {
        lines.push(Line::from("Ask me to play, queue, or find music.").style(app.theme.style(R::TextMuted)));
    }

    // Keep the latest content visible: scroll so the last lines sit at the bottom.
    let height = area.height as usize;
    let scroll = lines.len().saturating_sub(height) as u16;
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).scroll((scroll, 0)),
        area,
    );
}

fn render_suggestions(frame: &mut Frame, app: &App, area: Rect) {
    let focused = app.ai_focus == AiFocus::Suggestions;
    let border = if focused { R::BorderFocused } else { R::BorderMuted };
    let block = Block::default()
        .title(" Suggestions ")
        .borders(Borders::TOP)
        .border_style(app.theme.style(border))
        .style(app.theme.style(R::TextPrimary));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let items: Vec<ListItem> = app
        .ai_suggestions
        .iter()
        .map(|s| {
            let heart = if app.library.is_favorite(&s.video_id) { "♥ " } else { "" };
            ListItem::new(format!("{heart}{} — {}", s.title, s.artist)).style(app.theme.style(R::TextPrimary))
        })
        .collect();

    let highlight = if focused {
        Style::default()
            .fg(app.theme.color(R::SelectionFg))
            .bg(app.theme.color(R::SelectionBg))
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(app.theme.color(R::SelectionInactiveFg))
            .bg(app.theme.color(R::SelectionInactiveBg))
    };
    let list = List::new(items).style(app.theme.style(R::TextPrimary)).highlight_style(highlight).highlight_symbol("▶ ");

    let mut state = ListState::default();
    state.select(Some(app.ai_suggestions_selected.min(app.ai_suggestions.len().saturating_sub(1))));
    frame.render_stateful_widget(list, inner, &mut state);
}

fn render_input(frame: &mut Frame, app: &App, area: Rect) {
    let focused = app.ai_focus == AiFocus::Input;
    let border = if focused { R::BorderFocused } else { R::BorderMuted };
    let block = Block::default()
        .title(" Ask ")
        .borders(Borders::ALL)
        .border_style(app.theme.style(border))
        .style(app.theme.style(R::TextPrimary));
    let text = if focused {
        format!("{}\u{2588}", app.ai_input)
    } else {
        app.ai_input.clone()
    };
    frame.render_widget(Paragraph::new(text).style(app.theme.style(R::TextPrimary)).block(block), area);
}
