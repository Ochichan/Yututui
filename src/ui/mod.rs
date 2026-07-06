//! Top-level rendering: owns the screen layout and dispatches to the active view.

pub mod anim;
pub mod ascii_art;
pub mod buttons;
pub mod retro;
pub mod scroll;
pub mod text;
pub mod views;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Clear};

use crate::app::{App, Mode};
use crate::theme::ThemeRole as R;

pub fn render(frame: &mut Frame, app: &App) {
    app.clear_mouse_regions();
    let area = frame.area();
    frame.render_widget(
        Block::default().style(
            Style::default()
                .fg(app.theme.color(R::TextPrimary))
                .bg(app.theme.color(R::Background)),
        ),
        area,
    );
    // Album art is a per-frame output: only the player view draws it (and records its rect).
    // Clear the rect up front so it reflects *this* frame — otherwise a stale rect from the last
    // player frame survives into Search/Library/etc., where `mark_art_rows_for_popup` would
    // re-anchor the (still-transmitted) kitty image under a popup and bleed it through as a stray
    // vertical bar. Player mode re-sets it below before any popup reads it.
    app.art.rect.set(None);
    // The selected-row marquee flag is a per-frame output too: a list view sets it while it
    // renders a scrolling cursor row; `animation_active` reads the latest frame's verdict.
    app.bridges.marquee_ran.set(false);
    match app.mode {
        Mode::Player => views::player::render(frame, app, area),
        Mode::Search => views::search::render(frame, app, area),
        Mode::Library => views::library::render(frame, app, area),
        Mode::Settings => views::settings::render(frame, app, area),
        Mode::Ai => views::ai::render(frame, app, area),
    }
    // The `?` cheat-sheet draws on top of whatever screen is active.
    if app.help_visible {
        views::help::render(frame, app, area);
    }
    // The mouse cheat-sheet is opened only from the footer mouse icon.
    if app.mouse_help_visible {
        views::help::render_mouse(frame, app, area);
    }
    // The About card draws on top too (clicking the `ytm-tui` brand or F1).
    if app.about_visible {
        views::about::render(frame, app, area);
    }
    // The "Why DJ Gem" card explains the last DJ Gem streaming refill (`w`); also drawn on top.
    if app.why_ai_visible {
        views::why_ai::render(frame, app, area);
    }
    // The "what's playing" identify card (radio): the result + favorite / ask-DJ Gem
    // actions. Below the mode confirmations so those stay on top.
    if app.now_playing_overlay.is_some() {
        views::now_playing::render(frame, app, area);
    }
    // Radio mode switching is a global UI-mode confirmation.
    if let Some(confirm) = app.radio_mode.pending_radio_mode_confirm {
        views::player::render_radio_mode_confirm(frame, app, area, confirm);
    }
    // A keybinding-conflict warning (Keys tab) is modal — it sits above everything else.
    if let Some(conflict) = &app.key_conflict {
        views::settings::render_conflict(frame, app, area, conflict);
    }
    // Settings confirmations are likewise modal.
    if let Some(confirm) = app.pending_settings_confirm {
        views::settings::render_confirm(frame, app, area, confirm);
    }
    // The Spotify playlist picker (Import from Spotify…) is modal over Settings.
    if app.spotify_picker.is_some() {
        views::settings::render_spotify_picker(frame, app, area);
    }
    // The radio-recording settings popup is modal over the Playback tab.
    if app.recording_settings.is_some() {
        views::settings::render_recording_settings(frame, app, area);
    }
    // The create-playlist popup captures Library input while open.
    if app.library_ui.create_input.is_some() {
        views::library::render_playlist_create(frame, app, area);
    }
    // Deleting a whole playlist is modal, like the file delete below.
    if app.library_ui.confirm_playlist_delete.is_some() {
        views::library::render_confirm_playlist_delete(frame, app, area);
    }
    // Deleting downloaded files is modal and irreversible — drawn last so its buttons win.
    if app.library_ui.confirm_delete.is_some() {
        views::library::render_confirm_delete(frame, app, area);
    }
    // The bulk-download confirmation floats over the library like the delete modals (mutually
    // exclusive with them, so relative order doesn't matter).
    if app.library_ui.confirm_download.is_some() {
        views::library::render_confirm_download(frame, app, area);
    }
    // The add-to-playlist picker floats over whichever screen opened it.
    if app.playlist_picker.is_some() {
        views::library::render_playlist_picker(frame, app, area);
    }
    // The recordings browser floats over the player or the recording-settings popup — drawn
    // last so it sits on top of whatever opened it.
    if app.recordings_browser.is_some() {
        views::settings::render_recordings_browser(frame, app, area);
    }
    retro::scrub_frame(frame, app);
}

