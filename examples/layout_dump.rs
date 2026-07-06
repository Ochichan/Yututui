//! Scratch: render the UI at small virtual sizes and dump the buffer as text.
//! Run: cargo run --example layout_dump -- <w> <h> [mode]

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ytm_tui::app::{App, Mode};

fn main() {
    let mut args = std::env::args().skip(1);
    let w: u16 = args.next().and_then(|s| s.parse().ok()).unwrap_or(50);
    let h: u16 = args.next().and_then(|s| s.parse().ok()).unwrap_or(15);
    let mode = args.next().unwrap_or_else(|| "player".into());

    let mut app = App::new(100);
    let songs: Vec<_> = (0..5)
        .map(|i| {
            ytm_tui::api::Song::remote(
                format!("id{i}"),
                format!("A Fairly Long Song Title {i}"),
                "Some Artist",
                "3:45",
            )
        })
        .collect();
    app.queue.set(songs, 0);
    app.mode = match mode.as_str() {
        "search" => Mode::Search,
        "library" => Mode::Library,
        "settings" => Mode::Settings,
        "ai" => Mode::Ai,
        _ => Mode::Player,
    };
    if mode == "help" {
        app.mode = Mode::Player;
        app.overlays.help_visible = true;
    }

    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| ytm_tui::ui::render(f, &app)).unwrap();
    let buf = terminal.backend().buffer().clone();
    for y in 0..h {
        let mut line = String::new();
        for x in 0..w {
            line.push_str(buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "));
        }
        println!("|{line}|");
    }
}
