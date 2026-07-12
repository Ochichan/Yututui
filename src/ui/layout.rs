//! Responsive layout tiers: the full chrome, or — below the size where any normal screen
//! can render whole — a purpose-built miniplayer (see `views::mini`).

use ratatui::layout::Rect;

/// The screen-wide responsive tier, decided per frame from the real cell grid (text zoom
/// rescales the virtual grid without a terminal resize, so only the render pass knows it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UiTier {
    /// Normal chrome: bordered view, nav strip, footer, and (in Bottom bar mode) the
    /// docked control box.
    #[default]
    Full,
    /// The whole UI is a miniplayer: title, seekbar, transport, minimal status — no
    /// border, nav, or footer. Modeled on the tray panel's "Mini" skin.
    Mini,
}

/// Below this height the Bottom-default Search and DJ Gem layouts cannot fit their border,
/// fixed input/dock/footer chrome, and one useful content row. Enter Mini before ratatui has to
/// compress or clip those fixed rows; the legacy Top layout simply gets the same stable boundary.
pub const MINI_MIN_H: u16 = 14;
/// Below this width even the tight transport strip clips when centered: 26 cells of
/// controls in English, ~28 with the Korean `볼륨` label and a 3-digit volume, plus the
/// 2 border columns and a margin.
pub const MINI_MIN_W: u16 = 32;

/// The tier for a frame of the given size. Purely size-driven (no hysteresis needed —
/// the mapping is deterministic, so growing back restores the full chrome by itself).
pub fn tier(area: Rect) -> UiTier {
    if area.height < MINI_MIN_H || area.width < MINI_MIN_W {
        UiTier::Mini
    } else {
        UiTier::Full
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn size(w: u16, h: u16) -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: w,
            height: h,
        }
    }

    #[test]
    fn tier_thresholds_are_pinned() {
        // The smallest Full frame, and the boundary cells around it.
        assert_eq!(tier(size(32, 14)), UiTier::Full);
        assert_eq!(tier(size(31, 14)), UiTier::Mini);
        assert_eq!(tier(size(32, 13)), UiTier::Mini);
        assert_eq!(tier(size(80, 24)), UiTier::Full);
        assert_eq!(tier(size(28, 8)), UiTier::Mini);
    }
}
