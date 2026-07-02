use ratatui::Frame;

use crate::app::App;

/// Last-pass renderer guard for Linux basic TTY compatibility.
///
/// The kernel console renders through the loaded console font, so Unicode coverage varies by
/// distro/font. Retro mode keeps final cells inside printable ASCII plus the classic 256-cell
/// CP437/VGA repertoire; English UI text is handled by config fallback, and this catches icons,
/// borders, gauges, and track metadata. Every replacement is exactly one cell wide, so scrubbing
/// can never reflow a line the layout pass already measured.
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
    let base = symbol.chars().next()?;
    Some(replacement_for(base))
}

/// Map one unsupported grapheme's base character to a single-cell CP437-safe stand-in.
///
/// Any combining marks after the base are dropped: the cluster occupies one cell, and the
/// closest thing a 256-glyph console font can show is the bare base character (this keeps
/// decorated marquee motifs like `|̲̅●̲̅|` readable instead of collapsing them into `?`).
fn replacement_for(c: char) -> &'static str {
    // Braille patterns (throbber frames, dot art): grade by raised-dot count so dot art
    // keeps its light/dark structure instead of flattening into a wall of `?`.
    if let Some(shade) = braille_replacement(c) {
        return shade;
    }
    // A supported base char that only failed as a cluster: keep it as-is.
    if let Some(s) = printable_ascii(c) {
        return s;
    }
    if let Some(s) = cp437_static(c) {
        return s;
    }
    match c {
        '┌' | '┐' | '└' | '┘' | '╭' | '╮' | '╰' | '╯' | '╔' | '╗' | '╚' | '╝' | '┏' | '┓' | '┗'
        | '┛' | '┬' | '┴' | '├' | '┤' | '┼' => "+",
        '─' | '━' | '═' | '╌' | '╍' | '┄' | '┅' | '┈' | '┉' | '—' | '–' | '−' => {
            "-"
        }
        '│' | '┃' | '║' | '╎' | '╏' | '┆' | '┇' | '┊' | '┋' | '▏' | '▕' => {
            "|"
        }
        '▶' | '▸' | '➤' => "►",
        '⇥' => "→",
        '›' | '»' => ">",
        '◀' | '◂' => "◄",
        '⇤' => "←",
        '‹' | '«' => "<",
        '↗' | '➚' | '⤴' => "→",
        '‖' | 'Ⅱ' => "║",
        '©' => "c",
        '®' => "r",
        '™' => "t",
        '♥' | '★' | '☆' | '✦' | '✧' | '⋆' | '✨' | '♪' | '♫' | '♬' => "*",
        '✗' | '✕' | '×' => "x",
        '✓' | '✔' => "v",
        '⚠' => "!",
        '•' | '·' | '°' | '…' => ".",
        '●' | '◉' | '⬤' => "•",
        '○' | '◯' | '◦' => "o",
        '◆' | '◇' => "♦",
        'ı' => "i",
        '▒' | '░' | '▧' => "/",
        '█' | '▄' | '▀' | '▁' | '▂' | '▃' | '▅' | '▆' | '▇' | '▔' => "#",
        '⌕' => "?",
        '👍' => "+",
        '👎' => "-",
        '🤔' => "?",
        '🔀' => "S",
        '🔁' => "R",
        '🔂' => "1",
        '⬇' => "v",
        _ => "?",
    }
}

/// Braille patterns (U+2800–U+28FF) shaded by how many of the eight dots are raised, so
/// braille art (the Radio-mode set piece, the status throbber) degrades into halftone
/// ASCII instead of uniform `?` noise. The low byte of the codepoint is the dot bitmask.
fn braille_replacement(c: char) -> Option<&'static str> {
    let cp = c as u32;
    if !(0x2800..=0x28FF).contains(&cp) {
        return None;
    }
    const SHADES: [&str; 9] = [" ", ".", ".", ":", ":", "+", "*", "*", "#"];
    Some(SHADES[(cp - 0x2800).count_ones() as usize])
}

