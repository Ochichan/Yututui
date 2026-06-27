//! The `?` help overlay: a centered cheat-sheet of every keybinding, drawn on top of the
//! active view. Content is generated from the live [`crate::keymap::KeyMap`], so it always
//! reflects the current (possibly remapped) keys. Toggled by `Action::ToggleHelp`.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::app::App;
use crate::keymap;

/// Render the cheat-sheet as a centered popup over `area`.
pub fn render(frame: &mut Frame, app: &App, area: Rect) {
    let popup = centered(area, 80, 80);
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(" Help · keybindings ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Magenta));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    // Split into two columns so the full list fits without scrolling on most terminals.
    let cols = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(inner);
    let (left, right) = build_columns(app);
    frame.render_widget(Paragraph::new(left), cols[0]);
    frame.render_widget(Paragraph::new(right), cols[1]);
}

/// Build the cheat-sheet lines, split across two columns at a group boundary.
fn build_columns(app: &App) -> (Vec<Line<'static>>, Vec<Line<'static>>) {
    let groups = keymap::groups();
    let total: usize = groups.iter().map(|(_, a)| a.len() + 1).sum();

    let mut left: Vec<Line> = Vec::new();
    let mut right: Vec<Line> = Vec::new();
    let mut placed = 0usize;
    for (ctx, actions) in groups {
        // Once the left column holds roughly half the rows, spill into the right one.
        let col = if placed * 2 < total { &mut left } else { &mut right };
        if !col.is_empty() {
            col.push(Line::from(""));
        }
        col.push(Line::from(Span::styled(
            ctx.title().to_owned(),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )));
        for action in &actions {
            let key = app
                .keymap
                .chord(ctx, *action)
                .map_or_else(|| "—".to_owned(), keymap::format_chord);
            col.push(Line::from(vec![
                Span::styled(format!("{key:>8}  "), Style::default().fg(Color::Yellow)),
                Span::raw(action.human_label_for(ctx).to_owned()),
            ]));
        }
        placed += actions.len() + 1;
    }
    (left, right)
}

/// A rect centered in `area`, sized to the given width/height percentages.
fn centered(area: Rect, pct_w: u16, pct_h: u16) -> Rect {
    let w = area.width * pct_w / 100;
    let h = area.height * pct_h / 100;
    Rect {
        x: area.x + area.width.saturating_sub(w) / 2,
        y: area.y + area.height.saturating_sub(h) / 2,
        width: w,
        height: h,
    }
}
