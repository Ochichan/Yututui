//! Pure geometry for the ordinary music Player filler.
//!
//! Rendering stays in `player`: this module only decides the final album-art and lyrics
//! rectangles. Keeping that decision separate lets the canvas use the same rectangles as hard
//! masks before either foreground surface is drawn.

use ratatui::layout::{Rect, Size};

use crate::app::App;
use crate::config::PlayerBarPosition;

pub(super) const ART_MIN_WIDTH: u16 = 6;
pub(super) const ART_MIN_HEIGHT: u16 = 3;
pub(super) const ART_PREFERRED_WIDTH: u16 = 48;
pub(super) const ART_PREFERRED_HEIGHT: u16 = 15;
pub(super) const LYRICS_MIN_WIDTH: u16 = 24;
pub(super) const LYRICS_MIN_HEIGHT: u16 = 3;
pub(super) const LYRICS_PREFERRED_WIDTH: u16 = 40;
pub(super) const LYRICS_PREFERRED_HEIGHT: u16 = 12;

const TOP_ART_GAP: u16 = 1;
const SIDE_GAP: u16 = 2;
const STACK_GAP: u16 = 1;

/// How the ordinary music filler placed its visible foreground surfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PlayerFillerArrangement {
    Empty,
    ArtOnly,
    LyricsOnly,
    SideBySide,
    Stacked,
}

/// Final, non-overlapping foreground rectangles inside the Player filler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PlayerFillerLayout {
    pub(super) art: Option<Rect>,
    pub(super) lyrics: Option<Rect>,
    pub(super) arrangement: PlayerFillerArrangement,
}

impl PlayerFillerLayout {
    const fn empty() -> Self {
        Self {
            art: None,
            lyrics: None,
            arrangement: PlayerFillerArrangement::Empty,
        }
    }

    const fn lyrics_only(area: Rect) -> Self {
        Self {
            art: None,
            lyrics: Some(area),
            arrangement: PlayerFillerArrangement::LyricsOnly,
        }
    }
}

/// Calculate the ordinary music-mode filler geometry.
///
/// `art_visible` is deliberately supplied by the caller. In production it is
/// `App::art_active()`, but taking the resolved value keeps loading, zoom suppression, and the
/// album-art toggle out of this pure layout policy. Dedicated Radio mode has its own renderer and
/// must not call this helper.
pub(super) fn calculate_player_filler_layout(
    app: &App,
    area: Rect,
    art_visible: bool,
    lyrics_visible: bool,
) -> PlayerFillerLayout {
    if area.is_empty() {
        return PlayerFillerLayout::empty();
    }

    match app.player_bar_position() {
        PlayerBarPosition::Top => top_layout(app, area, art_visible, lyrics_visible),
        PlayerBarPosition::Bottom => bottom_layout(app, area, art_visible, lyrics_visible),
    }
}

/// The classic Top geometry is intentionally a direct transcription of the old render-time
/// calculation. In particular, its art is top-anchored after `App::art_fit_rect` and its lyrics
/// consume the full width below the real art bottom.
fn top_layout(
    app: &App,
    area: Rect,
    art_visible: bool,
    lyrics_visible: bool,
) -> PlayerFillerLayout {
    match (art_visible, lyrics_visible) {
        (true, true) => {
            let after_gap = area.height.saturating_sub(TOP_ART_GAP);
            let cap = (after_gap / 2).min(after_gap.saturating_sub(LYRICS_MIN_HEIGHT));
            let band = top_art_band(area, TOP_ART_GAP, cap);
            let Some(art) = top_art_rect(app, band) else {
                return PlayerFillerLayout::lyrics_only(area);
            };
            let lyrics_y = art.bottom().saturating_add(STACK_GAP);
            let lyrics = Rect {
                x: area.x,
                y: lyrics_y,
                width: area.width,
                height: area.bottom().saturating_sub(lyrics_y),
            };
            PlayerFillerLayout {
                art: Some(art),
                lyrics: Some(lyrics),
                arrangement: PlayerFillerArrangement::Stacked,
            }
        }
        (true, false) => {
            let cap = (u32::from(area.height) * 11 / 20) as u16;
            let art = top_art_rect(app, top_art_band(area, TOP_ART_GAP, cap));
            PlayerFillerLayout {
                art,
                lyrics: None,
                arrangement: if art.is_some() {
                    PlayerFillerArrangement::ArtOnly
                } else {
                    PlayerFillerArrangement::Empty
                },
            }
        }
        (false, true) => PlayerFillerLayout::lyrics_only(area),
        (false, false) => PlayerFillerLayout::empty(),
    }
}

