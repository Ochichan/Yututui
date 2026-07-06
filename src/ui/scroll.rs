//! Shared list-scroll math.
//!
//! Every browse list (Library, Search, the Queue window, the DJ Gem suggestions) keeps one
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
    /// One-shot request for append-only views (the DJ Gem transcript) to render the newest
    /// content at the bottom after new lines arrive.
    follow_tail: Cell<bool>,
}

impl ScrollState {
    /// Reset to the top. Call when the list's *content* is replaced (new search results, a
    /// switched library tab, a reopened queue) so a stale offset doesn't carry over.
    pub fn reset(&self) {
        self.offset.set(0);
        self.prev_sel.set(0);
        self.follow_tail.set(false);
    }

    /// Append-only views call this when fresh content should snap the viewport to the newest
    /// lines on the next render. A later user wheel/scrollbar move cancels the pending snap.
    pub fn scroll_to_end(&self) {
        self.follow_tail.set(true);
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
        let next = if up {
            cur.saturating_sub(delta)
        } else {
            // Saturating like the up-branch — a huge `delta` must clamp, not wrap past `max`.
            cur.saturating_add(delta).min(max)
        };
        self.follow_tail.set(false);
        self.offset.set(next);
    }

    /// Directly place the viewport offset, clamped using the last rendered viewport height.
    /// Used by mouse-dragging a scrollbar thumb; before the first render this is a no-op
    /// because there is no track geometry to interpret.
    pub fn set_offset(&self, offset: usize, len: usize) {
        let viewport = self.viewport.get() as usize;
        if viewport == 0 {
            return;
        }
        self.follow_tail.set(false);
        self.offset.set(offset.min(len.saturating_sub(viewport)));
    }

    /// Current first visible row. Kept public for reducer tests and for scrollbars that need
    /// to derive a grab offset from the last rendered thumb.
    pub fn offset(&self) -> usize {
        self.offset.get()
    }

