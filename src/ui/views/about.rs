//! The About card: a small, centered overlay showing the app icon, a one-line description,
//! the MIT license, and a clickable link to the GitHub repo. Opened by clicking the `ytm-tui`
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
pub const GITHUB_URL: &str = "https://github.com/Ochichan/ytm-tui";
/// The link text rendered in the card (the `https://` prefix is dropped to keep it compact).
const GITHUB_LABEL: &str = "github.com/Ochichan/ytm-tui";

/// The app icon, embedded at compile time so the card renders it no matter where (or how) the
/// binary is launched — there's no assets dir to find at runtime.
const ICON_PNG: &[u8] = include_bytes!("../../../assets/icons/ytm-tui-about.png");

/// How many rows the icon band gets at the top of the card.
const ICON_ROWS: u16 = 9;
/// The About icon is foreground UI, unlike album art, which is deliberately pushed behind text.
const ABOUT_ICON_KITTY_Z_INDEX: i32 = 0;

/// Render the About card as a centered popup over `area`.
pub fn render(frame: &mut Frame, app: &App, area: Rect) {
    let popup = centered_fixed(area, 60, 22);
    crate::ui::render_popup_background(frame, app, popup);

    let block = Block::default()
        .title(t!(" About ", " 정보 "))
        .borders(Borders::ALL)
        .border_style(crate::ui::popup_style(app, R::BorderPrimary))
        .style(crate::ui::popup_style(app, R::TextPrimary));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let rows = Layout::vertical([Constraint::Length(ICON_ROWS), Constraint::Min(0)]).split(inner);
    let icon = draw_icon(frame, app, rows[0]);
    // Sparkles twinkle on the blank cells around the icon while the About animation is on
    // (never inside the icon's own rect — a native-graphics icon leaves its text cells
    // blank, so the empty-cell check alone can't protect it).
    crate::ui::anim::about_sparkles(frame, app, rows[0], icon);
    draw_text(frame, app, rows[1]);
    crate::ui::seal_popup_background(frame, app, popup);
    crate::ui::mark_art_rows_for_popup(frame, app, popup);
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
    let mut guard = app.about_icon.borrow_mut();
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
        app.about_icon
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
        *app.about_icon.borrow_mut() = Some((bg, target_protocol, proto));
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
fn draw_text(frame: &mut Frame, app: &App, area: Rect) {
    let rows = Layout::vertical([
        Constraint::Length(1), // name + version
        Constraint::Length(2), // description
        Constraint::Length(1), // spacer
        Constraint::Length(4), // info rows (Command / License / Author / Built)
        Constraint::Length(1), // GitHub link (clickable)
        Constraint::Length(1), // spacer
        Constraint::Min(1),    // close hint
    ])
    .split(area);

    // Name + version — with a gradient band sweeping the brand while the About animation is on.
    let name = crate::ui::anim::about_brand_line(app, "ytm-tui", env!("CARGO_PKG_VERSION"))
        .unwrap_or_else(|| {
            Line::from(vec![
                Span::styled(
                    "ytm-tui",
                    crate::ui::popup_style(app, R::Accent).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("  v{}", env!("CARGO_PKG_VERSION")),
                    crate::ui::popup_style(app, R::TextMuted),
                ),
            ])
        });
    frame.render_widget(Paragraph::new(name).alignment(Alignment::Center), rows[0]);

    // Description.
    let desc = vec![
        Line::from(t!(
            "A YouTube Music player that lives",
            "터미널에서 바로 실행되는"
        )),
        Line::from(t!("inside your terminal.", "YouTube Music 플레이어.")),
    ];
    frame.render_widget(
        Paragraph::new(desc)
            .alignment(Alignment::Center)
            .style(crate::ui::popup_style(app, R::TextMuted)),
        rows[1],
    );

    // Cheat-sheet-style key/value rows — same theme roles as the `?` help sheet so they match.
    let info = [
        (t!("Command", "명령어"), "ytt"),
        (t!("License", "라이선스"), "MIT · © 2026 Ochichan"),
        (t!("Author", "제작자"), "Ochichan"),
        (t!("Built", "빌드"), "Rust · ratatui"),
    ];
    let key_style = crate::ui::popup_style(app, R::HelpKey);
    let val_style = crate::ui::popup_style(app, R::HelpAction);
    let lines: Vec<Line> = info
        .iter()
        .map(|(k, v)| {
            // Pad to a 9-cell key column by *display* width so Korean keys line their values up.
            let pad = 9usize.saturating_sub(UnicodeWidthStr::width(*k));
            Line::from(vec![
                Span::styled(format!("  {k}{}", " ".repeat(pad)), key_style),
                Span::styled((*v).to_owned(), val_style),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), rows[3]);

    // GitHub row: the URL is a real click target that opens the repo in the system browser. The
    // leading label is padded to line its value up with the rows above (`  ` + 9-wide key).
    let link_style = app
        .theme
        .style(R::Accent)
        .bg(crate::ui::popup_bg(app))
        .add_modifier(Modifier::UNDERLINED);
    let segs = [
        Seg::label("  GitHub   "),
        Seg::button(MouseTarget::AboutLink, GITHUB_LABEL),
    ];
    buttons::render_segments(
        frame,
        app,
        rows[4],
        &segs,
        link_style,
        key_style,
        Alignment::Left,
    );

    // Close hint.
    let hint = Line::from(t!(
        "F1 / Esc to close · click the link to open ↗",
        "F1 / Esc 닫기 · 링크를 클릭해 열기 ↗"
    ));
    frame.render_widget(
        Paragraph::new(hint)
            .alignment(Alignment::Center)
            .style(crate::ui::popup_style(app, R::TextMuted)),
        rows[6],
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