/// A centered popup sized to the queue-window proportions — about 3/5 of `area` wide and
/// 7/10 tall — tall enough for `body_rows` list rows plus `chrome_rows` of border/input/hint,
/// never narrower than `min_w`. Clamped so it always fits inside `area`. Shared by the queue
/// window and the search results-filter popup so the two modal lists keep the same geometry.
pub fn centered_list_popup(area: Rect, body_rows: usize, chrome_rows: u16, min_w: u16) -> Rect {
    let max_w = area.width.saturating_sub(2).max(24);
    // Saturating math + a checked row-count cast: a huge terminal (`width * 3` overflows u16
    // above ~21845 cols) or a giant list (`body_rows as u16` would truncate) must not panic.
    let box_w = (area.width.saturating_mul(3) / 5).clamp(min_w.min(max_w), max_w);
    let max_h = (area.height.saturating_mul(7) / 10).max(chrome_rows + 1);
    let body = u16::try_from(body_rows).unwrap_or(u16::MAX);
    let box_h = body.saturating_add(chrome_rows).min(max_h);
    Rect {
        x: area.x + area.width.saturating_sub(box_w) / 2,
        y: area.y + area.height.saturating_sub(box_h) / 2,
        width: box_w,
        height: box_h,
    }
    .intersection(area)
}

pub fn popup_bg(app: &App) -> Color {
    match app.theme.color(R::Background) {
        Color::Reset => app.theme.color(R::TextInverse),
        bg => bg,
    }
}

pub fn popup_style(app: &App, role: R) -> Style {
    app.theme.style(role).bg(popup_bg(app))
}

pub fn confirm_border_style(app: &App) -> Style {
    popup_style(app, R::BorderPrimary).add_modifier(ratatui::style::Modifier::BOLD)
}

pub fn confirm_button_style(app: &App) -> Style {
    popup_style(app, R::Accent).add_modifier(ratatui::style::Modifier::BOLD)
}

pub fn confirm_gap_style(app: &App) -> Style {
    popup_style(app, R::TextMuted)
}

pub fn render_popup_background(frame: &mut Frame, app: &App, area: Rect) {
    frame.render_widget(Clear, area);
    frame.render_widget(
        Block::default().style(Style::default().bg(popup_bg(app))),
        area,
    );
}

pub fn seal_popup_background(frame: &mut Frame, app: &App, area: Rect) {
    let bg = popup_bg(app);
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            if let Some(cell) = frame.buffer_mut().cell_mut((x, y))
                && cell.bg == Color::Reset
            {
                cell.set_bg(bg);
            }
        }
    }
    // Every popup ends its render here, so this one call gives all of them the fade-in
    // materialize (a no-op unless the popup-fade window is running — see `App::detect_fx`).
    // After the seal above, every cell has a concrete background to blend from.
    anim::popup_fade_overlay(frame, app, area);
}

pub fn mark_art_rows_for_popup(_frame: &mut Frame, _app: &App, _popup: Rect) {
    // Popup/native-image synchronization is handled by `App::sync_art_overlay_state`, which asks
    // the next overlay transition frame to clear and redraw. Re-planting Kitty row anchors here is
    // harmful with the explicit per-cell placeholder patch in `crates/ratatui-image`: an interior
    // popup can replace a full image row with a single marker and make the album art look sliced at
    // the popup edges.
}
