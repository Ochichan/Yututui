//! Last-session restore and snapshot helpers.

use super::*;

impl App {
    /// Seed the player with the last locally recorded track, without starting playback.
    /// This gives a fresh launch something useful to show while keeping autoplay opt-in.
    pub fn restore_last_session_from_library(&mut self, radio_mode: bool) {
        if radio_mode {
            self.restore_last_radio_from_library();
        } else {
            self.restore_last_played_from_library();
        }
    }

    pub fn restore_last_played_from_library(&mut self) {
        if !self.queue.is_empty() {
            return;
        }
        let Some(song) = self.library.history.front().cloned() else {
            return;
        };
        self.seed_restored_queue(song);
    }

    /// Restore dedicated Radio mode and seed the last played radio station, without starting
    /// playback. The station itself comes from the persisted radio history.
    pub fn restore_last_radio_from_library(&mut self) {
        if !self.radio_dedicated_mode {
            self.radio_mode.normal_mode_theme = Some(self.theme.clone());
        }
        self.activate_radio_dedicated_mode_ui();
        if !self.queue.is_empty() {
            return;
        }
        let Some(station) = self.library.radios.front().cloned() else {
            return;
        };
        self.seed_restored_queue(station);
    }

    fn seed_restored_queue(&mut self, song: Song) {
        self.queue.set(vec![song], 0);
        self.seed_restored_playback_state();
    }

    /// Build the persisted session cache from the active queue plus the inactive mode's stashed
    /// queue. This is the handoff used by both the next TUI launch and the headless daemon.
    pub fn session_cache_snapshot(&self) -> crate::session::SessionCache {
        let last_mode = if self.radio_dedicated_mode {
            crate::session::LastMode::Radio
        } else if self.local_dedicated_mode {
            crate::session::LastMode::Local
        } else {
            crate::session::LastMode::Normal
        };
        let mut cache = crate::session::SessionCache::from_last_mode(last_mode);
        match last_mode {
            crate::session::LastMode::Normal => {
                cache.normal_queue = Some(self.queue.snapshot());
                cache.radio_queue = self.radio_mode.radio_mode_queue.clone();
                cache.local_queue = self.local_mode.local_mode_queue.clone();
            }
            crate::session::LastMode::Radio => {
                cache.radio_queue = Some(self.queue.snapshot());
                cache.normal_queue = self.radio_mode.normal_mode_queue.clone();
                cache.local_queue = self.local_mode.local_mode_queue.clone();
            }
            crate::session::LastMode::Local => {
                cache.local_queue = Some(self.queue.snapshot());
                cache.normal_queue = self.local_mode.normal_mode_queue.clone();
                cache.radio_queue = self.radio_mode.radio_mode_queue.clone();
            }
        }
        cache
    }

    /// Restore an exact queue snapshot when one exists; fall back to the legacy library-history
    /// restore path for old session files.
    pub fn restore_last_session_from_cache(&mut self, cache: &crate::session::SessionCache) {
        self.radio_mode.normal_mode_queue = cache.normal_queue.clone();
        self.radio_mode.radio_mode_queue = cache.radio_queue.clone();
        self.local_mode.normal_mode_queue = cache.normal_queue.clone();
        self.local_mode.local_mode_queue = cache.local_queue.clone();

        if cache.was_radio_mode() {
            self.activate_radio_dedicated_mode_ui();
        } else if cache.was_local_mode() {
            if !self.local_dedicated_mode {
                self.local_mode.normal_mode_theme = Some(self.theme.clone());
            }
            self.activate_local_dedicated_mode_ui();
        }

        if let Some(snapshot) = cache.active_queue().cloned() {
            self.queue.restore_snapshot(snapshot);
            self.seed_restored_playback_state();
            return;
        }

        if cache.was_local_mode() {
            return;
        }

        self.restore_last_session_from_library(cache.was_radio_mode());
    }

    fn seed_restored_playback_state(&mut self) {
        self.playback.time_pos = None;
        self.playback.time_pos_at = None;
        self.bump_position_epoch(PositionEpochReason::RestoreSession);
        self.playback.duration = None;
        self.playback.paused = true;
        self.playback.stream_now_playing = None;
        self.anim.last_shown_sec = -1;
        self.prefetch.loaded_video_id = None;
        self.clear_status();
        self.dirty = true;
    }

    /// Opt-in: when "autoplay on launch" is enabled and [`restore_last_played_from_library`]
    /// seeded a track, start playing it at launch - the same path pressing play would take
    /// (load -> record -> prefetch). Returns no commands when the setting is off or nothing was
    /// restored, leaving the queue paused and idle (the default). Called once at startup.
    ///
    /// [`restore_last_played_from_library`]: Self::restore_last_played_from_library
    pub fn autoplay_on_start_cmds(&mut self) -> Vec<Cmd> {
        if !self.config.effective_autoplay_on_start() || !self.current_needs_load() {
            return Vec::new();
        }
        self.stay_on_current_track()
    }
}
