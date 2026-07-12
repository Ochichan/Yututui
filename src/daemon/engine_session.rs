//! Session-scoped scoring, persistence, and playback-outcome bookkeeping.

use super::*;

impl DaemonEngine {
    pub(super) fn record_session_event(
        &mut self,
        artist_key: &str,
        outcome: DaemonOutcome,
        completion: f32,
    ) {
        self.session_events.push_back(DaemonSessionEvent {
            artist_key: artist_key.to_owned(),
            outcome,
            completion,
        });
        while self.session_events.len() > SESSION_EVENTS_CAP {
            self.session_events.pop_front();
        }
    }

    pub(super) fn session_cache_snapshot(&self) -> SessionCache {
        let mut cache = SessionCache::from_last_mode(self.last_mode);
        match self.last_mode {
            LastMode::Normal => {
                cache.normal_queue = Some(self.queue.snapshot());
                cache.radio_queue = self.inactive_radio_queue.clone();
                cache.local_queue = self.inactive_local_queue.clone();
            }
            LastMode::Radio => {
                cache.radio_queue = Some(self.queue.snapshot());
                cache.normal_queue = self.inactive_normal_queue.clone();
                cache.local_queue = self.inactive_local_queue.clone();
            }
            LastMode::Local => {
                cache.local_queue = Some(self.queue.snapshot());
                cache.normal_queue = self.inactive_normal_queue.clone();
                cache.radio_queue = self.inactive_radio_queue.clone();
            }
        }
        cache
    }
}

pub(super) fn data_dir() -> Option<PathBuf> {
    crate::paths::data_dir()
}
