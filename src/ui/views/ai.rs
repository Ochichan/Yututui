//! The AI assistant view: a chat transcript over an optional pickable suggestions list
//! and a prompt input box. Mirrors the search view's focus-accent convention.
//!
//! When no API key is configured the transcript area shows an onboarding block instead;
//! the input still works (submitting yields an inline error pointing at settings).

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};

use crate::app::{AiFocus, AiRole, App};
use crate::t;
use crate::theme::ThemeRole as R;
use crate::ui::buttons;

/// Max rows the suggestions list takes when present.
const SUGGESTIONS_HEIGHT: u16 = 7;

pub fn render(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        // Model indicator: kept as metadata but moved to the right of the border line so it
        // doesn't collide with the nav strip that now rides the left of the same line.
        .title(
            Line::from(format!(" {} ", app.gemini_model.label()))
                .style(app.theme.style(R::TextMuted))
                .alignment(Alignment::Right),
        )
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

    // The nav strip rides the top border line itself; `render_nav` overlays only the cells
    // its text covers, so the border keeps drawing on either side of it.
    buttons::render_nav(
        frame,
        app,
        Rect { x: inner.x, y: area.y, width: inner.width, height: 1 },
    );

    let rows = Layout::vertical([
        Constraint::Length(2),                // reserved top band (aligns with Settings/Library)
        Constraint::Min(0),                   // transcript
        Constraint::Length(suggestions_rows), // suggestions (0 when none)
        Constraint::Length(3),                // input box
        Constraint::Length(1),                // help
    ])
    .split(inner);

    render_transcript(frame, app, rows[1]);
    if has_suggestions {
        render_suggestions(frame, app, rows[2]);
    }
    render_input(frame, app, rows[3]);

    buttons::render_help_button(frame, app, rows[4]);

    // "Gemini-tan" mascot sits in the upper-center-right while the start screen shows.
    // Drawn last so it overlays cleanly; it hides once a conversation begins.
    if app.ai_messages.is_empty() {
        render_mascot(frame, inner);
    }
}

/// Moe-ified "Gemini-tan" mascot — a chibi girl drawn in Gemini's blue→purple→cyan
/// palette with raw `Color::Rgb`, so she keeps her brand colors under any theme. Sits in
/// the upper-center-right of the start screen and is skipped on windows too small to hold
/// her clear of the left-aligned welcome text.
fn render_mascot(frame: &mut Frame, inner: Rect) {
    const ART_W: u16 = 15;
    const ART_H: u16 = 10;
    // Widest onboarding line plus the chat left pad; keep the art clear of it.
    const TEXT_W: u16 = 54;
    if inner.width < TEXT_W + ART_W || inner.height < ART_H + 3 {
        return;
    }

    // Gemini palette: blue → indigo → purple → pink → cyan, plus a bright sparkle.
    let star = Color::Rgb(0xBF, 0xE4, 0xFF);
    let cyan = Color::Rgb(0x5B, 0xC8, 0xFA);
    let blue = Color::Rgb(0x42, 0x85, 0xF4);
    let indigo = Color::Rgb(0x6E, 0x6B, 0xF0);
    let purple = Color::Rgb(0xA7, 0x8B, 0xFA);
    let pink = Color::Rgb(0xE6, 0x9B, 0xF2);

    let sp = |s: &str, c: Color| Span::styled(s.to_string(), Style::default().fg(c));

    // Per-letter blue→purple→cyan gradient for the name plate.
    let label_cols = [blue, indigo, purple, pink, cyan, star];
    let mut label_line = vec![sp("    ", blue)];
    label_line.extend("GEMINI".chars().enumerate().map(|(i, c)| {
        Span::styled(
            c.to_string(),
            Style::default().fg(label_cols[i % label_cols.len()]).add_modifier(Modifier::BOLD),
        )
    }));
    label_line.push(sp("     ", blue));

    // 10×15 chibi: star crown, bangs, big eyes + smile, sparkle dress, name plate.
    let lines = vec![
        Line::from(vec![sp("    ⋆  ", cyan), sp("✦", star), sp("  ⋆    ", cyan)]),
        Line::from(vec![sp("  ╭─────────╮  ", blue)]),
        Line::from(vec![
            sp("  │", blue),
            sp("▔▔▔", blue),
            sp("▔▔▔", purple),
            sp("▔▔▔", cyan),
            sp("│  ", blue),
        ]),
        Line::from(vec![
            sp("  │ ", blue),
            sp("◕", cyan),
            sp("  ", blue),
            sp("▿", pink),
            sp("  ", blue),
            sp("◕", cyan),
            sp(" │  ", blue),
        ]),
        Line::from(vec![sp("  │   ", blue), sp("◡◡◡", pink), sp("   │  ", blue)]),
        Line::from(vec![sp("  ╰────", blue), sp("┬", purple), sp("────╯  ", blue)]),
        Line::from(vec![
            sp(" ", blue),
            sp("✧", cyan),
            sp(" ", blue),
            sp("╭───┴───╮", purple),
            sp(" ", blue),
            sp("✧", cyan),
            sp(" ", blue),
        ]),
        Line::from(vec![
            sp("   ", blue),
            sp("│", purple),
            sp(" ", blue),
            sp("✦", cyan),
            sp(" ", blue),
            sp("✦", cyan),
            sp(" ", blue),
            sp("✦", cyan),
            sp(" ", blue),
            sp("│", purple),
            sp("   ", blue),
        ]),
        Line::from(vec![sp("   ╰───────╯   ", purple)]),
        Line::from(label_line),
    ];

    // Nudge in from the right edge toward center, but never over the welcome text.
    let free_w = inner.width - ART_W;
    let x = inner.x + (free_w * 3 / 4).max(TEXT_W);
    let art_rect = Rect { x, y: inner.y + 1, width: ART_W, height: ART_H };
    frame.render_widget(Paragraph::new(lines), art_rect);
}

