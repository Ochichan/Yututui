//! The About card: a small, centered overlay showing the app icon, a one-line description,
//! the MIT license, and a clickable link to the GitHub repo. Opened by clicking the `ytm-tui`
//! brand in the nav bar or pressing F1 ([`crate::keymap::Action::ToggleAbout`]); dismissed by
//! Esc / F1 / any click that isn't the link. Laid out like the `?` cheat-sheet and styled with
//! the same theme roles so it matches whatever preset/overrides are active.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui_image::picker::Picker;
use ratatui_image::{Resize, StatefulImage};

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
const ICON_PNG: &[u8] = include_bytes!("../../../assets/icons/ytm-tui.png");

/// How many rows the icon band gets at the top of the card.
const ICON_ROWS: u16 = 9;

/// Render the About card as a centered popup over `area`.
pub fn render(frame: &mut Frame, app: &App, area: Rect) {
    let popup = centered_fixed(area, 60, 22);
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(t!(" About ", " 정보 "))
        .borders(Borders::ALL)
        .border_style(app.theme.style(R::BorderPrimary))
        .style(app.theme.style(R::TextPrimary));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let rows = Layout::vertical([Constraint::Length(ICON_ROWS), Constraint::Min(0)]).split(inner);
    draw_icon(frame, app, rows[0]);
    draw_text(frame, app, rows[1]);
}

/// Draw the embedded icon centered in `band`, building (and caching) its protocol on first use.
fn draw_icon(frame: &mut Frame, app: &App, band: Rect) {
    ensure_icon(app);
    let mut guard = app.about_icon.borrow_mut();
    let Some(proto) = guard.as_mut() else { return };

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
    frame.render_stateful_widget(StatefulImage::new().resize(Resize::Fit(None)), rect, proto);
}

/// Decode the embedded PNG and build its render protocol once, caching it on the app.
///
/// Prefer the terminal's detected graphics protocol (`art_picker` — Kitty/Sixel) so the icon
/// renders at full resolution like album art, instead of the blocky ~2px-per-cell half-blocks.
/// Fall back to half-blocks when no picker was probed (album art off) or the terminal has no
/// graphics support. Closing the card over album art repaints cleanly: `about_visible` is part
/// of [`crate::app::App::art_overlay_mask`], so the main loop rebuilds the art (re-emitting every
/// row) whenever the card toggles — the same dance the eq/radio/queue popups already rely on.
fn ensure_icon(app: &App) {
    let needs_build = app.about_icon.borrow().is_none();
    if needs_build && let Ok(img) = image::load_from_memory(ICON_PNG) {
        let proto = match app.art.picker.as_ref() {
            Some(picker) => picker.new_resize_protocol(img),
            None => Picker::halfblocks().new_resize_protocol(img),
        };
        *app.about_icon.borrow_mut() = Some(proto);
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

    // Name + version.
    let name = Line::from(vec![
        Span::styled(
            "ytm-tui",
            app.theme.style(R::Accent).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  v{}", env!("CARGO_PKG_VERSION")),
            app.theme.style(R::TextMuted),
        ),
    ]);
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
            .style(app.theme.style(R::TextMuted)),
        rows[1],
    );

    // Cheat-sheet-style key/value rows — same theme roles as the `?` help sheet so they match.
    let info = [
        (t!("Command", "명령어"), "ytt"),
        (t!("License", "라이선스"), "MIT · © 2026 Ochichan"),
        (t!("Author", "제작자"), "Ochichan"),
        (t!("Built", "빌드"), "Rust · ratatui"),
    ];
    let key_style = app.theme.style(R::HelpKey);
    let val_style = app.theme.style(R::HelpAction);
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
            .style(app.theme.style(R::TextMuted)),
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
