//! The About card: a small, centered overlay showing the app icon, a one-line description,
//! the MIT license, and a clickable link to the GitHub repo. Opened by clicking the `YuTuTui!`
//! brand in the nav bar or pressing F1 ([`crate::keymap::Action::ToggleAbout`]); dismissed by
//! Esc / F1 / any click that isn't the link. Laid out like the `?` cheat-sheet and styled with
//! the same theme roles so it matches whatever preset/overrides are active.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::{Resize, StatefulImage};

use image::imageops::FilterType;
use unicode_width::UnicodeWidthStr;

use crate::app::{App, MouseTarget};
use crate::t;
use crate::theme::ThemeRole as R;
use crate::ui::buttons::{self, Seg};

/// The project's public home. Shown as a link in the card and opened in the browser on click.
pub const GITHUB_URL: &str = "https://github.com/Ochichan/Yututui";
/// The link text rendered in the card (the `https://` prefix is dropped to keep it compact).
const GITHUB_LABEL: &str = "github.com/Ochichan/Yututui";

/// The app icon, embedded at compile time so the card renders it no matter where (or how) the
/// binary is launched — there's no assets dir to find at runtime.
const ICON_PNG: &[u8] = include_bytes!("../../../assets/icons/yututui-about.png");

/// How many rows the icon band gets at the top of the card.
const ICON_ROWS: u16 = 12;
const NORMAL_TEXT_ROWS: u16 = 13;
const UPDATE_TEXT_ROWS: u16 = 17;
/// Card width. The widest line (the close hint, 44 cells) keeps ~10 cells of side air.
const CARD_WIDTH: u16 = 66;
/// Display width of the key column in the info block (`Command` / `라이선스` …).
const KEY_COL: usize = 9;
/// The About icon is foreground UI, unlike album art, which is deliberately pushed behind text.
const ABOUT_ICON_KITTY_Z_INDEX: i32 = 0;

/// Render the About card as a centered popup over `area`.
pub fn render(frame: &mut Frame, app: &App, area: Rect) {
    // Grow the card by the update block's four rows only when there's an update to show.
    let height = if update_available(app) { 31 } else { 27 };
    let popup = centered_fixed(area, CARD_WIDTH, height);
    crate::ui::render_popup_background(frame, app, popup);

    let block = Block::default()
        .title(t!(" About ", " 정보 ", " 情報 "))
        .borders(Borders::ALL)
        .border_style(crate::ui::popup_style(app, R::BorderPrimary))
        .style(crate::ui::popup_style(app, R::TextPrimary));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let icon_rows = about_icon_rows(inner.height, update_available(app));
    let rows = Layout::vertical([Constraint::Length(icon_rows), Constraint::Min(0)]).split(inner);
    let icon = draw_icon(frame, app, rows[0]);
    // Sparkles twinkle on the blank cells around the icon while the About animation is on
    // (never inside the icon's own rect — a native-graphics icon leaves its text cells
    // blank, so the empty-cell check alone can't protect it).
    crate::ui::anim::about_sparkles(frame, app, rows[0], icon);
    draw_text(frame, app, rows[1]);
    crate::ui::seal_popup_background(frame, app, popup);
    crate::ui::mark_art_rows_for_popup(frame, app, popup);
}

fn about_icon_rows(inner_height: u16, update_available: bool) -> u16 {
    let text_rows = if update_available {
        UPDATE_TEXT_ROWS
    } else {
        NORMAL_TEXT_ROWS
    };
    inner_height.saturating_sub(text_rows).clamp(1, ICON_ROWS)
}

