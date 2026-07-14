//! Session-only lyric synchronization state and timing helpers.

use std::time::{Duration, Instant};

use super::*;
use crate::lyrics::{LyricDelay, current_index_with_delay};

const LYRICS_DELAY_OSD_DURATION: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::app) enum LyricsDelayDirection {
    Earlier,
    Later,
}

impl App {
    /// Current synced lyrics only when every ownership boundary agrees. Restored queue state can
    /// name a song before mpv has loaded it, and a late actor result can belong to an older song;
    /// neither is interactive or eligible for the lyric clock.
    pub(crate) fn current_loaded_lyrics(&self) -> Option<&TrackLyrics> {
        if !self.lyrics.visible {
            return None;
        }
        let song = self.queue.current()?;
        if song.is_radio_station()
            || self.prefetch.loaded_video_id.as_deref() != Some(song.video_id.as_str())
        {
            return None;
        }
        let track = self.lyrics.track.as_ref()?;
        (track.video_id.as_ref() == song.video_id.as_str() && !track.lines.is_empty())
            .then_some(track)
    }

    /// Clear fetched lyric data while retaining the delay and its owning song across a reload.
    pub(in crate::app) fn clear_lyrics_data(&mut self) {
        self.lyrics.loading = false;
        self.lyrics.track = None;
        self.lyrics.active_index = None;
        self.lyrics.initial_osd_pending = false;
        self.lyrics.delay_osd_until = None;
    }

    /// Associate lyric controls with one admitted track. A same-id reload keeps its delay;
    /// changing the real id resets it before any new lyrics can become interactive.
    pub(in crate::app) fn associate_lyrics_with_track(&mut self, video_id: &str) {
        if self.lyrics.delay_video_id.as_deref() != Some(video_id) {
            self.lyrics.delay = LyricDelay::ZERO;
            self.lyrics.delay_video_id = Some(video_id.to_owned());
        }
        self.clear_lyrics_data();
    }

    /// Expand the delay control after a current, non-empty lyric payload arrives.
    pub(in crate::app) fn arm_lyrics_delay_osd(&mut self, now: Instant) -> bool {
        let Some(video_id) = self
            .current_loaded_lyrics()
            .map(|track| track.video_id.clone())
        else {
            return false;
        };
        self.ensure_lyrics_delay_owner(&video_id);
        self.lyrics.initial_osd_pending = false;
        self.lyrics.delay_osd_until = Some(lyrics_osd_deadline(now));
        true
    }

    /// Re-expand the collapsed handle. This is stricter than initial arming because a stale hit
    /// target must not act after leaving the full Player surface.
    pub(in crate::app) fn reopen_lyrics_delay_osd(&mut self, now: Instant) -> bool {
        self.lyrics_controls_available() && self.arm_lyrics_delay_osd(now)
    }

    /// Collapse an expired delay control. Returns whether visible state changed.
    pub(in crate::app) fn expire_lyrics_delay_osd(&mut self, now: Instant) -> bool {
        if self
            .lyrics
            .delay_osd_until
            .is_some_and(|deadline| now >= deadline)
        {
            self.lyrics.delay_osd_until = None;
            return true;
        }
        false
    }

    /// Apply one exact 100 ms step, refresh the current line, and restart the OSD deadline.
    /// Hidden, mini, stale, live, empty, and not-yet-loaded states fail closed.
    pub(in crate::app) fn adjust_lyrics_delay(
        &mut self,
        direction: LyricsDelayDirection,
        now: Instant,
    ) -> bool {
        if !self.lyrics_controls_available() {
            return false;
        }
        let Some(video_id) = self
            .current_loaded_lyrics()
            .map(|track| track.video_id.clone())
        else {
            return false;
        };
        self.ensure_lyrics_delay_owner(&video_id);
        self.lyrics.delay = match direction {
            LyricsDelayDirection::Earlier => self.lyrics.delay.earlier(),
            LyricsDelayDirection::Later => self.lyrics.delay.later(),
        };
        self.lyrics.initial_osd_pending = false;
        self.lyrics.delay_osd_until = Some(lyrics_osd_deadline(now));
        self.refresh_lyrics_active_at(now);
        true
    }

