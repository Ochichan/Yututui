//! Chapter navigation shared by the interactive and headless playback owners.
//!
//! Both owners keep mpv's chapter list and must resolve `chapter next|prev` and the current
//! chapter identically, so the arithmetic lives here over bare start times.

use serde::{Deserialize, Serialize};

/// Margin around the playhead so a jump lands on the neighbouring boundary instead of
/// re-selecting the boundary the playhead already sits on.
const BOUNDARY_HYSTERESIS_SECS: f64 = 0.5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChapterDirection {
    Next,
    Prev,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChapterJumpRefusal {
    NoChapters,
    AtBoundary,
}

impl ChapterJumpRefusal {
    /// The stable `reason` string a remote reply carries for this refusal.
    pub fn reason(self) -> &'static str {
        match self {
            Self::NoChapters => "no_chapters",
            Self::AtBoundary => "chapter_boundary",
        }
    }
}

/// The start time to seek to for a jump from `position_secs`, given the chapters' ascending
/// start times.
pub fn jump_target(
    mut starts: impl DoubleEndedIterator<Item = f64> + ExactSizeIterator,
    position_secs: f64,
    direction: ChapterDirection,
) -> Result<f64, ChapterJumpRefusal> {
    if starts.len() == 0 {
        return Err(ChapterJumpRefusal::NoChapters);
    }
    let target = match direction {
        ChapterDirection::Next => {
            starts.find(|&start| start > position_secs + BOUNDARY_HYSTERESIS_SECS)
        }
        ChapterDirection::Prev => starts
            .rev()
            .find(|&start| start < position_secs - BOUNDARY_HYSTERESIS_SECS),
    };
    target.ok_or(ChapterJumpRefusal::AtBoundary)
}

/// Index of the chapter containing `position_secs`. A position before the first chapter
/// belongs to the first chapter. `None` only when there are no chapters.
pub fn current_index(
    starts: impl DoubleEndedIterator<Item = f64> + ExactSizeIterator,
    position_secs: f64,
) -> Option<usize> {
    let mut starts = starts;
    if starts.len() == 0 {
        return None;
    }
    Some(
        starts
            .rposition(|start| start <= position_secs + BOUNDARY_HYSTERESIS_SECS)
            .unwrap_or(0),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const STARTS: [f64; 4] = [0.0, 60.0, 120.0, 180.0];

    fn jump(position: f64, direction: ChapterDirection) -> Result<f64, ChapterJumpRefusal> {
        jump_target(STARTS.iter().copied(), position, direction)
    }

    #[test]
    fn next_picks_the_first_boundary_after_the_playhead() {
        assert_eq!(jump(10.0, ChapterDirection::Next), Ok(60.0));
        assert_eq!(jump(60.0, ChapterDirection::Next), Ok(120.0));
        assert_eq!(jump(150.0, ChapterDirection::Next), Ok(180.0));
    }

    #[test]
    fn next_skips_a_boundary_within_the_hysteresis_margin() {
        assert_eq!(jump(59.6, ChapterDirection::Next), Ok(120.0));
        assert_eq!(jump(59.4, ChapterDirection::Next), Ok(60.0));
    }

    #[test]
    fn prev_picks_the_last_boundary_before_the_playhead() {
        assert_eq!(jump(130.0, ChapterDirection::Prev), Ok(120.0));
        assert_eq!(jump(121.0, ChapterDirection::Prev), Ok(120.0));
    }

    #[test]
    fn prev_threshold_restarts_the_current_chapter_only_after_half_a_second() {
        assert_eq!(jump(120.4, ChapterDirection::Prev), Ok(60.0));
        assert_eq!(jump(120.6, ChapterDirection::Prev), Ok(120.0));
    }

    #[test]
    fn boundaries_refuse_with_at_boundary() {
        assert_eq!(
            jump(180.0, ChapterDirection::Next),
            Err(ChapterJumpRefusal::AtBoundary)
        );
        assert_eq!(
            jump(500.0, ChapterDirection::Next),
            Err(ChapterJumpRefusal::AtBoundary)
        );
        assert_eq!(
            jump(0.0, ChapterDirection::Prev),
            Err(ChapterJumpRefusal::AtBoundary)
        );
        assert_eq!(
            jump(0.4, ChapterDirection::Prev),
            Err(ChapterJumpRefusal::AtBoundary)
        );
    }

    #[test]
    fn a_first_chapter_that_starts_after_zero_still_allows_a_jump_back_to_it() {
        let starts = [30.0, 90.0];
        assert_eq!(
            jump_target(starts.iter().copied(), 10.0, ChapterDirection::Next),
            Ok(30.0)
        );
        assert_eq!(
            jump_target(starts.iter().copied(), 10.0, ChapterDirection::Prev),
            Err(ChapterJumpRefusal::AtBoundary)
        );
    }

    #[test]
    fn an_empty_list_refuses_with_no_chapters_in_both_directions() {
        for direction in [ChapterDirection::Next, ChapterDirection::Prev] {
            assert_eq!(
                jump_target(std::iter::empty::<f64>(), 5.0, direction),
                Err(ChapterJumpRefusal::NoChapters)
            );
        }
        assert_eq!(current_index(std::iter::empty::<f64>(), 5.0), None);
    }

    #[test]
    fn refusal_reasons_are_stable() {
        assert_eq!(ChapterJumpRefusal::NoChapters.reason(), "no_chapters");
        assert_eq!(ChapterJumpRefusal::AtBoundary.reason(), "chapter_boundary");
    }

    #[test]
    fn current_index_follows_the_playhead() {
        let at = |position| current_index(STARTS.iter().copied(), position);
        assert_eq!(at(0.0), Some(0));
        assert_eq!(at(59.0), Some(0));
        assert_eq!(at(59.6), Some(1));
        assert_eq!(at(60.0), Some(1));
        assert_eq!(at(1_000.0), Some(3));
    }

    #[test]
    fn a_position_before_the_first_chapter_belongs_to_the_first() {
        assert_eq!(current_index([30.0, 90.0].iter().copied(), 5.0), Some(0));
    }
}
