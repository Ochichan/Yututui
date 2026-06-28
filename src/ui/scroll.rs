//! Shared list-scroll math.
//!
//! Every browse list (Library, Search, the Queue window, the AI suggestions) keeps one
//! [`ScrollState`]. The mouse wheel moves its [`offset`](ScrollState::offset) **directly and
//! independently of the selection** — the highlighted row may scroll out of view, matching
//! how desktop apps and browsers behave. Keyboard / click moves the selection instead, and
//! the render pass nudges the offset the *minimum* amount to keep that selection on-screen
//! with a margin of context (vim's `scrolloff`).
//!
//! Splitting the two means content moves by exactly the wheel delta — no re-centering lurch —
//! while keyboard navigation still never loses the cursor against an edge.

use std::cell::Cell;

/// Rows of context kept between the selected row and the viewport edge during keyboard
/// navigation (vim's `scrolloff`). Clamped to half the viewport on short list areas.
pub const SCROLLOFF: usize = 2;

/// Persistent scroll state for one list. `Cell` throughout because the render pass only
/// holds `&App` yet needs to record the viewport height and remember the offset across
/// frames (mirroring the [`crate::app::RenderBridges`] fields).
#[derive(Debug, Default)]
pub struct ScrollState {
    /// Index of the first visible row.
    offset: Cell<usize>,
    /// Selection seen on the previous frame, so [`resolve`](Self::resolve) can tell a
    /// keyboard / click move (re-center on the cursor) apart from a wheel scroll (leave the
    /// offset where the wheel put it).
    prev_sel: Cell<usize>,
    /// Viewport height (rows) recorded by the last render, so the wheel handler — which runs
    /// between frames — can clamp without a layout pass.
    viewport: Cell<u16>,
}

impl ScrollState {
    /// Reset to the top. Call when the list's *content* is replaced (new search results, a
    /// switched library tab, a reopened queue) so a stale offset doesn't carry over.
    pub fn reset(&self) {
        self.offset.set(0);
        self.prev_sel.set(0);
    }

    /// Mouse wheel: move the viewport by `delta` rows, decoupled from the selection and
    /// clamped to the content. `up` scrolls toward earlier items. A no-op before the first
    /// render records a viewport height.
    pub fn wheel(&self, up: bool, delta: usize, len: usize) {
        let viewport = self.viewport.get() as usize;
        if viewport == 0 {
            return;
        }
        let max = len.saturating_sub(viewport);
        let cur = self.offset.get();
        let next = if up { cur.saturating_sub(delta) } else { (cur + delta).min(max) };
        self.offset.set(next);
    }

    /// Render-time: record the viewport height and return the offset to draw this frame.
    ///
    /// If the selection moved since the last frame (keyboard / click) the offset is nudged to
    /// keep it visible with a `scrolloff` margin; otherwise the wheel-set offset is honored
    /// as-is (the selection may sit off-screen). Always clamped to the content.
    pub fn resolve(&self, selected: usize, viewport: u16, len: usize, scrolloff: usize) -> usize {
        self.viewport.set(viewport);
        let viewport = viewport as usize;
        let max = len.saturating_sub(viewport);
        let mut off = self.offset.get().min(max);
        if selected != self.prev_sel.get() {
            off = scroll_to_cursor(off, selected, viewport, len, scrolloff);
        }
        self.offset.set(off);
        self.prev_sel.set(selected);
        off
    }
}