fn bottom_layout(
    app: &App,
    area: Rect,
    art_visible: bool,
    lyrics_visible: bool,
) -> PlayerFillerLayout {
    match (art_visible, lyrics_visible) {
        (true, true) => bottom_art_and_lyrics(app, area),
        (true, false) => {
            let art = fitted_bottom_art(app, area.width, area.height)
                .map(|size| centered_rect(area, size));
            PlayerFillerLayout {
                art,
                lyrics: None,
                arrangement: if art.is_some() {
                    PlayerFillerArrangement::ArtOnly
                } else {
                    PlayerFillerArrangement::Empty
                },
            }
        }
        (false, true) => PlayerFillerLayout::lyrics_only(area),
        (false, false) => PlayerFillerLayout::empty(),
    }
}

fn bottom_art_and_lyrics(app: &App, area: Rect) -> PlayerFillerLayout {
    if let Some(layout) = side_by_side_layout(app, area) {
        return layout;
    }
    stacked_layout(app, area).unwrap_or_else(|| PlayerFillerLayout::lyrics_only(area))
}

/// Prefer the fully fitted art and spend width on lyrics down toward their minimum. Only after
/// that minimum is reached may the art shrink. Height constraints aspect-fit the art immediately;
/// there is no useful vertical padding left once the preferred 15-row box no longer fits.
fn side_by_side_layout(app: &App, area: Rect) -> Option<PlayerFillerLayout> {
    if area.width < ART_MIN_WIDTH + SIDE_GAP + LYRICS_MIN_WIDTH
        || area.height < ART_MIN_HEIGHT.max(LYRICS_MIN_HEIGHT)
    {
        return None;
    }

    let mut art = fitted_bottom_art(app, area.width, area.height)?;
    if art.width + SIDE_GAP + LYRICS_MIN_WIDTH > area.width {
        let art_budget = area.width.saturating_sub(SIDE_GAP + LYRICS_MIN_WIDTH);
        art = fitted_bottom_art(app, art_budget, area.height)?;
    }

    let lyrics_width = area
        .width
        .saturating_sub(art.width + SIDE_GAP)
        .min(LYRICS_PREFERRED_WIDTH);
    if lyrics_width < LYRICS_MIN_WIDTH {
        return None;
    }
    let lyrics_height = area.height.min(LYRICS_PREFERRED_HEIGHT);
    if lyrics_height < LYRICS_MIN_HEIGHT {
        return None;
    }

    let group = Size::new(
        art.width + SIDE_GAP + lyrics_width,
        art.height.max(lyrics_height),
    );
    let group_rect = centered_rect(area, group);
    let art_rect = Rect::new(
        group_rect.x,
        group_rect.y + group.height.saturating_sub(art.height) / 2,
        art.width,
        art.height,
    );
    let lyrics_rect = Rect::new(
        art_rect.right() + SIDE_GAP,
        group_rect.y + group.height.saturating_sub(lyrics_height) / 2,
        lyrics_width,
        lyrics_height,
    );
    Some(PlayerFillerLayout {
        art: Some(art_rect),
        lyrics: Some(lyrics_rect),
        arrangement: PlayerFillerArrangement::SideBySide,
    })
}

fn stacked_layout(app: &App, area: Rect) -> Option<PlayerFillerLayout> {
    if area.width < LYRICS_MIN_WIDTH || area.height < ART_MIN_HEIGHT + STACK_GAP + LYRICS_MIN_HEIGHT
    {
        return None;
    }

    // Reserve the minimum readable lyrics window first. The art retains its preferred size while
    // it fits in the remaining space, then aspect-fits monotonically as rows/columns disappear.
    let art_budget_height = area.height.saturating_sub(STACK_GAP + LYRICS_MIN_HEIGHT);
    let art = fitted_bottom_art(app, area.width, art_budget_height)?;
    let lyrics_height = area
        .height
        .saturating_sub(art.height + STACK_GAP)
        .min(LYRICS_PREFERRED_HEIGHT);
    if lyrics_height < LYRICS_MIN_HEIGHT {
        return None;
    }
    let lyrics_width = area.width.min(LYRICS_PREFERRED_WIDTH);
    if lyrics_width < LYRICS_MIN_WIDTH {
        return None;
    }

    let group_height = art.height + STACK_GAP + lyrics_height;
    let group_y = area.y + area.height.saturating_sub(group_height) / 2;
    let art_rect = Rect::new(
        area.x + area.width.saturating_sub(art.width) / 2,
        group_y,
        art.width,
        art.height,
    );
    let lyrics_rect = Rect::new(
        area.x + area.width.saturating_sub(lyrics_width) / 2,
        art_rect.bottom() + STACK_GAP,
        lyrics_width,
        lyrics_height,
    );
    Some(PlayerFillerLayout {
        art: Some(art_rect),
        lyrics: Some(lyrics_rect),
        arrangement: PlayerFillerArrangement::Stacked,
    })
}

