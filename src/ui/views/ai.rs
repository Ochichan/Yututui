//! The DJ Gem assistant view: a chat transcript over an optional pickable suggestions list
//! and a prompt input box. Mirrors the search view's focus-accent convention.
//!
//! When no API key is configured the transcript area shows an onboarding block instead;
//! the input still works (submitting yields an inline error pointing at settings).

use std::cell::RefCell;
use std::sync::Arc;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, HighlightSpacing, List, ListItem, ListState, Paragraph, Wrap,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{AiFocus, AiRole, App, MouseTarget, ScrollSurface, StatusKind};
use crate::t;
use crate::theme::ThemeRole as R;
use crate::ui::buttons;

/// Max rows the suggestions list takes when present.
const SUGGESTIONS_HEIGHT: u16 = 7;

pub fn render(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(app.theme.style(R::BorderPrimary))
        .style(app.theme.style(R::TextPrimary));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let has_suggestions = !app.ai.suggestions.is_empty();
    let suggestions_rows = if has_suggestions {
        SUGGESTIONS_HEIGHT.min(app.ai.suggestions.len() as u16 + 1)
    } else {
        0
    };

    // The nav strip rides the top border line itself; `render_nav` overlays only the cells
    // its text covers, so the border keeps drawing on either side of it.
    buttons::render_nav(
        frame,
        app,
        Rect {
            x: inner.x,
            y: area.y,
            width: inner.width,
            height: 1,
        },
    );

    let rows = Layout::vertical([
        Constraint::Length(2), // reserved top band (aligns with Settings/Library)
        Constraint::Min(0),    // transcript
        Constraint::Length(suggestions_rows), // suggestions (0 when none)
        Constraint::Length(3), // input box
        Constraint::Length(crate::ui::control_box::docked_rows(app)), // docked player bar
        Constraint::Length(1), // help
    ])
    .split(inner);

    if !app.status.text.is_empty() && !app.control_box_active() {
        // A just-set message types itself in while the toast animation's window runs.
        if let Some(line) = crate::ui::anim::status_toast_line(app, rows[0].width) {
            frame.render_widget(Paragraph::new(line), rows[0]);
        } else {
            let role = match app.status.kind {
                StatusKind::Error => R::Error,
                StatusKind::Info => R::Success,
            };
            frame.render_widget(
                Paragraph::new(
                    Line::from(app.status.text.as_str())
                        .style(app.theme.style(role))
                        .alignment(Alignment::Center),
                ),
                rows[0],
            );
        }
    }

    render_transcript(frame, app, rows[1]);
    if has_suggestions {
        render_suggestions(frame, app, rows[2]);
    }
    render_input(frame, app, rows[3]);
    crate::ui::control_box::render_docked(frame, app, rows[4]);

    render_footer(frame, app, rows[5]);

    // DJ Gem mascot sits in the upper-center-right while the start screen shows.
    // Drawn last so it overlays cleanly; it hides once a conversation begins.
    if app.ai.messages.is_empty() {
        render_mascot(frame, app, inner);
    }
}

fn render_mascot(frame: &mut Frame, app: &App, inner: Rect) {
    crate::ui::mascot::render_dj_gem(frame, app, inner);
}

fn render_footer(frame: &mut Frame, app: &App, area: Rect) {
    buttons::render_help_button(frame, app, area);

    let label = format!(" Model: {} ", app.ai.model.label());
    let label_w = buttons::text_width(&label);
    let key_label = app.help_footer();
    let mouse_label = if app.retro_mode() {
        t!("mouse", "마우스").to_owned()
    } else {
        format!("🖱 {}", t!("mouse", "마우스"))
    };
    let help_w = buttons::text_width(key_label.as_str())
        .saturating_add(buttons::text_width("   "))
        .saturating_add(buttons::text_width(mouse_label.as_str()));
    let help_left = area.x + area.width.saturating_sub(help_w) / 2;
    if area.x.saturating_add(label_w).saturating_add(1) <= help_left {
        let model_area = Rect {
            width: label_w,
            ..area
        };
        let segs = [buttons::Seg::button(MouseTarget::AiModel, label.as_str())];
        buttons::render_segments(
            frame,
            app,
            model_area,
            &segs,
            app.theme.style(R::Accent).add_modifier(Modifier::BOLD),
            app.theme.style(R::TextMuted),
            Alignment::Left,
        );
    }
}

