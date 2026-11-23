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

    /// Next monotonic wall-clock instant at which the highlighted synced-lyric line can change.
    ///
    /// Returns `None` when the lyric clock is parked (paused, unfocused, hidden, non-Player, Mini
    /// tier, or already on/past the last reachable line), `Some(now)` when `active_index` is
    /// already behind the interpolated position, or the exact boundary instant scaled by playback
    /// speed plus a 1 ms rounding margin.
    pub(crate) fn next_lyrics_deadline_at(&self, now: Instant) -> Option<Instant> {
        if !self.lyrics_clock_active() {
            return None;
        }
        let position = self.interpolated_lyrics_position_at(now)?;
        let track = self.current_loaded_lyrics()?;
        let active_pos = self.lyrics.delay.active_position(position);
        let pp = track.lines.partition_point(|line| line.time <= active_pos);
        let current = pp.checked_sub(1);
        if self.lyrics.active_index != current {
            return Some(now);
        }
        let next_line = track.lines.get(pp)?;
        let target_playback_pos = next_line.time + self.lyrics.delay.seconds();
        if !target_playback_pos.is_finite() {
            return None;
        }
        if let Some(duration) = self.playback.duration
            && (target_playback_pos > duration || position >= duration)
        {
            return None;
        }
        let delta_playback = target_playback_pos - position;
        if delta_playback <= 0.0 {
            return now.checked_add(Duration::from_millis(1));
        }
        let delta_wall_secs = delta_playback / self.playback.speed;
        let wait = Duration::try_from_secs_f64(delta_wall_secs)
            .ok()?
            .checked_add(Duration::from_millis(1))?;
        now.checked_add(wait)
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::api::Song;
    use crate::lyrics::{LyricDelay, LyricLine};
    use crate::ui::layout::UiTier;

    fn app_with_synced_lyrics(times: &[f64], now: Instant, pos: f64, speed: f64) -> App {
        let mut app = App::new(100);
        app.mode = Mode::Player;
        app.focused = true;
        app.bridges.ui_tier.set(UiTier::Full);
        let song = Song::remote("vid-1", "Song", "Artist", "3:00");
        app.queue.set(vec![song], 0);
        app.prefetch.loaded_video_id = Some("vid-1".to_owned());
        app.playback.paused = false;
        app.playback.speed = speed;
        app.playback.time_pos = Some(pos);
        app.playback.time_pos_at = Some(now);
        app.playback.duration = Some(180.0);
        app.lyrics.visible = true;
        app.lyrics.track = Some(TrackLyrics {
            video_id: Arc::from("vid-1"),
            lines: Arc::from(
                times
                    .iter()
                    .enumerate()
                    .map(|(i, &time)| LyricLine {
                        time,
                        text: format!("line {i}"),
                    })
                    .collect::<Vec<_>>(),
            ),
        });
        app.refresh_lyrics_active_at(now);
        app
    }

    #[test]
    fn lyrics_deadline_tracks_next_boundary_speed_delay_seek_pause_and_end() {
        let t0 = Instant::now();
        let mut app = app_with_synced_lyrics(&[5.0, 10.0, 10.0, 20.0], t0, 6.0, 1.0);
        assert_eq!(app.lyrics.active_index, Some(0));

        // Next boundary is 10.0s -> 4.0s + 1ms at 1.0x speed.
        let d1 = app
            .next_lyrics_deadline_at(t0)
            .expect("deadline before next line");
        assert_eq!(d1.duration_since(t0), Duration::from_millis(4_001));

        // At 2.0x speed, the same 4.0s playback delta takes 2.0s + 1ms wall time.
        app.playback.speed = 2.0;
        let d2 = app
            .next_lyrics_deadline_at(t0)
            .expect("deadline at 2.0x speed");
        assert_eq!(d2.duration_since(t0), Duration::from_millis(2_001));

        // At 0.5x speed, 4.0s playback delta takes 8.0s + 1ms wall time.
        app.playback.speed = 0.5;
        let d3 = app
            .next_lyrics_deadline_at(t0)
            .expect("deadline at 0.5x speed");
        assert_eq!(d3.duration_since(t0), Duration::from_millis(8_001));
        app.playback.speed = 1.0;

        // Adjusting delay +0.5s delays the 10.0s line to 10.5s -> 4.5s + 1ms.
        app.lyrics.delay = LyricDelay::from_steps(5);
        app.refresh_lyrics_active_at(t0);
        let d_delay = app
            .next_lyrics_deadline_at(t0)
            .expect("deadline after delay shift");
        assert_eq!(d_delay.duration_since(t0), Duration::from_millis(4_501));
        app.lyrics.delay = LyricDelay::ZERO;

        // Seeking onto duplicate timestamp 10.0s advances active_index to the last duplicate (2)
        // and schedules the 20.0s line (10.0s away), never spinning on duplicates.
        app.playback.time_pos = Some(10.0);
        app.playback.time_pos_at = Some(t0);
        app.refresh_lyrics_active_at(t0);
        assert_eq!(app.lyrics.active_index, Some(2));
        let d_dup = app
            .next_lyrics_deadline_at(t0)
            .expect("deadline after duplicate timestamp");
        assert_eq!(d_dup.duration_since(t0), Duration::from_millis(10_001));

        // If time advances past 20.0s before refresh, deadline fires immediately (`Some(now)`).
        let t_late = t0 + Duration::from_secs(11);
        assert_eq!(app.next_lyrics_deadline_at(t_late), Some(t_late));
        assert!(app.lyrics_tick_at(t_late));
        assert_eq!(app.lyrics.active_index, Some(3));

        // Past the final line -> fully parked (`None`).
        assert_eq!(app.next_lyrics_deadline_at(t_late), None);

        // Pausing parks immediately even before the final line.
        app.playback.time_pos = Some(6.0);
        app.playback.time_pos_at = Some(t0);
        app.refresh_lyrics_active_at(t0);
        app.playback.paused = true;
        assert_eq!(app.next_lyrics_deadline_at(t0), None);

        // Resuming re-arms immediately.
        app.playback.paused = false;
        assert!(app.next_lyrics_deadline_at(t0).is_some());

        // Switching track clears lyrics and parks until new lyrics load.
        app.associate_lyrics_with_track("vid-2");
        assert_eq!(app.next_lyrics_deadline_at(t0), None);
    }

    #[test]
    fn lyrics_deadline_parks_when_next_line_exceeds_duration() {
        let t0 = Instant::now();
        let mut app = app_with_synced_lyrics(&[5.0, 250.0], t0, 6.0, 1.0);
        app.playback.duration = Some(180.0);
        assert_eq!(app.next_lyrics_deadline_at(t0), None);
    }

    #[test]
    fn lyrics_deadline_never_rearms_immediately_on_float_rounding_or_duration_clamp() {
        let t0 = Instant::now();
        // `256.2 - 0.3` rounds just below `255.9` while `255.9 + 0.3 == 256.2`, leaving
        // `delta_playback == 0.0`. At the duration clamp the position cannot advance further,
        // so the clock must park instead of spinning on `Some(now)`.
        let mut app = app_with_synced_lyrics(&[5.0, 255.9], t0, 256.2, 1.0);
        app.playback.duration = Some(256.2);
        app.lyrics.delay = LyricDelay::from_steps(3);
        app.refresh_lyrics_active_at(t0);
        assert_eq!(app.next_lyrics_deadline_at(t0), None);

        // Before the duration clamp, the same float-rounding edge sleeps 1 ms so playback
        // advances past the boundary instead of returning an already-due deadline.
        app.playback.duration = Some(300.0);
        assert_eq!(
            app.next_lyrics_deadline_at(t0),
            Some(t0 + Duration::from_millis(1))
        );
    }
}