/// The smallest offset shift from `offset` that brings `cursor` within `scrolloff` rows of
/// neither viewport edge — clamped to the content, and to half the viewport on short areas.
/// Pure (so it is unit-tested directly).
pub fn scroll_to_cursor(
    offset: usize,
    cursor: usize,
    viewport: usize,
    len: usize,
    scrolloff: usize,
) -> usize {
    if viewport == 0 {
        return 0;
    }
    let max = len.saturating_sub(viewport);
    // Never demand more padding than the viewport can hold, or the two limits would cross.
    let pad = scrolloff.min(viewport.saturating_sub(1) / 2);
    // To keep `cursor` >= offset + pad, the offset must be at most this:
    let top_limit = cursor.saturating_sub(pad);
    // To keep `cursor` <= offset + viewport - 1 - pad, the offset must be at least this:
    let bot_limit = (cursor + pad + 1).saturating_sub(viewport);
    // `bot_limit <= top_limit` always holds given the `pad` clamp, but guard the clamp anyway.
    let hi = top_limit.max(bot_limit);
    offset.clamp(bot_limit, hi).min(max)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SO: usize = SCROLLOFF;

    #[test]
    fn empty_and_underfull_lists_stay_at_top() {
        assert_eq!(scroll_to_cursor(0, 0, 10, 0, SO), 0); // empty
        assert_eq!(scroll_to_cursor(5, 3, 10, 4, SO), 0); // len < viewport
        let s = ScrollState::default();
        assert_eq!(s.resolve(0, 10, 0, SO), 0);
    }

    #[test]
    fn cursor_at_top_keeps_offset_zero() {
        assert_eq!(scroll_to_cursor(0, 0, 10, 100, SO), 0);
        // Cursor inside the top margin pulls the offset up to give it room.
        assert_eq!(scroll_to_cursor(5, 1, 10, 100, SO), 0);
    }

    #[test]
    fn cursor_at_bottom_shows_last_page() {
        // The very last item clamps to the final full page (no phantom rows past the end).
        assert_eq!(scroll_to_cursor(0, 99, 10, 100, SO), 90);
        // A few rows from the end: the minimal downward scroll that still keeps SO rows of
        // context below the cursor (rows 88..=97, cursor 95 at index 7, 2 rows below it).
        assert_eq!(scroll_to_cursor(0, 95, 10, 100, SO), 88);
    }

    #[test]
    fn margin_is_kept_on_both_sides() {
        // Cursor 50, offset far away -> land so 50 sits >= SO from each edge.
        let off = scroll_to_cursor(0, 50, 10, 100, SO);
        assert!(50 >= off + SO, "top margin: cursor {} offset {}", 50, off);
        assert!(50 <= off + 10 - 1 - SO, "bottom margin: cursor {} offset {}", 50, off);
    }

    #[test]
    fn scrolloff_larger_than_half_viewport_is_clamped() {
        // viewport 5, huge scrolloff -> pad clamps to (5-1)/2 = 2, never crosses.
        let off = scroll_to_cursor(0, 50, 5, 100, 999);
        assert!(off <= 50 && off + 5 > 50);
    }

    #[test]
    fn wheel_clamps_at_both_ends() {
        let s = ScrollState::default();
        s.resolve(0, 10, 100, SO); // record viewport = 10
        s.wheel(true, 3, 100); // already at top
        assert_eq!(s.resolve(0, 10, 100, SO), 0);
        for _ in 0..100 {
            s.wheel(false, 3, 100);
        }
        assert_eq!(s.resolve(0, 10, 100, SO), 90); // 100 - 10, never past the end
    }

    #[test]
    fn wheel_is_decoupled_from_selection() {
        let s = ScrollState::default();
        assert_eq!(s.resolve(0, 10, 100, SO), 0);
        s.wheel(false, 3, 100);
        s.wheel(false, 3, 100);
        // Re-render with the SAME selection: the wheel offset is honored, selection 0 is now
        // scrolled off the top (not snapped back).
        assert_eq!(s.resolve(0, 10, 100, SO), 6);
    }

    #[test]
    fn keyboard_move_recenters_after_a_wheel_scroll() {
        let s = ScrollState::default();
        s.resolve(0, 10, 100, SO);
        s.wheel(false, 3, 100); // viewport scrolled away from the selection
        // Selection jumps to 50 (keyboard): offset must move to show it with margin.
        let off = s.resolve(50, 10, 100, SO);
        assert!(50 >= off + SO && 50 <= off + 10 - 1 - SO);
    }

    #[test]
    fn content_reset_returns_to_top() {
        let s = ScrollState::default();
        s.resolve(0, 10, 100, SO);
        s.wheel(false, 3, 100);
        assert_eq!(s.resolve(0, 10, 100, SO), 3);
        s.reset();
        assert_eq!(s.resolve(0, 10, 100, SO), 0);
    }
}
