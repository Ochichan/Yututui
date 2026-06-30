use ratatui::Frame;

use crate::app::App;

/// Last-pass renderer guard for Linux basic TTY compatibility.
///
/// The kernel console renders through the loaded console font, so Unicode coverage varies by
/// distro/font. Retro mode keeps every final cell in printable 7-bit ASCII; English UI text is
/// handled by config fallback, and this catches icons, borders, gauges, and track metadata.
pub fn scrub_frame(frame: &mut Frame, app: &App) {
    if !app.retro_mode() {
        return;
    }
    let area = frame.area();
    let buf = frame.buffer_mut();
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            let Some(cell) = buf.cell_mut((x, y)) else {
                continue;
            };
            let replacement = ascii_replacement(cell.symbol());
            if let Some(symbol) = replacement {
                cell.set_symbol(symbol);
            }
        }
    }
}

fn ascii_replacement(symbol: &str) -> Option<&'static str> {
    if symbol.is_ascii() {
        return None;
    }
    Some(match symbol {
        "┌" | "┐" | "└" | "┘" | "╭" | "╮" | "╰" | "╯" | "╔" | "╗" | "╚" | "╝" | "┏" | "┓" | "┗"
        | "┛" | "┬" | "┴" | "├" | "┤" | "┼" => "+",
        "─" | "━" | "═" | "╌" | "╍" | "┄" | "┅" | "┈" | "┉" | "—" | "–" | "−" => {
            "-"
        }
        "│" | "┃" | "║" | "╎" | "╏" | "┆" | "┇" | "┊" | "┋" | "▏" | "▕" => {
            "|"
        }
        "▶" | "▸" | "➤" | "⇥" | "›" | "»" => ">",
        "◀" | "‹" | "«" | "⇤" => "<",
        "‖" | "Ⅱ" => "=",
        "♥" | "★" | "☆" | "✦" | "✧" | "⋆" | "✨" | "♪" | "♫" => "*",
        "✗" | "✕" | "×" => "x",
        "✓" | "✔" => "v",
        "⚠" => "!",
        "•" | "·" | "°" | "…" => ".",
        "▒" | "░" | "▧" => "/",
        "█" | "▄" | "▀" | "▁" | "▂" | "▃" | "▅" | "▆" | "▇" | "▔" => "#",
        "⌕" => "?",
        "👍" => "+",
        "👎" => "-",
        "🤔" => "?",
        "🔀" => "S",
        "🔁" | "🔂" => "R",
        "⬇" => "v",
        _ => "?",
    })
}

#[cfg(test)]
mod tests {
    use super::ascii_replacement;

    #[test]
    fn maps_common_tui_symbols_to_ascii() {
        assert_eq!(ascii_replacement("┌"), Some("+"));
        assert_eq!(ascii_replacement("─"), Some("-"));
        assert_eq!(ascii_replacement("│"), Some("|"));
        assert_eq!(ascii_replacement("▶"), Some(">"));
        assert_eq!(ascii_replacement("♥"), Some("*"));
        assert_eq!(ascii_replacement("a"), None);
    }
}