    /// Last viewport height recorded by render.
    pub fn viewport(&self) -> usize {
        self.viewport.get() as usize
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

    /// Render-time offset for a list that has **no active selection this frame** (e.g. the
    /// unfocused column of the Keys tab). Records the viewport and clamps the stored offset to
    /// the content, but never re-anchors to a cursor — so the column keeps its place instead of
    /// snapping when focus moves elsewhere.
    pub fn view(&self, viewport: u16, len: usize) -> usize {
        self.viewport.set(viewport);
        let max = len.saturating_sub(viewport as usize);
        let off = self.offset.get().min(max);
        self.offset.set(off);
        off
    }

    /// Render-time offset for an append-only transcript. The first render, and any render after
    /// [`scroll_to_end`](Self::scroll_to_end), pins the viewport to the newest lines. Otherwise it
    /// preserves the user's wheel/scrollbar position like [`view`](Self::view).
    pub fn view_tail(&self, viewport: u16, len: usize) -> usize {
        let first_render = self.viewport.get() == 0;
        self.viewport.set(viewport);
        let max = len.saturating_sub(viewport as usize);
        let follow_tail = self.follow_tail.get();
        self.follow_tail.set(false);
        let off = if first_render || follow_tail {
            max
        } else {
            self.offset.get().min(max)
        };
        self.offset.set(off);
        off
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollbarThumb {
    /// Row offset from the top of the scrollbar track.
    pub start: u16,
    /// Thumb height in rows.
    pub len: u16,
}

/// Browser-style scrollbar geometry for a list viewport.
///
/// `position` is the first visible row and therefore ranges over `0..=content_len -
/// viewport`. The thumb's top edge is placed from that row's position in the whole content,
/// then clamped so the final page still reaches the end of the track.
pub fn scrollbar_thumb(
    content_len: usize,
    viewport: usize,
    track_len: u16,
    position: usize,
) -> Option<ScrollbarThumb> {
    if content_len <= viewport || track_len == 0 {
        return None;
    }
    let track = track_len as usize;
    let max_offset = content_len - viewport;
    let thumb_len = viewport
        .saturating_mul(track)
        .div_ceil(content_len)
        .clamp(1, track);
    let travel = track.saturating_sub(thumb_len);
    let pos = position.min(max_offset);
    let start = pos.saturating_mul(track) / content_len;
    Some(ScrollbarThumb {
        start: start.min(travel) as u16,
        len: thumb_len as u16,
    })
}

/// Convert a row within a scrollbar track back into a list offset.
///
/// `grab` is the row inside the thumb that the pointer is holding. Keeping it stable makes
/// dragging the middle or bottom of the thumb behave without a jump on the first drag event.
pub fn offset_from_scrollbar_row(
    row: u16,
    grab: u16,
    content_len: usize,
    viewport: usize,
    track_len: u16,
) -> usize {
    let Some(thumb) = scrollbar_thumb(content_len, viewport, track_len, 0) else {
        return 0;
    };
    let track = track_len as usize;
    let max_offset = content_len - viewport;
    let thumb_len = thumb.len as usize;
    let travel = track.saturating_sub(thumb_len);
    if travel == 0 {
        return 0;
    }
    let grab = (grab as usize).min(thumb_len.saturating_sub(1));
    let row = (row as usize).min(track.saturating_sub(1));
    let thumb_start = row.saturating_sub(grab).min(travel);
    if thumb_start == travel {
        return max_offset;
    }
    (thumb_start.saturating_mul(content_len) + track / 2)
        .checked_div(track)
        .unwrap_or(0)
        .min(max_offset)
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
        assert!(
            50 <= off + 10 - 1 - SO,
            "bottom margin: cursor {} offset {}",
            50,
            off
        );
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

    #[test]
    fn scrolloff_zero_click_in_place_does_not_scroll() {
        // The Settings form passes scrolloff = 0: clicking any already-visible row must leave
        // the offset untouched (the row stays exactly where it was on screen).
        assert_eq!(scroll_to_cursor(5, 5, 10, 20, 0), 5); // topmost visible row
        assert_eq!(scroll_to_cursor(5, 14, 10, 20, 0), 5); // bottommost visible row
        assert_eq!(scroll_to_cursor(5, 9, 10, 20, 0), 5); // mid-viewport
        // Keyboard nav one row past an edge still scrolls — by exactly one row, no lurch.
        assert_eq!(scroll_to_cursor(5, 15, 10, 20, 0), 6); // one below the bottom
        assert_eq!(scroll_to_cursor(5, 4, 10, 20, 0), 4); // one above the top
    }

    #[test]
    fn resolve_scrolloff_zero_always_keeps_cursor_visible() {
        let s = ScrollState::default();
        for cursor in [30usize, 0, 49, 25] {
            let off = s.resolve(cursor, 10, 50, 0);
            assert!(
                cursor >= off && cursor < off + 10,
                "cursor {cursor} not visible at off {off}"
            );
        }
    }

    #[test]
    fn view_clamps_without_re_anchoring() {
        let s = ScrollState::default();
        // A column with no selection keeps whatever offset it had, clamped to the content.
        s.resolve(40, 10, 50, 0); // cursor pushed the offset down
        let parked = s.resolve(40, 10, 50, 0);
        assert_eq!(s.view(10, 50), parked); // view() honors the parked offset, no snap
        // Clamps when the content shrinks below the stored offset.
        assert_eq!(s.view(10, 5), 0);
    }

    #[test]
    fn scrollbar_thumb_reaches_track_end_at_last_page() {
        let top = scrollbar_thumb(40, 15, 15, 0).unwrap();
        assert_eq!(top.start, 0);

        let bottom = scrollbar_thumb(40, 15, 15, 25).unwrap();
        assert_eq!(
            bottom.start + bottom.len,
            15,
            "last page should put the thumb against the track bottom"
        );
    }

    #[test]
    fn scrollbar_thumb_top_tracks_whole_content_position() {
        let thumb = scrollbar_thumb(40, 10, 12, 12).unwrap();
        assert_eq!(
            thumb.start, 3,
            "first visible row 12/40 should map through the whole 12-row track"
        );
    }

    #[test]
    fn scrollbar_track_row_maps_back_to_scroll_offset() {
        assert_eq!(offset_from_scrollbar_row(0, 0, 40, 15, 15), 0);
        assert_eq!(offset_from_scrollbar_row(14, 0, 40, 15, 15), 25);
        assert_eq!(offset_from_scrollbar_row(3, 0, 40, 10, 12), 10);

        let thumb = scrollbar_thumb(40, 15, 15, 0).unwrap();
        let center_grab = thumb.len / 2;
        assert_eq!(offset_from_scrollbar_row(14, center_grab, 40, 15, 15), 25);
    }
}