fn render_transcript(frame: &mut Frame, app: &App, area: Rect) {
    // A little left breathing room so chat text doesn't hug the left border.
    const CHAT_LEFT_PAD: u16 = 2;
    let padded = Rect {
        x: area.x + CHAT_LEFT_PAD,
        width: area.width.saturating_sub(CHAT_LEFT_PAD),
        ..area
    };

    // Onboarding when no key is set and nothing has been said yet.
    if !app.ai.available && app.ai.messages.is_empty() {
        let lines = vec![
            Line::from(
                t!(
                    "DJ Gem assistant — control playback in plain language.",
                    "DJ Gem 어시스턴트 — 평범한 말로 재생을 제어하세요."
                )
                .bold(),
            ),
            Line::from(""),
            Line::from(t!(
                "No Gemini API key is configured.",
                "Gemini API 키가 설정되지 않았어요."
            ))
            .style(app.theme.style(R::Warning)),
            Line::from(t!(
                "Add one in Settings under the DJ Gem tab,",
                "설정의 DJ Gem 탭에서 추가하거나,"
            ))
            .style(app.theme.style(R::TextPrimary)),
            Line::from(t!(
                "or set the GEMINI_API_KEY environment variable.",
                "GEMINI_API_KEY 환경 변수를 설정하세요."
            ))
            .style(app.theme.style(R::TextPrimary)),
            Line::from(""),
            Line::from(t!("Then ask things like:", "그런 다음 이렇게 물어보세요:"))
                .style(app.theme.style(R::TextMuted)),
            Line::from(t!(
                "  \"play some lo-fi beats\"",
                "  \"로파이 음악 좀 틀어줘\""
            ))
            .style(app.theme.style(R::TextMuted)),
            Line::from(t!(
                "  \"queue three upbeat songs\"",
                "  \"신나는 곡 세 개 대기열에 넣어줘\""
            ))
            .style(app.theme.style(R::TextMuted)),
            Line::from(t!(
                "  \"start streaming based on what's playing\"",
                "  \"지금 나오는 곡으로 라디오 시작해줘\""
            ))
            .style(app.theme.style(R::TextMuted)),
        ];
        *app.bridges.ai_transcript_copy_lines.borrow_mut() = Arc::default();
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), padded);
        return;
    }

    let text_area = Rect {
        width: padded.width.saturating_sub(1),
        ..padded
    };
    let (lines, copy_lines) = cached_transcript_lines(app, text_area.width as usize);
    *app.bridges.ai_transcript_copy_lines.borrow_mut() = copy_lines;

    let len = lines.len();
    let offset = app
        .bridges
        .ai_transcript_scroll
        .view_tail(text_area.height, len);
    let end = offset
        .saturating_add(text_area.height as usize)
        .min(lines.len());
    let selected = app.interaction.ai_transcript_drag.map(|drag| {
        if drag.anchor <= drag.cursor {
            (drag.anchor, drag.cursor)
        } else {
            (drag.cursor, drag.anchor)
        }
    });
    let selection_style = Style::default()
        .fg(app.theme.color(R::SelectionFg))
        .bg(app.theme.color(R::SelectionBg));
    let visible: Vec<Line> = lines[offset..end]
        .iter()
        .enumerate()
        .map(|(vis, line)| {
            let abs = offset + vis;
            if selected.is_some_and(|(start, end)| abs >= start && abs <= end) {
                Line::from(line.text.as_ref()).style(selection_style)
            } else {
                Line::from(
                    line.styles
                        .iter()
                        .map(|range| Span::styled(&line.text[range.start..range.end], range.style))
                        .collect::<Vec<Span>>(),
                )
            }
        })
        .collect();

    frame.render_widget(Paragraph::new(visible), text_area);
    register_transcript_rows(app, text_area, offset, len);
    buttons::render_list_scrollbar(
        frame,
        app,
        Rect {
            x: padded.x.saturating_add(padded.width.saturating_sub(1)),
            y: padded.y,
            width: 1,
            height: padded.height,
        },
        ScrollSurface::AiTranscript,
        len,
        offset,
        text_area.height as usize,
    );
}

#[derive(Debug, Clone)]
struct TranscriptLine {
    /// The only retained copy of this rendered row. Markdown markers are already
    /// stripped; rendering, selection highlighting, and drag-copy all share it.
    text: Arc<str>,
    /// Styled runs as UTF-8 byte ranges into `text`, never duplicated strings.
    styles: Vec<StyleRange>,
}

#[derive(Debug, Clone)]
struct StyleRange {
    start: usize,
    end: usize,
    style: Style,
}

struct TranscriptLineBuilder {
    text: String,
    styles: Vec<StyleRange>,
}

struct TranscriptCache {
    owner: Arc<()>,
    revision: u64,
    width: usize,
    korean: bool,
    thinking_text: Option<String>,
    styles: [Style; 6],
    #[cfg(test)]
    fixture_messages: Vec<(AiRole, String)>,
    lines: Arc<[TranscriptLine]>,
    copy_lines: Arc<[Arc<str>]>,
}