/// Fit the source aspect into the preferred 48x15-cell box, clipped by the available size.
/// Reject the result (not merely its bounding box) when aspect fitting makes it smaller than the
/// legibility floor.
fn fitted_bottom_art(app: &App, available_width: u16, available_height: u16) -> Option<Size> {
    let bounds = Rect::new(
        0,
        0,
        available_width.min(ART_PREFERRED_WIDTH),
        available_height.min(ART_PREFERRED_HEIGHT),
    );
    if bounds.width == 0 || bounds.height == 0 {
        return None;
    }
    let fitted = app.art_fit_rect(bounds);
    (fitted.width >= ART_MIN_WIDTH && fitted.height >= ART_MIN_HEIGHT)
        .then_some(Size::new(fitted.width, fitted.height))
}

fn top_art_band(area: Rect, gap: u16, max_height: u16) -> Rect {
    let available = area.height.saturating_sub(gap);
    Rect {
        x: area.x,
        y: area.y + gap,
        width: area.width,
        height: max_height.min(available),
    }
}

fn top_art_rect(app: &App, band: Rect) -> Option<Rect> {
    // This check is on the band, not the aspect-fitted result, matching the classic renderer.
    if band.width < ART_MIN_WIDTH || band.height < ART_MIN_HEIGHT {
        return None;
    }
    let mut rect = app.art_fit_rect(band);
    rect.y = band.y;
    Some(rect)
}

