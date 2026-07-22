//! The miniplayer: what the whole UI becomes below the mini tier thresholds (see
//! `ui::layout`). Modeled on the desktop tray panel's "Mini" skin — title/artist,
//! seek, transport — with no border, nav strip, or footer; every row is the shared
//! control-box renderer, so the controls stay clickable and byte-consistent.

use ratatui::Frame;
use ratatui::layout::Rect;

use crate::app::{App, Mode};
use crate::ui::control_box;

pub fn render(frame: &mut Frame, app: &App, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    // The player animation clock only ticks on the Player screen (the mode is retained
    // under the miniplayer for grow-back); other modes get the static forms.
    let animated = app.mode == Mode::Player;
    // Row budget, most important first: title, seekbar, transport, then a minimal status
    // line only when a 4th row exists. Any residual rows stay blank.
    // Build only rows that actually exist. Asking ratatui to solve four fixed one-row
    // constraints inside a 2–3-row terminal can collapse an earlier critical row; direct
    // offsets keep the documented title → seek → transport degradation order stable.
    let row = |offset: u16| Rect {
        x: area.x,
        y: area.y.saturating_add(offset),
        width: area.width,
        height: 1,
    };
    // Mini replaces the whole UI, so it is the one caller allowed to marquee the title
    // (no competing marquee surface can render in the same frame — see render_title_row).
    control_box::render_title_row(frame, app, row(0), animated, true);
    if area.height >= 2 {
        control_box::render_seekbar(frame, app, row(1), animated);
    }
    if area.height >= 3 {
        control_box::render_controls(frame, app, row(2), animated);
    }
    if area.height >= 4 {
        control_box::render_status_line(frame, app, row(3), animated);
    }
    // The status line's `EQ:`/`streaming:` toggles and the queue position stay live in the
    // miniplayer, so their surfaces must render here too — an open popup that isn't drawn
    // would capture input invisibly.
    if app.dropdowns.eq_open {
        control_box::render_eq_dropdown(frame, app, area);
    }
    if app.dropdowns.streaming_open {
        control_box::render_streaming_dropdown(frame, app, area);
    }
    app.queue_popup.rect.set(None);
    if app.queue_popup.open {
        super::player::render_queue_popup(frame, app, area);
    }
    super::settings::render_spotify_import_mode_dropdown_popup(frame, app, area);
}