thread_local! {
    static TRANSCRIPT_CACHE: RefCell<Option<TranscriptCache>> = const { RefCell::new(None) };
}

impl TranscriptLine {
    fn blank() -> Self {
        Self {
            text: Arc::from(""),
            styles: Vec::new(),
        }
    }

    fn plain(text: String, style: Style) -> Self {
        let mut line = TranscriptLineBuilder::new(text.len());
        line.push_str(&text, style);
        line.finish()
    }
}

impl TranscriptLineBuilder {
    fn new(text_capacity: usize) -> Self {
        Self {
            text: String::with_capacity(text_capacity),
            styles: Vec::new(),
        }
    }

    fn push_str(&mut self, text: &str, style: Style) {
        if text.is_empty() {
            return;
        }
        let start = self.text.len();
        self.text.push_str(text);
        self.extend_style(start, style);
    }

    fn push_char(&mut self, ch: char, style: Style) {
        let start = self.text.len();
        self.text.push(ch);
        self.extend_style(start, style);
    }

    fn extend_style(&mut self, start: usize, style: Style) {
        let end = self.text.len();
        match self.styles.last_mut() {
            Some(range) if range.end == start && range.style == style => range.end = end,
            _ => self.styles.push(StyleRange { start, end, style }),
        }
    }

    fn finish(self) -> TranscriptLine {
        TranscriptLine {
            text: Arc::from(self.text),
            styles: self.styles,
        }
    }
}

#[cfg(test)]
fn build_transcript_lines(app: &App, width: usize) -> Vec<TranscriptLine> {
    let thinking = transcript_thinking_text(app);
    build_transcript_lines_with_thinking(app, width, thinking.as_deref())
}

fn build_transcript_lines_with_thinking(
    app: &App,
    width: usize,
    thinking_text: Option<&str>,
) -> Vec<TranscriptLine> {
    let mut lines = Vec::new();
    let width = width.max(1);
    let accent = app.theme.style(R::Accent);
    for m in &app.ai.messages {
        // A blank line between messages so exchanges breathe instead of stacking.
        if !lines.is_empty() {
            lines.push(TranscriptLine::blank());
        }
        let (prefix, role) = match m.role {
            AiRole::User => (t!("you ", "나    "), R::AiUser),
            AiRole::Ai => (t!("Gem ", "Gem   "), R::AiAssistant),
            AiRole::Error => (t!("err ", "오류  "), R::AiError),
        };
        let base = app.theme.style(role);
        // Only assistant replies speak markdown; user prompts and errors stay verbatim.
        let chars = if m.role == AiRole::Ai {
            markdown_styled_chars(&m.text, base, accent)
        } else {
            m.text.chars().map(|c| (c, base)).collect()
        };
        push_wrapped_styled(&mut lines, prefix, base, &chars, width);
    }
    if let Some(text) = thinking_text {
        if !lines.is_empty() {
            lines.push(TranscriptLine::blank());
        }
        let style = app.theme.style(R::AiThinking);
        let chars: Vec<(char, Style)> = text.chars().map(|c| (c, style)).collect();
        push_wrapped_styled(&mut lines, t!("Gem ", "Gem   "), style, &chars, width);
    }
    if lines.is_empty() {
        lines.push(TranscriptLine::plain(
            t!(
                "Ask me to play, queue, or find music.",
                "재생, 대기열 추가, 음악 찾기를 부탁해 보세요."
            )
            .to_owned(),
            app.theme.style(R::TextMuted),
        ));
    }
    lines
}

fn transcript_thinking_text(app: &App) -> Option<String> {
    app.ai.thinking.then(|| {
        // Animated dots while a request is in flight (the static text when the flag is off).
        match crate::ui::anim::activity_dots(app) {
            Some(dots) => format!("{}{dots}", t!("…thinking", "…생각 중")),
            None => t!("…thinking", "…생각 중").to_owned(),
        }
    })
}

