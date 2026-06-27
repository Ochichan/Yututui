//! The `?` help overlay: a centered cheat-sheet of every keybinding, drawn on top of the
//! active view. Content is generated from the live [`crate::keymap::KeyMap`], so it always
//! reflects the current (possibly remapped) keys. Toggled by `Action::ToggleHelp`.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::app::App;
use crate::keymap::{self, Action, KeyContext};
use crate::theme::ThemeRole as R;

/// Render the cheat-sheet as a centered popup over `area`.
pub fn render(frame: &mut Frame, app: &App, area: Rect) {
    let popup = centered(area, 80, 80);
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(" Help · keybindings ")
        .borders(Borders::ALL)
        .border_style(app.theme.style(R::BorderPrimary))
        .style(app.theme.style(R::TextPrimary));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    // Split into two columns so the full list fits without scrolling on most terminals.
    let cols = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(inner);
    let (left, right) = build_columns(app);
    frame.render_widget(Paragraph::new(left).style(app.theme.style(R::TextPrimary)), cols[0]);
    frame.render_widget(Paragraph::new(right).style(app.theme.style(R::TextPrimary)), cols[1]);
}

/// Build the cheat-sheet lines, split across two columns at a group boundary.
fn build_columns(app: &App) -> (Vec<Line<'static>>, Vec<Line<'static>>) {
    let groups = help_groups(app);
    // Each group occupies its header + bindings + one trailing blank as padding.
    let total: usize = groups.iter().map(|(_, rows)| rows.len() + 2).sum();

    let mut left: Vec<Line> = Vec::new();
    let mut right: Vec<Line> = Vec::new();
    let mut placed = 0usize;
    for (title, rows) in groups {
        // Once the left column holds roughly half the rows, spill into the right one.
        let col = if placed * 2 < total { &mut left } else { &mut right };
        col.push(Line::from(Span::styled(
            title,
            app.theme.style(R::HelpGroup).add_modifier(Modifier::BOLD),
        )));
        for (key, label) in &rows {
            col.push(Line::from(vec![
                Span::styled(format!("{key:>8}  "), app.theme.style(R::HelpKey)),
                Span::styled(label.to_owned(), app.theme.style(R::HelpAction)),
            ]));
        }
        // A little padding after each group so the sections breathe.
        col.push(Line::from(""));
        placed += rows.len() + 2;
    }
    (left, right)
}

fn help_groups(app: &App) -> Vec<(String, Vec<(String, String)>)> {
    let mut out = Vec::new();
    for (ctx, actions) in keymap::groups() {
        if ctx == KeyContext::SearchResults {
            out.push((
                KeyContext::SearchInput.title().to_owned(),
                vec![fixed_enter_row(
                    Action::Confirm.human_label_for(KeyContext::SearchInput),
                )],
            ));
        }

        let mut rows: Vec<(String, String)> = actions
            .iter()
            .map(|action| {
                let key = app
                    .keymap
                    .chord(ctx, *action)
                    .map_or_else(|| "—".to_owned(), keymap::format_chord);
                (key, action.human_label_for(ctx).to_owned())
            })
            .collect();
        if ctx == KeyContext::SearchResults {
            rows.insert(
                0,
                fixed_enter_row(Action::Confirm.human_label_for(KeyContext::SearchResults)),
            );
        }
        out.push((ctx.title().to_owned(), rows));
    }
    out
}

fn fixed_enter_row(label: &str) -> (String, String) {
    ("Enter".to_owned(), label.to_owned())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_enter_rows_are_listed_as_fixed_help_rows() {
        let app = App::new(100);
        let groups = help_groups(&app);

        let search_box = groups
            .iter()
            .find_map(|(title, rows)| (title == "Search box").then_some(rows))
            .expect("search box group");
        assert_eq!(search_box, &vec![("Enter".to_owned(), "Search".to_owned())]);

        let search_results = groups
            .iter()
            .find_map(|(title, rows)| (title == "Search results").then_some(rows))
            .expect("search results group");
        assert_eq!(
            search_results.first(),
            Some(&("Enter".to_owned(), "Play selected".to_owned()))
        );
    }
}
