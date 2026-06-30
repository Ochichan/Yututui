use ratatui::Frame;

use crate::app::App;

/// Last-pass renderer guard for Linux basic TTY compatibility.
///
/// The kernel console renders through the loaded console font, so Unicode coverage varies by
/// distro/font. Retro mode keeps final cells inside printable ASCII plus the classic 256-cell
/// CP437/VGA repertoire; English UI text is handled by config fallback, and this catches icons,
/// borders, gauges, and track metadata.
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
    if retro_supported(symbol) {
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
        "▶" | "▸" | "➤" => "►",
        "⇥" => "→",
        "›" | "»" => ">",
        "◀" => "◄",
        "⇤" => "←",
        "‹" | "«" => "<",
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

fn retro_supported(symbol: &str) -> bool {
    if symbol.is_ascii() {
        return true;
    }
    let mut chars = symbol.chars();
    let Some(c) = chars.next() else {
        return true;
    };
    chars.next().is_none() && (CP437_GRAPHICS.contains(c) || CP437_PRINTABLE.contains(c))
}

// Classic CP437 display glyphs from the C0 control-code positions, as shown by VGA text mode.
const CP437_GRAPHICS: &str = "☺☻♥♦♣♠•◘○◙♂♀♪♫☼►◄↕‼¶§▬↨↑↓→←∟↔▲▼⌂";

// Printable 0x20-0xFF characters from the Unicode CP437 mapping table. Keeping this explicit
// makes the scrubber conservative while still preserving the box, shade, block, math, and Latin
// glyphs a 256-cell retro console font normally carries.
const CP437_PRINTABLE: &str = concat!(
    " !\"#$%&'()*+,-./0123456789:;<=>?",
    "@ABCDEFGHIJKLMNOPQRSTUVWXYZ[\\]^_",
    "`abcdefghijklmnopqrstuvwxyz{|}~",
    "ÇüéâäàåçêëèïîìÄÅÉæÆôöòûùÿÖÜ¢£¥₧ƒ",
    "áíóúñÑªº¿⌐¬½¼¡«»",
    "░▒▓│┤╡╢╖╕╣║╗╝╜╛┐",
    "└┴┬├─┼╞╟╚╔╩╦╠═╬╧",
    "╨╤╥╙╘╒╓╫╪┘┌",
    "█▄▌▐▀αßΓπΣσµτΦΘΩδ∞φε∩",
    "≡±≥≤⌠⌡÷≈°∙·√ⁿ²■\u{00A0}",
);

#[cfg(test)]
mod tests {
    use super::ascii_replacement;

    #[test]
    fn maps_common_tui_symbols_to_ascii() {
        assert_eq!(ascii_replacement("┌"), None);
        assert_eq!(ascii_replacement("─"), None);
        assert_eq!(ascii_replacement("│"), None);
        assert_eq!(ascii_replacement("▶"), Some("►"));
        assert_eq!(ascii_replacement("♥"), None);
        assert_eq!(ascii_replacement("a"), None);
        assert_eq!(ascii_replacement("✓"), Some("v"));
    }
}