fn cached_transcript_lines(app: &App, width: usize) -> (Arc<[TranscriptLine]>, Arc<[Arc<str>]>) {
    let thinking_text = transcript_thinking_text(app);
    let korean = crate::i18n::is_korean();
    let styles = [
        R::Accent,
        R::AiUser,
        R::AiAssistant,
        R::AiError,
        R::AiThinking,
        R::TextMuted,
    ]
    .map(|role| app.theme.style(role));
    #[cfg(test)]
    let fixture_messages = app
        .ai
        .messages
        .iter()
        .map(|message| (message.role, message.text.clone()))
        .collect::<Vec<_>>();

    TRANSCRIPT_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(cached) = cache.as_ref()
            && Arc::ptr_eq(&cached.owner, &app.ai.transcript_cache_token)
            && cached.revision == app.ai.transcript_revision
            && cached.width == width
            && cached.korean == korean
            && cached.thinking_text == thinking_text
            && cached.styles == styles
            && fixture_messages_match(cached, app)
        {
            return (Arc::clone(&cached.lines), Arc::clone(&cached.copy_lines));
        }

        let lines = build_transcript_lines_with_thinking(app, width, thinking_text.as_deref());
        let copy_lines: Arc<[Arc<str>]> = lines
            .iter()
            .map(|line| Arc::clone(&line.text))
            .collect::<Vec<_>>()
            .into();
        let lines: Arc<[TranscriptLine]> = lines.into();
        *cache = Some(TranscriptCache {
            owner: Arc::clone(&app.ai.transcript_cache_token),
            revision: app.ai.transcript_revision,
            width,
            korean,
            thinking_text,
            styles,
            #[cfg(test)]
            fixture_messages,
            lines: Arc::clone(&lines),
            copy_lines: Arc::clone(&copy_lines),
        });
        (lines, copy_lines)
    })
}

#[cfg(not(test))]
fn fixture_messages_match(_cached: &TranscriptCache, _app: &App) -> bool {
    true
}

/// UI tests intentionally build fixtures by mutating `messages` directly instead of going through
/// the private reducer helper. Keep that test seam exact without making production redraws hash or
/// compare the full transcript; production invalidation is the monotonic revision above.
#[cfg(test)]
fn fixture_messages_match(cached: &TranscriptCache, app: &App) -> bool {
    cached.fixture_messages.len() == app.ai.messages.len()
        && cached
            .fixture_messages
            .iter()
            .zip(&app.ai.messages)
            .all(|((role, text), message)| *role == message.role && text == &message.text)
}

/// Wrap one styled message under its role tag. The tag renders as a reversed-video chip
/// (its trailing padding stays plain) so who-said-what reads at a glance; continuation
/// lines hang-indent under the message body exactly as before.
fn push_wrapped_styled(
    out: &mut Vec<TranscriptLine>,
    prefix: &str,
    prefix_style: Style,
    chars: &[(char, Style)],
    width: usize,
) {
    let prefix_w = UnicodeWidthStr::width(prefix);
    let body_w = width.saturating_sub(prefix_w).max(1);
    let indent = " ".repeat(prefix_w);
    let chip = prefix.trim_end();
    let pad = &prefix[chip.len()..];
    let chip_style = prefix_style.add_modifier(Modifier::REVERSED | Modifier::BOLD);
    let mut first = true;
    for segment in wrap_styled_cells(chars, body_w) {
        let mut line = TranscriptLineBuilder::new(prefix.len() + segment.len());
        if first {
            if !chip.is_empty() {
                line.push_str(chip, chip_style);
            }
            if !pad.is_empty() {
                line.push_str(pad, prefix_style);
            }
        } else {
            line.push_str(&indent, prefix_style);
        }
        for &(ch, style) in &segment {
            line.push_char(ch, style);
        }
        out.push(line.finish());
        first = false;
    }
}

/// Markdown-lite styling for assistant replies — the subset Gemini actually emits:
/// `**bold**`, `` `code` ``, `-`/`*` bullets, and `#` headings. Markers are stripped
/// from the rendered text (and therefore from drag-copy). Anything unclosed stays
/// literal, so a title like "*ハロー、プラネット。" is never eaten.
fn markdown_styled_chars(text: &str, base: Style, accent: Style) -> Vec<(char, Style)> {
    let bold = base.add_modifier(Modifier::BOLD);
    let mut out = Vec::new();
    let mut first_line = true;
    for line in text.split('\n') {
        if !first_line {
            out.push(('\n', base));
        }
        first_line = false;
        let trimmed = line.trim_start();
        let lead = &line[..line.len() - trimmed.len()];
        out.extend(lead.chars().map(|c| (c, base)));

        // Heading: `#`s + a space → the hashes vanish, the line renders bold.
        let hashes = trimmed.chars().take_while(|&c| c == '#').count();
        if hashes > 0
            && let Some(heading) = trimmed[hashes..].strip_prefix(' ')
        {
            inline_styled_chars(heading, bold, bold, accent, &mut out);
            continue;
        }
        // Bullet: `- ` / `* ` → an accent-colored `•`.
        if let Some(item) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
        {
            out.push(('•', accent));
            out.push((' ', base));
            inline_styled_chars(item, base, bold, accent, &mut out);
            continue;
        }
        inline_styled_chars(trimmed, base, bold, accent, &mut out);
    }
    out
}