fn render_transcript(frame: &mut Frame, app: &App, area: Rect) {
    // A little left breathing room so chat text doesn't hug the left border.
    const CHAT_LEFT_PAD: u16 = 2;
    let area = Rect {
        x: area.x + CHAT_LEFT_PAD,
        width: area.width.saturating_sub(CHAT_LEFT_PAD),
        ..area
    };

    // Onboarding when no key is set and nothing has been said yet.
    if !app.ai_available && app.ai_messages.is_empty() {
        let lines = vec![
            Line::from(
                t!("AI assistant — control playback in plain language.", "AI 어시스턴트 — 평범한 말로 재생을 제어하세요.").bold(),
            ),
            Line::from(""),
            Line::from(t!("No Gemini API key is configured.", "Gemini API 키가 설정되지 않았어요."))
                .style(app.theme.style(R::Warning)),
            Line::from(t!(
                "Add one in Settings (press , then the AI tab),",
                "설정에서 추가하거나 ( , 를 누른 뒤 AI 탭),"
            ))
            .style(app.theme.style(R::TextPrimary)),
            Line::from(t!(
                "or set the GEMINI_API_KEY environment variable.",
                "GEMINI_API_KEY 환경 변수를 설정하세요."
            ))
            .style(app.theme.style(R::TextPrimary)),
            Line::from(""),
            Line::from(t!("Then ask things like:", "그런 다음 이렇게 물어보세요:")).style(app.theme.style(R::TextMuted)),
            Line::from(t!("  \"play some lo-fi beats\"", "  \"로파이 음악 좀 틀어줘\"")).style(app.theme.style(R::TextMuted)),
            Line::from(t!("  \"queue three upbeat songs\"", "  \"신나는 곡 세 개 대기열에 넣어줘\""))
                .style(app.theme.style(R::TextMuted)),
            Line::from(t!(
                "  \"start a radio based on what's playing\"",
                "  \"지금 나오는 곡으로 라디오 시작해줘\""
            ))
            .style(app.theme.style(R::TextMuted)),
        ];
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
        return;
    }

    let mut lines: Vec<Line> = Vec::new();
    for m in &app.ai_messages {
        // The prefix is a fixed-width label column; the Korean variants pad to 6 display cells
        // (한글 글자 = 2 cells) so the message text lines up across role lines.
        let (prefix, role) = match m.role {
            AiRole::User => (t!("you ", "나    "), R::AiUser),
            AiRole::Ai => (t!("ai  ", "ai    "), R::AiAssistant),
            AiRole::Error => (t!("err ", "오류  "), R::AiError),
        };
        // First visual line carries the role prefix; wrapping handles the rest.
        lines.push(Line::from(format!("{prefix}{}", m.text)).style(app.theme.style(role)));
    }
    if app.ai_thinking {
        lines.push(Line::from(t!("ai  …thinking", "ai    …생각 중").to_owned()).style(app.theme.style(R::AiThinking)));
    }
    if lines.is_empty() {
        lines.push(
            Line::from(t!(
                "Ask me to play, queue, or find music.",
                "재생, 대기열 추가, 음악 찾기를 부탁해 보세요."
            ))
            .style(app.theme.style(R::TextMuted)),
        );
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
        .title(t!(" Suggestions ", " 추천 곡 "))
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
        .title(t!(" Ask ", " 질문 "))
        .borders(Borders::ALL)
        .border_style(app.theme.style(border))
        .style(app.theme.style(R::TextPrimary));
    // Ctrl+A selects the whole prompt: paint it with the selection colors. Otherwise show a
    // trailing block cursor while focused, or plain text when not.
    let para = if focused && app.ai_select_all && !app.ai_input.is_empty() {
        let hl = Style::default()
            .fg(app.theme.color(R::SelectionFg))
            .bg(app.theme.color(R::SelectionBg));
        Paragraph::new(Line::from(Span::styled(app.ai_input.clone(), hl)))
    } else {
        let text = if focused {
            format!("{}\u{2588}", app.ai_input)
        } else {
            app.ai_input.clone()
        };
        Paragraph::new(text).style(app.theme.style(R::TextPrimary))
    };
    frame.render_widget(para.block(block), area);
}