/// Draw the embedded icon centered in `band`, building (and caching) its protocol on first use.
/// Returns the rect the icon occupies so the sparkle overlay can keep clear of it.
fn draw_icon(frame: &mut Frame, app: &App, band: Rect) -> Rect {
    // Terminal cells are about half as wide as they are tall, so a square icon wants roughly
    // twice the columns of rows. `Resize::Fit` keeps the icon's aspect inside whatever rect we
    // hand it, so this only has to be in the right ballpark.
    let h = band.height.clamp(1, ICON_ROWS);
    let w = (h * 2).min(band.width);
    let rect = Rect {
        x: band.x + band.width.saturating_sub(w) / 2,
        y: band.y + band.height.saturating_sub(h) / 2,
        width: w,
        height: h,
    };
    // Retro mode: ASCII-art the icon instead of building an image protocol (see `draw_art`).
    if app.retro_mode() {
        crate::ui::ascii_art::render_png(
            frame,
            crate::ui::ascii_art::Slot::AboutIcon,
            "about-icon",
            ICON_PNG,
            rect,
        );
        return rect;
    }
    ensure_icon(app);
    let mut guard = app.overlays.about_icon.borrow_mut();
    let Some((_, _, proto)) = guard.as_mut() else {
        return rect;
    };
    frame.render_stateful_widget(
        StatefulImage::new().resize(Resize::Fit(Some(FilterType::Lanczos3))),
        rect,
        proto,
    );
    rect
}

/// Decode the embedded PNG and build its render protocol once, caching it on the app.
///
/// Use the detected native image protocol when available so the embedded PNG keeps pixel-level
/// detail inside the popup. Half-block fallback is still composited against the popup background
/// so transparent corners repaint cleanly on text-only terminals.
fn ensure_icon(app: &App) {
    let bg = crate::ui::popup_bg(app);
    let target_protocol = about_icon_protocol(app);
    let needs_build =
        app.overlays
            .about_icon
            .borrow()
            .as_ref()
            .is_none_or(|(cached_bg, cached_protocol, _)| {
                *cached_bg != bg || *cached_protocol != target_protocol
            });
    if needs_build && let Ok(img) = image::load_from_memory(ICON_PNG) {
        let proto = match target_protocol {
            Some(ProtocolType::Kitty) => {
                let mut picker = app
                    .art
                    .picker
                    .as_ref()
                    .cloned()
                    .unwrap_or_else(Picker::halfblocks);
                picker.set_background_color(Some(color_to_rgba(bg)));
                picker.new_resize_protocol_with_kitty_z_index(img, Some(ABOUT_ICON_KITTY_Z_INDEX))
            }
            Some(_) => {
                let mut picker = app
                    .art
                    .picker
                    .as_ref()
                    .cloned()
                    .unwrap_or_else(Picker::halfblocks);
                picker.set_background_color(Some(color_to_rgba(bg)));
                picker.new_resize_protocol(img)
            }
            _ => {
                let mut picker = Picker::halfblocks();
                picker.set_background_color(Some(color_to_rgba(bg)));
                picker.new_resize_protocol(img)
            }
        };
        *app.overlays.about_icon.borrow_mut() = Some((bg, target_protocol, proto));
    }
}

fn about_icon_protocol(app: &App) -> Option<ProtocolType> {
    // While text zoom is active the pixel protocols can't render on the scaled virtual
    // grid (their placeholder cells would stripe); `None` selects the halfblock icon,
    // which is text and scales with the rest of the popup. The cache key includes the
    // protocol, so toggling zoom rebuilds the icon automatically.
    if app.zoom.scale() > 1 {
        return None;
    }
    app.art
        .picker
        .as_ref()
        .map(|picker| picker.protocol_type())
        .filter(|protocol| *protocol != ProtocolType::Halfblocks)
}

fn color_to_rgba(color: Color) -> image::Rgba<u8> {
    let (r, g, b) = match color {
        Color::Reset => (255, 255, 255),
        Color::Black => (0, 0, 0),
        Color::Red => (205, 0, 0),
        Color::Green => (0, 205, 0),
        Color::Yellow => (205, 205, 0),
        Color::Blue => (0, 0, 238),
        Color::Magenta => (205, 0, 205),
        Color::Cyan => (0, 205, 205),
        Color::Gray => (229, 229, 229),
        Color::DarkGray => (127, 127, 127),
        Color::LightRed => (255, 0, 0),
        Color::LightGreen => (0, 255, 0),
        Color::LightYellow => (255, 255, 0),
        Color::LightBlue => (92, 92, 255),
        Color::LightMagenta => (255, 0, 255),
        Color::LightCyan => (0, 255, 255),
        Color::White => (255, 255, 255),
        Color::Rgb(r, g, b) => (r, g, b),
        Color::Indexed(i) => indexed_to_rgb(i),
    };
    image::Rgba([r, g, b, 255])
}