/// Inline spans within one line: `**bold**` and `` `code` `` (code renders in the
/// accent color). Unclosed or empty markers fall through as literal text.
fn inline_styled_chars(
    text: &str,
    base: Style,
    bold: Style,
    code: Style,
    out: &mut Vec<(char, Style)>,
) {
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '`'
            && let Some(close) = (i + 1..chars.len()).find(|&j| chars[j] == '`')
            && close > i + 1
        {
            out.extend(chars[i + 1..close].iter().map(|&c| (c, code)));
            i = close + 1;
            continue;
        }
        if chars[i] == '*'
            && chars.get(i + 1) == Some(&'*')
            && let Some(close) = (i + 2..chars.len().saturating_sub(1))
                .find(|&j| chars[j] == '*' && chars[j + 1] == '*')
            && close > i + 2
        {
            out.extend(chars[i + 2..close].iter().map(|&c| (c, bold)));
            i = close + 2;
            continue;
        }
        out.push((chars[i], base));
        i += 1;
    }
}

/// Plain-text wrapper kept as the styled wrap's shadow so the original wrapping tests
/// keep pinning the shared behavior (word wrap, whitespace collapse, long-word split).
#[cfg(test)]
fn wrap_text_cells(text: &str, width: usize) -> Vec<String> {
    let chars: Vec<(char, Style)> = text.chars().map(|c| (c, Style::default())).collect();
    wrap_styled_cells(&chars, width)
        .into_iter()
        .map(|seg| seg.into_iter().map(|(c, _)| c).collect())
        .collect()
}

/// Word-wrap styled chars to `width` display cells: paragraphs split on `\n`, runs of
/// whitespace collapse to one space, and a word longer than the width splits at the
/// cell boundary — the exact behavior the plain wrapper always had, carried per char so
/// markdown runs survive wrapping.
fn wrap_styled_cells(chars: &[(char, Style)], width: usize) -> Vec<Vec<(char, Style)>> {
    let width = width.max(1);
    let mut out = Vec::new();
    for para in chars.split(|(c, _)| *c == '\n') {
        if para.iter().all(|(c, _)| c.is_whitespace()) {
            out.push(Vec::new());
            continue;
        }
        let mut cur: Vec<(char, Style)> = Vec::new();
        let mut cur_w = 0usize;
        for word in para
            .split(|(c, _)| c.is_whitespace())
            .filter(|w| !w.is_empty())
        {
            let word_w: usize = word
                .iter()
                .map(|(c, _)| UnicodeWidthChar::width(*c).unwrap_or(0))
                .sum();
            if cur.is_empty() {
                push_word_wrapped(&mut out, &mut cur, &mut cur_w, word, width);
            } else if cur_w + 1 + word_w <= width {
                // The joining space carries the upcoming word's leading style (it's
                // invisible either way — this just keeps runs mergeable).
                cur.push((' ', word[0].1));
                cur.extend_from_slice(word);
                cur_w += 1 + word_w;
            } else {
                out.push(std::mem::take(&mut cur));
                cur_w = 0;
                push_word_wrapped(&mut out, &mut cur, &mut cur_w, word, width);
            }
        }
        if !cur.is_empty() {
            out.push(cur);
        }
    }
    out
}

fn push_word_wrapped(
    out: &mut Vec<Vec<(char, Style)>>,
    cur: &mut Vec<(char, Style)>,
    cur_w: &mut usize,
    word: &[(char, Style)],
    width: usize,
) {
    for &(ch, style) in word {
        let ch_w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if *cur_w > 0 && cur_w.saturating_add(ch_w) > width {
            out.push(std::mem::take(cur));
            *cur_w = 0;
        }
        cur.push((ch, style));
        *cur_w += ch_w;
    }
}

fn register_transcript_rows(app: &App, area: Rect, offset: usize, count: usize) {
    for vis in 0..area.height {
        let item = offset + vis as usize;
        if item >= count {
            break;
        }
        app.register_mouse_button(
            Rect {
                x: area.x,
                y: area.y + vis,
                width: area.width,
                height: 1,
            },
            MouseTarget::AiTranscriptRow(item),
        );
    }
}

