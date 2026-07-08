pub mod asset;
pub mod generated;
pub mod render;

use ratatui::Frame;
use ratatui::layout::Rect;

use crate::app::App;

pub fn render_dj_gem(frame: &mut Frame, app: &App, inner: Rect) {
    const TEXT_W: u16 = 54;

    let asset = if app.ai.thinking {
        &generated::dj_gem::DJ_GEM_THINKING
    } else if app.ai_mascot_active() {
        &generated::dj_gem::DJ_GEM_GROOVE
    } else {
        &generated::dj_gem::DJ_GEM_IDLE
    };
    if inner.width < TEXT_W + asset.width || inner.height < asset.height + 3 {
        return;
    }

    let free_w = inner.width - asset.width;
    let area = Rect {
        x: inner.x + (free_w * 3 / 4).max(TEXT_W),
        y: inner.y + 1,
        width: asset.width,
        height: asset.height,
    };
    render::render(frame, app, area, asset);
}

#[cfg(test)]
mod tests {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

    use super::generated;

    fn changed_cells(a: &[&str], b: &[&str]) -> usize {
        a.iter()
            .zip(b.iter())
            .map(|(a, b)| a.chars().zip(b.chars()).filter(|(a, b)| a != b).count())
            .sum()
    }

    fn nonblank_bounds(lines: &[&str]) -> Option<(usize, usize, usize, usize)> {
        let mut min_x = usize::MAX;
        let mut min_y = usize::MAX;
        let mut max_x = 0;
        let mut max_y = 0;
        let mut any = false;
        for (y, line) in lines.iter().enumerate() {
            for (x, ch) in line.chars().enumerate() {
                if ch == ' ' {
                    continue;
                }
                any = true;
                min_x = min_x.min(x);
                min_y = min_y.min(y);
                max_x = max_x.max(x);
                max_y = max_y.max(y);
            }
        }
        any.then_some((min_x, min_y, max_x, max_y))
    }

    fn frame_contains(lines: &[&str], needle: &str) -> bool {
        lines.iter().any(|line| line.contains(needle))
    }

    #[test]
    fn asset_lines_match_dimensions() {
        for asset in generated::all_assets() {
            for frame in asset.frames {
                assert_eq!(
                    frame.lines.len(),
                    usize::from(asset.height),
                    "{}",
                    asset.name
                );
                for line in frame.lines {
                    assert_eq!(
                        UnicodeWidthStr::width(*line),
                        usize::from(asset.width),
                        "{} line {line:?}",
                        asset.name
                    );
                }
            }
        }
    }

    #[test]
    fn asset_glyphs_are_single_width() {
        for asset in generated::all_assets() {
            for frame in asset.frames {
                for line in frame.lines {
                    for ch in line.chars() {
                        assert_eq!(
                            UnicodeWidthChar::width(ch),
                            Some(1),
                            "{} {ch:?}",
                            asset.name
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn groove_animation_stays_under_cell_change_budget() {
        let asset = &generated::dj_gem::DJ_GEM_GROOVE;
        let total_cells = usize::from(asset.width) * usize::from(asset.height);
        let max_changed = total_cells * 15 / 100;
        for pair in asset.frames.windows(2) {
            let changed = changed_cells(pair[0].lines, pair[1].lines);
            assert!(
                changed <= max_changed,
                "{} changed {changed}/{total_cells} cells",
                asset.name
            );
        }

        let changed = changed_cells(
            asset.frames.last().unwrap().lines,
            asset.frames.first().unwrap().lines,
        );
        assert!(
            changed <= max_changed,
            "{} loop seam changed {changed}/{total_cells} cells",
            asset.name
        );
    }

    #[test]
    fn groove_loop_returns_to_rest_pose_without_a_large_seam() {
        let asset = &generated::dj_gem::DJ_GEM_GROOVE;
        let total_cells = usize::from(asset.width) * usize::from(asset.height);
        let changed = changed_cells(
            asset.frames.last().unwrap().lines,
            asset.frames.first().unwrap().lines,
        );
        assert!(
            changed * 100 <= total_cells * 6,
            "{} loop seam changed {changed}/{total_cells} cells",
            asset.name
        );
        assert_eq!(
            nonblank_bounds(asset.frames.last().unwrap().lines),
            nonblank_bounds(asset.frames.first().unwrap().lines),
            "{} loop seam bounds should not jump",
            asset.name
        );
    }

    #[test]
    fn groove_animation_keeps_nonblank_bounds_stable() {
        let asset = &generated::dj_gem::DJ_GEM_GROOVE;
        let expected = nonblank_bounds(asset.frames[0].lines).unwrap();
        for frame in asset.frames {
            assert_eq!(
                nonblank_bounds(frame.lines),
                Some(expected),
                "{} frame bounds should not jump by a cell",
                asset.name
            );
        }
    }

    #[test]
    fn dj_gem_frames_keep_24x15_silhouette_and_core_features() {
        for asset in [
            &generated::dj_gem::DJ_GEM_IDLE,
            &generated::dj_gem::DJ_GEM_GROOVE,
            &generated::dj_gem::DJ_GEM_THINKING,
            &generated::dj_gem::DJ_GEM_IDLE_RETRO,
            &generated::dj_gem::DJ_GEM_GROOVE_RETRO,
            &generated::dj_gem::DJ_GEM_THINKING_RETRO,
        ] {
            assert!(asset.width <= 24, "{}", asset.name);
            assert!(asset.height <= 15, "{}", asset.name);
            for frame in asset.frames {
                let (min_x, min_y, max_x, max_y) = nonblank_bounds(frame.lines).unwrap();
                assert!(
                    max_x - min_x + 1 >= 18,
                    "{} silhouette too narrow",
                    asset.name
                );
                assert!(
                    max_y - min_y + 1 >= 14,
                    "{} silhouette too short",
                    asset.name
                );
                assert!(frame_contains(frame.lines, "/\\"), "{} ears", asset.name);
                assert!(frame_contains(frame.lines, "DJ"), "{} label DJ", asset.name);
                assert!(
                    frame_contains(frame.lines, "GEM"),
                    "{} label GEM",
                    asset.name
                );
                assert!(
                    frame_contains(frame.lines, "||"),
                    "{} body/legs",
                    asset.name
                );
                assert!(
                    frame_contains(frame.lines, "\\____/") || frame_contains(frame.lines, "___"),
                    "{} mouth detail",
                    asset.name
                );
            }
        }
    }

    #[test]
    fn retro_asset_is_ascii_safe() {
        for asset in [
            &generated::dj_gem::DJ_GEM_IDLE_RETRO,
            &generated::dj_gem::DJ_GEM_GROOVE_RETRO,
            &generated::dj_gem::DJ_GEM_THINKING_RETRO,
        ] {
            for frame in asset.frames {
                for line in frame.lines {
                    for ch in line.chars() {
                        assert!(
                            ch.is_ascii() && !ch.is_ascii_control(),
                            "{} {ch:?}",
                            asset.name
                        );
                    }
                }
            }
        }
    }
}