fn indexed_to_rgb(i: u8) -> (u8, u8, u8) {
    match i {
        0 => (0, 0, 0),
        1 => (205, 0, 0),
        2 => (0, 205, 0),
        3 => (205, 205, 0),
        4 => (0, 0, 238),
        5 => (205, 0, 205),
        6 => (0, 205, 205),
        7 => (229, 229, 229),
        8 => (127, 127, 127),
        9 => (255, 0, 0),
        10 => (0, 255, 0),
        11 => (255, 255, 0),
        12 => (92, 92, 255),
        13 => (255, 0, 255),
        14 => (0, 255, 255),
        15 => (255, 255, 255),
        16..=231 => {
            let i = i - 16;
            let r = (i / 36) % 6;
            let g = (i / 6) % 6;
            let b = i % 6;
            let to_val = |c: u8| if c == 0 { 0 } else { 55 + c * 40 };
            (to_val(r), to_val(g), to_val(b))
        }
        232..=255 => {
            let gray = 8 + (i - 232) * 10;
            (gray, gray, gray)
        }
    }
}

/// Draw the name, description, info rows, the clickable GitHub link, and the close hint.
/// Whether the About card should show the "update available" notice.
fn update_available(app: &App) -> bool {
    app.overlays
        .update_status
        .as_ref()
        .is_some_and(|s| s.available)
}

/// Push a constraint and return its row index, so every row below is addressed by a
/// recorded name instead of a magic number that shifts with the update block.
fn push_row(constraints: &mut Vec<Constraint>, c: Constraint) -> usize {
    constraints.push(c);
    constraints.len() - 1
}

/// The shared left pad that centers the key/value block (info rows + GitHub row) as a whole,
/// measured over the actual values so Korean keys and future edits stay centered.
fn kv_left_pad(inner_w: u16) -> usize {
    let widest = [
        "ytt",
        "MIT · © 2026 Ochichan",
        "Ochichan",
        "Rust · ratatui",
        GITHUB_LABEL,
    ]
    .iter()
    .map(|v| UnicodeWidthStr::width(*v))
    .max()
    .unwrap_or(0);
    (inner_w as usize).saturating_sub(KEY_COL + widest) / 2
}