fn render_suggestions(frame: &mut Frame, app: &App, area: Rect) {
    let focused = app.ai.focus == AiFocus::Suggestions;
    let border = if focused {
        R::BorderFocused
    } else {
        R::BorderMuted
    };
    let block = Block::default()
        .title(t!(" Suggestions ", " 추천 곡 "))
        .borders(Borders::TOP)
        .border_style(app.theme.style(border))
        .style(app.theme.style(R::TextPrimary));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // The wheel scrolls this viewport freely; the render keeps a keyboard-moved cursor on
    // screen with a margin (see `ui::scroll`). Resolved before the rows are built because
    // the selection only highlights — and only marquees — while actually inside the window.
    let len = app.ai.suggestions.len();
    let offset = app.bridges.ai_scroll.resolve(
        app.ai.suggestions_selected.min(len.saturating_sub(1)),
        inner.height,
        len,
        crate::ui::scroll::SCROLLOFF,
    );
    let visible_sel = (len > 0)
        .then(|| app.ai.suggestions_selected.min(len - 1))
        .filter(|sel| (offset..offset + inner.height as usize).contains(sel));

    let items: Vec<ListItem> = app
        .ai
        .suggestions
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let title = app.display_title(s);
            let artist = app.display_artist(s);
            // Fixed-width heart slot (like the library lists) so favoriting a row never
            // shifts its title relative to its neighbors.
            let heart = if app.library.is_favorite(&s.video_id) {
                "♥ "
            } else {
                "  "
            };
            let text = format!("{title} — {artist}");
            // The focused, visible cursor row marquees when clipped (the heart gutter
            // stays put); the extra column keeps it clear of the overlaid scrollbar.
            let text = if focused && visible_sel == Some(i) {
                crate::ui::anim::selected_marquee(
                    app,
                    ScrollSurface::AiSuggestions,
                    i,
                    &text,
                    (inner.width as usize).saturating_sub(2 + 2 + 1),
                )
            } else {
                text
            };
            ListItem::new(format!("{heart}{text}")).style(app.theme.style(R::TextPrimary))
        })
        .collect();

    let highlight = if focused {
        crate::ui::anim::selection_style(
            app,
            Style::default()
                .fg(app.theme.color(R::SelectionFg))
                .bg(app.theme.color(R::SelectionBg))
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Style::default()
            .fg(app.theme.color(R::SelectionInactiveFg))
            .bg(app.theme.color(R::SelectionInactiveBg))
    };
    let list = List::new(items)
        .style(app.theme.style(R::TextPrimary))
        .highlight_style(highlight)
        .highlight_symbol("▶ ")
        // Reserve the ▶ gutter even while the selection is scrolled off-view
        // (`state.select` below is skipped then) — otherwise every visible row
        // shifts 2 cells left whenever the wheel moves the cursor out of the
        // viewport. Mirrors the Search results and Settings lists.
        .highlight_spacing(HighlightSpacing::Always);

    // Pre-seed the wheel offset so ratatui honors it; only highlight the selection while
    // it is actually visible, so the wheel can scroll past it.
    let mut state = ListState::default().with_offset(offset);
    if let Some(sel) = visible_sel {
        state.select(Some(sel));
    }
    frame.render_stateful_widget(list, inner, &mut state);
    register_suggestion_rows(app, inner, state.offset(), len);
    // Scrollbar on the right border, tracking the viewport position; hidden when it all fits.
    buttons::render_list_scrollbar(
        frame,
        app,
        Rect {
            x: inner.x.saturating_add(inner.width.saturating_sub(1)),
            y: inner.y,
            width: 1,
            height: inner.height,
        },
        ScrollSurface::AiSuggestions,
        len,
        state.offset(),
        inner.height as usize,
    );
}

fn register_suggestion_rows(app: &App, area: Rect, offset: usize, count: usize) {
    for vis in 0..area.height {
        let item = offset + vis as usize;
        if item >= count {
            break;
        }
        app.register_mouse_button(
            Rect {
                x: area.x,
                y: area.y + vis,
                width: area.width,
                height: 1,
            },
            MouseTarget::AiSuggestionRow(item),
        );
    }
}

fn render_input(frame: &mut Frame, app: &App, area: Rect) {
    let cols =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(AI_SEND_BTN_W)]).split(area);
    let input_area = cols[0];
    let button_area = cols[1];

    let focused = app.ai.focus == AiFocus::Input;
    let border = if focused {
        R::BorderFocused
    } else {
        R::BorderMuted
    };
    let block = Block::default()
        .title(t!(" Ask ", " 질문 "))
        .borders(Borders::ALL)
        .border_style(app.theme.style(border))
        .style(app.theme.style(R::TextPrimary));
    // Ctrl+A selects the whole prompt: paint it with the selection colors. Otherwise show a
    // trailing block cursor while focused, or plain text when not.
    let para = if focused && app.ai.select_all && !app.ai.input.is_empty() {
        let hl = Style::default()
            .fg(app.theme.color(R::SelectionFg))
            .bg(app.theme.color(R::SelectionBg));
        Paragraph::new(Line::from(Span::styled(app.ai.input.clone(), hl)))
    } else {
        if focused {
            // The caret is its own span so the caret animation can blink it (the plain solid
            // block in the text's own style when that flag is off, exactly as before).
            let caret = crate::ui::anim::caret_span(
                app,
                app.theme.style(R::TextPrimary),
                app.theme.color(R::Background),
            );
            Paragraph::new(Line::from(vec![
                Span::styled(app.ai.input.clone(), app.theme.style(R::TextPrimary)),
                caret,
            ]))
        } else {
            Paragraph::new(app.ai.input.clone()).style(app.theme.style(R::TextPrimary))
        }
    };
    frame.render_widget(para.block(block), input_area);
    app.register_mouse_button(input_area, MouseTarget::AiInput);
    render_send_button(frame, app, button_area);
}