/// A `'static` one-char slice for a printable-ASCII character (every printable ASCII char
/// is one byte, so byte indexing is char indexing).
fn printable_ascii(c: char) -> Option<&'static str> {
    const PRINTABLE: &str = concat!(
        " !\"#$%&'()*+,-./0123456789:;<=>?",
        "@ABCDEFGHIJKLMNOPQRSTUVWXYZ[\\]^_",
        "`abcdefghijklmnopqrstuvwxyz{|}~",
    );
    let i = (c as usize).checked_sub(0x20)?;
    PRINTABLE.get(i..i + 1)
}

/// A `'static` one-char slice for a CP437-covered character, found in the repertoire tables.
fn cp437_static(c: char) -> Option<&'static str> {
    for table in [CP437_GRAPHICS, CP437_PRINTABLE] {
        if let Some((i, m)) = table.char_indices().find(|&(_, m)| m == c) {
            return table.get(i..i + m.len_utf8());
        }
    }
    None
}

/// Whether a cell symbol can be shown verbatim by a 256-glyph console font (printable ASCII
/// or the CP437 repertoire). Crate-visible so app-level render tests can assert a scrubbed
/// frame contains nothing else.
pub(crate) fn retro_supported(symbol: &str) -> bool {
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
    use super::{CP437_GRAPHICS, CP437_PRINTABLE, ascii_replacement};
    use unicode_width::UnicodeWidthStr;

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

    #[test]
    fn repeat_modes_stay_distinguishable() {
        assert_ne!(ascii_replacement("🔁"), ascii_replacement("🔂"));
        assert_eq!(ascii_replacement("🔁"), Some("R"));
        assert_eq!(ascii_replacement("🔂"), Some("1"));
    }

    #[test]
    fn braille_shades_by_dot_count() {
        assert_eq!(ascii_replacement("⠀"), Some(" ")); // 0 dots
        assert_eq!(ascii_replacement("⠁"), Some(".")); // 1 dot
        assert_eq!(ascii_replacement("⠋"), Some(":")); // 3 dots (spinner frame)
        assert_eq!(ascii_replacement("⣿"), Some("#")); // all 8 dots
    }

    #[test]
    fn combining_clusters_keep_their_base_character() {
        // The marquee motif decorates ASCII/CP437 bases with combining marks; the scrub
        // keeps the readable base instead of a `?`.
        assert_eq!(ascii_replacement("|\u{332}\u{305}"), Some("|"));
        assert_eq!(ascii_replacement("=\u{332}\u{305}"), Some("="));
        assert_eq!(ascii_replacement("●\u{332}\u{305}"), Some("•"));
        assert_eq!(ascii_replacement("♪\u{305}"), Some("♪"));
    }

    #[test]
    fn every_replacement_is_one_cell_wide_and_cp437_safe() {
        // Widths must never change during the scrub, and outputs must themselves be
        // retro-supported, or the fallback could reflow or re-break a measured layout.
        let samples = [
            "▶",
            "⇥",
            "✗",
            "✓",
            "⚠",
            "…",
            "●",
            "◆",
            "ı",
            "▧",
            "▁",
            "⌕",
            "👍",
            "👎",
            "🤔",
            "🔀",
            "🔁",
            "🔂",
            "⬇",
            "⠀",
            "⠿",
            "⣿",
            "|\u{332}\u{305}",
            "✨",
            "한",
            "🖱",
        ];
        for s in samples {
            let Some(r) = ascii_replacement(s) else {
                panic!("{s:?} should need a replacement");
            };
            assert_eq!(
                UnicodeWidthStr::width(r),
                1,
                "{s:?} → {r:?} must be one cell"
            );
            assert!(
                r.is_ascii() || CP437_GRAPHICS.contains(r) || CP437_PRINTABLE.contains(r),
                "{s:?} → {r:?} must itself be retro-supported"
            );
        }
    }
}