fn centered_rect(area: Rect, size: Size) -> Rect {
    Rect::new(
        area.x + area.width.saturating_sub(size.width) / 2,
        area.y + area.height.saturating_sub(size.height) / 2,
        size.width.min(area.width),
        size.height.min(area.height),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui_image::picker::Picker;

    fn app_with_art(position: PlayerBarPosition, dimensions: (u32, u32)) -> App {
        let mut app = App::new(100);
        app.config.player_bar_position = Some(position);
        app.art.picker = Some(Picker::halfblocks()); // 10x20-pixel cells
        app.art.dims = dimensions;
        app
    }

    fn layout(
        app: &App,
        area: Rect,
        art_visible: bool,
        lyrics_visible: bool,
    ) -> PlayerFillerLayout {
        calculate_player_filler_layout(app, area, art_visible, lyrics_visible)
    }

    fn contains_rect(outer: Rect, inner: Rect) -> bool {
        inner.left() >= outer.left()
            && inner.top() >= outer.top()
            && inner.right() <= outer.right()
            && inner.bottom() <= outer.bottom()
    }

    #[test]
    fn bottom_art_only_keeps_preferred_size_while_padding_collapses() {
        let app = app_with_art(PlayerBarPosition::Bottom, (100, 100));
        let large = layout(&app, Rect::new(0, 0, 98, 21), true, false);
        let less_padding = layout(&app, Rect::new(0, 0, 40, 15), true, false);
        let no_padding = layout(&app, Rect::new(0, 0, 30, 15), true, false);

        assert_eq!(large.art.unwrap().as_size(), Size::new(30, 15));
        assert_eq!(less_padding.art.unwrap().as_size(), Size::new(30, 15));
        assert_eq!(no_padding.art.unwrap().as_size(), Size::new(30, 15));
        assert_eq!(large.art.unwrap(), Rect::new(34, 3, 30, 15));
        assert_eq!(less_padding.art.unwrap(), Rect::new(5, 0, 30, 15));
    }

    #[test]
    fn bottom_art_only_shrinks_monotonically_with_source_aspect() {
        let app = app_with_art(PlayerBarPosition::Bottom, (160, 90));
        let mut previous = Size::new(u16::MAX, u16::MAX);
        for area in [
            Rect::new(0, 0, 58, 20),
            Rect::new(0, 0, 48, 15),
            Rect::new(0, 0, 40, 12),
            Rect::new(0, 0, 32, 9),
        ] {
            let art = layout(&app, area, true, false).art.unwrap();
            assert!(art.width <= previous.width && art.height <= previous.height);
            // 16:9 pixels in 1:2 cells is approximately a 32:9 cell ratio. Allow one cell
            // for independent integer rounding of width and height.
            assert!((i32::from(art.width) * 9 - i32::from(art.height) * 32).abs() <= 16);
            previous = art.as_size();
        }
    }

    #[test]
    fn bottom_requested_filler_sizes_choose_side_by_side_then_yield_tiny_filler() {
        let square = app_with_art(PlayerBarPosition::Bottom, (100, 100));
        for area in [
            Rect::new(1, 2, 158, 41), // 160x50 frame
            Rect::new(1, 2, 98, 21),  // 100x30 frame
            Rect::new(1, 2, 78, 15),  // 80x24 frame
            Rect::new(1, 2, 58, 9),   // 60x18 frame
        ] {
            let result = layout(&square, area, true, true);
            assert_eq!(result.arrangement, PlayerFillerArrangement::SideBySide);
            assert!(contains_rect(area, result.art.unwrap()));
            assert!(contains_rect(area, result.lyrics.unwrap()));
            assert_eq!(
                result.art.unwrap().right() + SIDE_GAP,
                result.lyrics.unwrap().x
            );
        }

        let tiny = Rect::new(1, 2, 30, 5); // 32x14 Full-frame filler
        let result = layout(&square, tiny, true, true);
        assert_eq!(result, PlayerFillerLayout::lyrics_only(tiny));
    }

    #[test]
    fn wide_art_spends_lyrics_width_before_shrinking() {
        let app = app_with_art(PlayerBarPosition::Bottom, (160, 90));
        let preferred = layout(&app, Rect::new(0, 0, 90, 15), true, true);
        assert_eq!(preferred.art.unwrap().as_size(), Size::new(48, 14));
        assert_eq!(preferred.lyrics.unwrap().as_size(), Size::new(40, 12));

        let narrower = layout(&app, Rect::new(0, 0, 78, 15), true, true);
        assert_eq!(narrower.art.unwrap().as_size(), Size::new(48, 14));
        assert_eq!(narrower.lyrics.unwrap().width, 28);

        let short = layout(&app, Rect::new(0, 0, 58, 9), true, true);
        assert_eq!(short.art.unwrap().as_size(), Size::new(32, 9));
        assert_eq!(short.lyrics.unwrap().as_size(), Size::new(24, 9));
    }

    #[test]
    fn horizontal_minimum_failure_falls_back_to_centered_stack() {
        let app = app_with_art(PlayerBarPosition::Bottom, (100, 100));
        let area = Rect::new(10, 4, 30, 20);
        let result = layout(&app, area, true, true);
        assert_eq!(result.arrangement, PlayerFillerArrangement::Stacked);
        assert_eq!(result.art.unwrap(), Rect::new(10, 4, 30, 15));
        assert_eq!(result.lyrics.unwrap(), Rect::new(10, 20, 30, 4));
        assert_eq!(
            result.art.unwrap().bottom() + STACK_GAP,
            result.lyrics.unwrap().y
        );
    }

    #[test]
    fn art_that_cannot_reach_its_final_minimum_is_hidden() {
        let app = app_with_art(PlayerBarPosition::Bottom, (160, 90));
        let area = Rect::new(3, 7, 30, 6);
        let result = layout(&app, area, true, true);
        assert_eq!(result, PlayerFillerLayout::lyrics_only(area));

        let art_only = layout(&app, Rect::new(0, 0, 8, 3), true, false);
        assert_eq!(art_only, PlayerFillerLayout::empty());
    }

    #[test]
    fn art_off_or_loading_gives_lyrics_the_entire_filler() {
        let app = app_with_art(PlayerBarPosition::Bottom, (100, 100));
        let area = Rect::new(1, 2, 78, 15);
        assert_eq!(
            layout(&app, area, false, true),
            PlayerFillerLayout::lyrics_only(area)
        );
        assert_eq!(
            layout(&app, area, false, false),
            PlayerFillerLayout::empty()
        );
    }

    #[test]
    fn top_art_only_geometry_matches_the_classic_formula() {
        let app = app_with_art(PlayerBarPosition::Top, (100, 100));
        let area = Rect::new(1, 9, 78, 13);
        let result = layout(&app, area, true, false);
        // cap=floor(13*11/20)=7; a square is 14x7 cells and is re-anchored at y+1.
        assert_eq!(result.art, Some(Rect::new(33, 10, 14, 7)));
        assert_eq!(result.lyrics, None);
    }

    #[test]
    fn top_art_and_lyrics_geometry_matches_the_classic_formula() {
        let app = app_with_art(PlayerBarPosition::Top, (100, 100));
        let area = Rect::new(1, 9, 78, 13);
        let result = layout(&app, area, true, true);
        // after_gap=12, cap=min(6, 9)=6; square art=12x6, then the old one-row gap.
        assert_eq!(result.art, Some(Rect::new(34, 10, 12, 6)));
        assert_eq!(result.lyrics, Some(Rect::new(1, 17, 78, 5)));
        assert_eq!(result.arrangement, PlayerFillerArrangement::Stacked);
    }

    #[test]
    fn top_tiny_band_preserves_old_art_rejection_and_lyrics_fallback() {
        let app = app_with_art(PlayerBarPosition::Top, (100, 100));
        let area = Rect::new(0, 0, 30, 5);
        assert_eq!(
            layout(&app, area, true, true),
            PlayerFillerLayout::lyrics_only(area)
        );
        assert_eq!(layout(&app, area, true, false), PlayerFillerLayout::empty());
    }
}