    /// Interpolate the transport position from its last mpv report without consulting animation
    /// settings. Paused playback uses the reported base exactly.
    pub(in crate::app) fn interpolated_lyrics_position_at(&self, now: Instant) -> Option<f64> {
        let mut position = self.playback.time_pos?;
        if !position.is_finite() {
            return None;
        }
        if !self.playback.paused {
            let anchor = self.playback.time_pos_at?;
            let speed = self.playback.speed;
            if !speed.is_finite() || speed < 0.0 {
                return None;
            }
            position += now.saturating_duration_since(anchor).as_secs_f64() * speed;
        }
        if !position.is_finite() {
            return None;
        }
        match self.playback.duration {
            Some(duration) if duration.is_finite() && duration >= 0.0 => {
                Some(position.clamp(0.0, duration))
            }
            Some(_) => None,
            None => Some(position.max(0.0)),
        }
    }

    /// Recompute the one stored lyric index. Returns true only when the highlighted line changed.
    pub(in crate::app) fn refresh_lyrics_active_at(&mut self, now: Instant) -> bool {
        let next = self
            .interpolated_lyrics_position_at(now)
            .and_then(|position| {
                self.current_loaded_lyrics().and_then(|track| {
                    current_index_with_delay(&track.lines, position, self.lyrics.delay)
                })
            });
        if self.lyrics.active_index == next {
            return false;
        }
        self.lyrics.active_index = next;
        true
    }

    /// Reconcile state that must agree with the currently visible lyric surface. This runs after
    /// every reducer turn, so returning while paused and an admitted paused seek both refresh the
    /// stored index without depending on the playback clock. A fresh payload's initial OSD is
    /// armed only here, once a full, focused Player frame can actually show it.
    pub(in crate::app) fn reconcile_lyrics_surface_at(&mut self, now: Instant) -> bool {
        if self.mode != Mode::Player
            || self.bridges.ui_tier.get() != crate::ui::layout::UiTier::Full
            || !self.focused
        {
            return false;
        }
        let active_changed = self.refresh_lyrics_active_at(now);
        let osd_opened = self.lyrics.initial_osd_pending && self.arm_lyrics_delay_osd(now);
        active_changed || osd_opened
    }

    /// The dedicated 100 ms lyric clock is fully parked outside this exact state.
    pub(crate) fn lyrics_clock_active(&self) -> bool {
        self.mode == Mode::Player
            && self.bridges.ui_tier.get() == crate::ui::layout::UiTier::Full
            && self.focused
            && !self.playback.paused
            && self.playback.speed.is_finite()
            && self.playback.speed > 0.0
            && self.playback.time_pos.is_some_and(f64::is_finite)
            && self.playback.time_pos_at.is_some()
            && self
                .playback
                .duration
                .is_none_or(|duration| duration.is_finite() && duration >= 0.0)
            && self.current_loaded_lyrics().is_some()
    }

    /// Process one lyric-clock instant, changing state only on a line boundary.
    pub(in crate::app) fn lyrics_tick_at(&mut self, now: Instant) -> bool {
        self.lyrics_clock_active() && self.refresh_lyrics_active_at(now)
    }

    /// Validated seek position for a rendered lyric hit target. The caller still sends this
    /// through `PlayerIntent`; calculating the target never mutates transport state or its epoch.
    pub(in crate::app) fn lyrics_line_seek_target(
        &self,
        video_id: &str,
        line_index: usize,
    ) -> Option<f64> {
        if !self.lyrics_controls_available() {
            return None;
        }
        let track = self.current_loaded_lyrics()?;
        if track.video_id.as_ref() != video_id {
            return None;
        }
        let timestamp = track.lines.get(line_index)?.time;
        self.lyrics
            .delay
            .seek_position(timestamp, self.playback.duration?)
    }

    fn lyrics_controls_available(&self) -> bool {
        self.mode == Mode::Player
            && self.bridges.ui_tier.get() == crate::ui::layout::UiTier::Full
            && self.current_loaded_lyrics().is_some()
    }

    fn ensure_lyrics_delay_owner(&mut self, video_id: &str) {
        if self.lyrics.delay_video_id.as_deref() != Some(video_id) {
            self.lyrics.delay = LyricDelay::ZERO;
            self.lyrics.delay_video_id = Some(video_id.to_owned());
        }
    }
}

fn lyrics_osd_deadline(now: Instant) -> Instant {
    now.checked_add(LYRICS_DELAY_OSD_DURATION).unwrap_or(now)
}