fn draw_text(frame: &mut Frame, app: &App, area: Rect) {
    let has_update = update_available(app);

    // Layout: the update block's four rows are inserted between the info block and the
    // GitHub link only when an update is available, so the card's normal look is untouched.
    let mut constraints: Vec<Constraint> = Vec::new();
    let name_row = push_row(&mut constraints, Constraint::Length(1));
    push_row(&mut constraints, Constraint::Length(1)); // spacer
    let desc_row = push_row(&mut constraints, Constraint::Length(2));
    push_row(&mut constraints, Constraint::Length(1)); // spacer
    let info_row = push_row(&mut constraints, Constraint::Length(4));
    let section_row = push_row(&mut constraints, Constraint::Length(1)); // spacer / update rule
    let update_rows = if has_update {
        let headline = push_row(&mut constraints, Constraint::Length(1));
        let body = push_row(&mut constraints, Constraint::Length(1));
        let link = push_row(&mut constraints, Constraint::Length(1));
        push_row(&mut constraints, Constraint::Length(1)); // spacer
        Some((headline, body, link))
    } else {
        None
    };
    let github_row = push_row(&mut constraints, Constraint::Length(1));
    push_row(&mut constraints, Constraint::Length(1)); // spacer
    let close_row = push_row(&mut constraints, Constraint::Min(1));
    let rows = Layout::vertical(constraints).split(area);

    // Name + version — with a gradient band sweeping the brand while the About animation is on.
    let name = crate::ui::anim::about_brand_line(app, "YuTuTui!", env!("CARGO_PKG_VERSION"))
        .unwrap_or_else(|| {
            Line::from(vec![
                Span::styled(
                    "YuTuTui!",
                    crate::ui::popup_style(app, R::TextPrimary).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("  v{}", env!("CARGO_PKG_VERSION")),
                    crate::ui::popup_style(app, R::TextMuted),
                ),
            ])
        });
    frame.render_widget(
        Paragraph::new(name).alignment(Alignment::Center),
        rows[name_row],
    );

    // Description.
    let desc = vec![
        Line::from(t!(
            "A YouTube Music player that lives",
            "터미널에서 바로 실행되는",
            "ターミナルの中で動く"
        )),
        Line::from(t!(
            "inside your terminal.",
            "YouTube Music 플레이어.",
            "YouTube Musicプレイヤー。"
        )),
    ];
    frame.render_widget(
        Paragraph::new(desc)
            .alignment(Alignment::Center)
            .style(crate::ui::popup_style(app, R::TextMuted)),
        rows[desc_row],
    );

    // Cheat-sheet-style key/value rows — same theme roles as the `?` help sheet so they match.
    // The whole block is centered as a unit via the shared pad so it no longer hugs the left edge.
    let block_pad = " ".repeat(kv_left_pad(area.width));
    let info = [
        (t!("Command", "명령어", "コマンド"), "ytt"),
        (
            t!("License", "라이선스", "使用許諾"),
            "MIT · © 2026 Ochichan",
        ),
        (t!("Author", "제작자", "作者"), "Ochichan"),
        (t!("Built", "빌드", "ビルド"), "Rust · ratatui"),
    ];
    let key_style = crate::ui::popup_style(app, R::HelpKey);
    let val_style = crate::ui::popup_style(app, R::HelpAction);
    let lines: Vec<Line> = info
        .iter()
        .map(|(k, v)| {
            // Pad to a KEY_COL-cell key column by *display* width so Korean keys line up.
            let pad = KEY_COL.saturating_sub(UnicodeWidthStr::width(*k));
            Line::from(vec![
                Span::styled(format!("{block_pad}{k}{}", " ".repeat(pad)), key_style),
                Span::styled((*v).to_owned(), val_style),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), rows[info_row]);

    // Update notice (only when a newer release is available): a headline, the tailored
    // upgrade command/instruction for the detected install method, and a clickable
    // "Releases" link. A subtle centered rule in the spacer above marks it as its own section.
    if let Some((headline, body, link)) = update_rows {
        frame.render_widget(
            Paragraph::new(Line::from("─".repeat(28)))
                .alignment(Alignment::Center)
                .style(crate::ui::popup_style(app, R::TextSubtle)),
            rows[section_row],
        );
        draw_update_block(frame, app, rows[headline], rows[body], rows[link]);
    }

    // GitHub row: the URL is a real click target that opens the repo in the system browser. The
    // leading label is padded to line its value up with the info rows above.
    let link_style = app
        .theme
        .style(R::TextPrimary)
        .bg(crate::ui::popup_bg(app))
        .add_modifier(Modifier::UNDERLINED);
    let github_label = format!("{block_pad}{:<width$}", "GitHub", width = KEY_COL);
    let segs = [
        Seg::label(&github_label),
        Seg::button(MouseTarget::AboutLink, GITHUB_LABEL),
    ];
    buttons::render_segments(
        frame,
        app,
        rows[github_row],
        &segs,
        link_style,
        key_style,
        Alignment::Left,
    );

    // Close hint — the key chord pops in the help-key color, the rest stays muted.
    let hint = Line::from(vec![
        Span::styled("F1 / Esc", key_style),
        Span::styled(
            t!(
                " to close · click the link to open ↗",
                " 닫기 · 링크를 클릭해 열기 ↗",
                " で閉じる · リンクをクリックで開く ↗"
            ),
            crate::ui::popup_style(app, R::TextMuted),
        ),
    ]);
    frame.render_widget(
        Paragraph::new(hint).alignment(Alignment::Center),
        rows[close_row],
    );
}

/// The "update available" notice: headline, tailored upgrade command/note, and a
/// clickable Releases link. Only called when [`update_available`] is true.
fn draw_update_block(frame: &mut Frame, app: &App, headline: Rect, body: Rect, link: Rect) {
    let Some(status) = app.overlays.update_status.as_ref() else {
        return;
    };
    let ins = crate::update::update_instructions(status.method);

    // Headline — the one line that must stand out; Accent + bold, centered.
    // `↑` (U+2191) means "version upgrade"; `↗` (U+2197) is reserved for links that open
    // externally. Both stay in the text-presentation Arrows block — `⬆` (U+2B06) renders
    // as a double-width color emoji in common terminal font stacks and breaks centering.
    let head = match crate::i18n::current() {
        crate::i18n::Language::Korean => {
            format!("↑ 새 버전 v{} 사용 가능", status.latest_display())
        }
        crate::i18n::Language::Japanese => {
            format!("↑ 新バージョン v{} が利用可能", status.latest_display())
        }
        _ => format!("↑ New version v{} available", status.latest_display()),
    };
    frame.render_widget(
        Paragraph::new(Line::from(head))
            .alignment(Alignment::Center)
            .style(crate::ui::popup_style(app, R::Accent).add_modifier(Modifier::BOLD)),
        headline,
    );

    // Body — the exact command when the channel has one, else the short instruction.
    let body_text = match ins.command {
        Some(cmd) => format!("$ {cmd}"),
        None => ins.note.to_owned(),
    };
    frame.render_widget(
        Paragraph::new(Line::from(body_text))
            .alignment(Alignment::Center)
            .style(crate::ui::popup_style(app, R::HelpAction)),
        body,
    );

    // Releases link — a real click target (opens the latest release page in the browser).
    let link_style = app
        .theme
        .style(R::TextPrimary)
        .bg(crate::ui::popup_bg(app))
        .add_modifier(Modifier::UNDERLINED);
    let segs = [Seg::button(
        MouseTarget::AboutUpdateLink,
        t!("Releases ↗", "릴리스 ↗", "リリース ↗"),
    )];
    buttons::render_segments(
        frame,
        app,
        link,
        &segs,
        link_style,
        link_style,
        Alignment::Center,
    );
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

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::update::{InstallMethod, UpdateStatus};

    fn buffer_text(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    fn draw(app: &App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, app, frame.area()))
            .unwrap();
        buffer_text(&terminal)
    }

    #[test]
    fn normal_card_uses_the_external_link_arrow_only() {
        let app = App::new(100);
        let text = draw(&app, 90, 40);
        assert!(text.contains("YuTuTui!"));
        assert!(text.contains(GITHUB_LABEL));
        assert!(text.contains("↗"));
        assert!(!text.contains("⬆"));
        assert!(!text.contains("↑"));
        assert!(!text.contains("Releases"));
    }

    #[test]
    fn update_card_marks_the_upgrade_with_the_up_arrow() {
        let mut app = App::new(100);
        app.overlays.update_status = Some(UpdateStatus {
            current: "0.1.0".into(),
            latest: "v9.9.9".into(),
            available: true,
            first_seen: false,
            method: InstallMethod::Homebrew,
        });
        let text = draw(&app, 90, 40);
        assert!(text.contains("↑"));
        assert!(text.contains("9.9.9"));
        assert!(text.contains("Releases"));
        assert!(text.contains("↗"));
        assert!(!text.contains("⬆"));
    }

    #[test]
    fn tiny_terminal_renders_without_panic() {
        let app = App::new(100);
        let text = draw(&app, 40, 12);
        assert!(text.contains("YuTuTui!"));
    }

    #[test]
    fn kv_block_pad_centers_the_widest_row() {
        // Inner width 64 (66-wide card minus borders): 9-cell key column + the 27-cell
        // GitHub value must sit centered with equal-ish air on both sides.
        let pad = kv_left_pad(64);
        assert_eq!(
            pad,
            (64 - (KEY_COL + UnicodeWidthStr::width(GITHUB_LABEL))) / 2
        );
        // Degrades to zero, not underflow, when the card is clamped tiny.
        assert_eq!(kv_left_pad(10), 0);
    }
}
