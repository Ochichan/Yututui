use std::time::Instant;

use crate::{app::App, tui, ui};

use super::{PerfStats, is_transient_terminal_draw_error};

pub(super) fn draw_app_frame(
    terminal: &mut tui::AppTerminal,
    app: &mut App,
    perf: &mut PerfStats,
    output_deadline: Option<Instant>,
) -> std::io::Result<bool> {
    let size = terminal.size()?;
    let tier = crate::ui::layout::tier(ratatui::layout::Rect::new(0, 0, size.width, size.height));
    app.prepare_ui_tier_for_render(tier);
    let start = perf.enabled.then(Instant::now);
    let clear_before = app.take_clear_before_draw();
    let synchronized = clear_before || app.synchronized_draw_active();
    let res = match output_deadline {
        Some(deadline) => {
            tui::draw_frame_until(terminal, synchronized, clear_before, deadline, |f| {
                ui::render(f, app)
            })
        }
        None => tui::draw_frame(terminal, synchronized, clear_before, |f| ui::render(f, app)),
    };
    match res {
        Ok(()) => {
            app.mark_local_rows_rendered();
            if let Some(start) = start {
                perf.record_draw(start.elapsed());
            }
            Ok(true)
        }
        Err(error) if is_transient_terminal_draw_error(&error) => {
            tracing::warn!(
                %error,
                "ignored transient terminal draw failure; waiting for the next input or resize"
            );
            app.dirty = false;
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

pub(super) fn finish_draw_cycle(app: &mut App) {
    app.dirty = app.clear_before_draw_pending();
}

/// Keep IME freshness fail-closed until a successful draw and finish step reach the terminal.
pub(super) fn draw_full_app_frame(
    terminal: &mut tui::AppTerminal,
    app: &mut App,
    perf: &mut PerfStats,
    reducer_turn_unrendered: &mut bool,
    output_deadline: Instant,
) -> std::io::Result<bool> {
    *reducer_turn_unrendered = true;
    let rendered = draw_app_frame(terminal, app, perf, Some(output_deadline))?;
    if rendered {
        finish_draw_cycle(app);
        *reducer_turn_unrendered = false;
    }
    Ok(rendered)
}