const AI_SEND_BTN_W: u16 = 8;

fn render_send_button(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(app.theme.style(R::BorderMuted))
        .style(app.theme.style(R::TextPrimary));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let label = Line::from(t!("Send", "전송"))
        .style(app.theme.style(R::Accent).add_modifier(Modifier::BOLD))
        .alignment(Alignment::Center);
    frame.render_widget(Paragraph::new(label), inner);
    app.register_mouse_button(area, MouseTarget::AiSubmit);
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::style::Color;

    use super::*;
    use crate::app::{AiMessage, AiMsg, Msg};

    #[test]
    fn wraps_on_words_when_possible() {
        assert_eq!(
            wrap_text_cells("alpha beta gamma", 10),
            vec!["alpha beta".to_owned(), "gamma".to_owned()]
        );
    }

    #[test]
    fn splits_words_that_exceed_the_width() {
        let lines = wrap_text_cells("abcdefgh", 3);
        assert_eq!(lines, vec!["abc", "def", "gh"]);
        assert!(
            lines
                .iter()
                .all(|line| UnicodeWidthStr::width(line.as_str()) <= 3)
        );
    }

    #[test]
    fn empty_state_renders_generated_dj_gem_asset() {
        let app = App::new(100);
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| render(frame, &app, frame.area()))
            .unwrap();

        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("DJ"));
        assert!(text.contains("GEM"));
        assert!(text.contains("Settings under the DJ Gem tab"));
        assert!(!text.contains("press ,"));
    }

    #[test]
    fn unchanged_transcript_reuses_wrapped_and_copy_storage() {
        let mut app = App::new(100);
        app.ai.messages.push(AiMessage {
            role: AiRole::Ai,
            text: "**cached** transcript line with enough words to wrap".to_owned(),
        });

        let (first_lines, first_copy) = cached_transcript_lines(&app, 18);
        let (same_lines, same_copy) = cached_transcript_lines(&app, 18);
        assert!(Arc::ptr_eq(&first_lines, &same_lines));
        assert!(Arc::ptr_eq(&first_copy, &same_copy));

        app.ai.messages[0].text.push('!');
        let (changed_lines, changed_copy) = cached_transcript_lines(&app, 18);
        assert!(!Arc::ptr_eq(&first_lines, &changed_lines));
        assert!(!Arc::ptr_eq(&first_copy, &changed_copy));
    }

    #[test]
    fn copy_bridge_shares_every_transcript_line_payload_arc() {
        let mut app = App::new(100);
        app.ai.messages.push(AiMessage {
            role: AiRole::Ai,
            text: "**shared** UTF-8 줄 with enough words to wrap".to_owned(),
        });

        let (lines, copy_lines) = cached_transcript_lines(&app, 16);
        assert_eq!(lines.len(), copy_lines.len());
        for (line, copy) in lines.iter().zip(copy_lines.iter()) {
            assert!(Arc::ptr_eq(&line.text, copy));
        }
    }

    #[test]
    fn reducer_append_advances_revision_and_rebuilds_cached_transcript() {
        let mut app = App::new(100);
        let before_revision = app.ai.transcript_revision;
        let _ = app.update(Msg::Ai(AiMsg::Chat("first reply".to_owned())));
        assert_eq!(app.ai.transcript_revision, before_revision.wrapping_add(1));
        let (first_lines, first_copy) = cached_transcript_lines(&app, 18);

        let _ = app.update(Msg::Ai(AiMsg::Chat("second reply".to_owned())));
        let (changed_lines, changed_copy) = cached_transcript_lines(&app, 18);
        assert!(!Arc::ptr_eq(&first_lines, &changed_lines));
        assert!(!Arc::ptr_eq(&first_copy, &changed_copy));
    }

    #[test]
    fn transcript_cache_invalidates_for_presentation_and_app_identity() {
        let mut app = App::new(100);
        app.ai.messages.push(AiMessage {
            role: AiRole::Ai,
            text: "one transcript owned by one app".to_owned(),
        });
        let (first_lines, _) = cached_transcript_lines(&app, 18);

        let (different_width, _) = cached_transcript_lines(&app, 19);
        assert!(!Arc::ptr_eq(&first_lines, &different_width));

        app.ai.thinking = true;
        let (with_thinking, _) = cached_transcript_lines(&app, 19);
        assert!(!Arc::ptr_eq(&different_width, &with_thinking));

        let other_app = App::new(100);
        let (other_owner, _) = cached_transcript_lines(&other_app, 19);
        assert!(!Arc::ptr_eq(&with_thinking, &other_owner));
    }

    fn plain_text(chars: &[(char, Style)]) -> String {
        chars.iter().map(|(c, _)| c).collect()
    }

    #[test]
    fn markdown_strips_markers_and_styles_runs() {
        let base = Style::default();
        let accent = Style::default().fg(Color::Cyan);
        let bold = base.add_modifier(Modifier::BOLD);
        let out = markdown_styled_chars("a **bold** and `code` end", base, accent);
        assert_eq!(plain_text(&out), "a bold and code end");
        let style_of = |needle: char| out.iter().find(|(c, _)| *c == needle).unwrap().1;
        assert_eq!(style_of('b'), bold);
        assert_eq!(style_of('o'), bold);
        assert_eq!(style_of('c'), accent);
        assert_eq!(style_of('a'), base);
    }

    #[test]
    fn markdown_leaves_unclosed_markers_literal() {
        let base = Style::default();
        let accent = Style::default().fg(Color::Cyan);
        // The M3 identify examples contain exactly this shape — it must never be eaten.
        let out = markdown_styled_chars("*ハロー、プラネット。", base, accent);
        assert_eq!(plain_text(&out), "*ハロー、プラネット。");
        let out = markdown_styled_chars("a ** b and `tick", base, accent);
        assert_eq!(plain_text(&out), "a ** b and `tick");
        // Empty markers stay literal too.
        let out = markdown_styled_chars("**** and ``", base, accent);
        assert_eq!(plain_text(&out), "**** and ``");
    }

    #[test]
    fn markdown_renders_bullets_and_headings() {
        let base = Style::default();
        let accent = Style::default().fg(Color::Cyan);
        let out = markdown_styled_chars("## Head\n- item one\n* item two\n#tag", base, accent);
        assert_eq!(plain_text(&out), "Head\n• item one\n• item two\n#tag");
        // Heading text is bold; the bullet dot carries the accent.
        assert_eq!(
            out.iter().find(|(c, _)| *c == 'H').unwrap().1,
            base.add_modifier(Modifier::BOLD)
        );
        assert_eq!(out.iter().find(|(c, _)| *c == '•').unwrap().1, accent);
    }

    #[test]
    fn transcript_chips_roles_separates_messages_and_strips_markdown() {
        let mut app = App::new(100);
        app.ai.messages.push(AiMessage {
            role: AiRole::User,
            text: "who sings this?".to_owned(),
        });
        app.ai.messages.push(AiMessage {
            role: AiRole::Ai,
            text: "It's **YOASOBI**.".to_owned(),
        });
        let lines = build_transcript_lines(&app, 60);

        // Message, blank separator, message.
        assert_eq!(lines.len(), 3);
        assert!(lines[1].text.is_empty() && lines[1].styles.is_empty());

        // The role tag renders as a reversed chip; its text stays in the copy line.
        assert_eq!(lines[0].text.as_ref(), "you who sings this?");
        let first_range = &lines[0].styles[0];
        assert_eq!(&lines[0].text[first_range.start..first_range.end], "you");
        assert!(first_range.style.add_modifier.contains(Modifier::REVERSED));

        // Markdown markers are stripped from both the render and the drag-copy text.
        assert_eq!(lines[2].text.as_ref(), "Gem It's YOASOBI.");
        assert!(lines[2].styles.iter().any(|range| {
            lines[2].text[range.start..range.end].contains("YOASOBI")
                && range.style.add_modifier.contains(Modifier::BOLD)
        }));
        // Byte ranges are contiguous, valid UTF-8 slices, and cover the one text payload.
        for line in &lines {
            let mut next = 0;
            let joined: String = line
                .styles
                .iter()
                .map(|range| {
                    assert_eq!(range.start, next);
                    next = range.end;
                    &line.text[range.start..range.end]
                })
                .collect();
            assert_eq!(next, line.text.len());
            assert_eq!(joined, line.text.as_ref());
        }
    }

    #[test]
    fn transcript_styled_wrap_keeps_hanging_indent() {
        let mut app = App::new(100);
        app.ai.messages.push(AiMessage {
            role: AiRole::Ai,
            text: "**alpha beta** gamma".to_owned(),
        });
        // Width 14 = 4 ("Gem " chip+pad) + 10 body cells: the bold pair fills line one,
        // and the continuation hang-indents under the body, not under the chip.
        let lines = build_transcript_lines(&app, 14);
        assert_eq!(lines[0].text.as_ref(), "Gem alpha beta");
        assert_eq!(lines[1].text.as_ref(), "    gamma");
    }
}
