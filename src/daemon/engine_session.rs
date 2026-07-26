//! Session-scoped scoring, persistence, and playback-outcome bookkeeping.

use super::*;

pub(super) const SESSION_EVENTS_CAP: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DaemonOutcome {
    FullPlay,
    Skip,
    QuickSkip,
    Like,
    Dislike,
}

#[derive(Debug, Clone)]
pub(super) struct DaemonSessionEvent {
    pub(super) artist_key: String,
    pub(super) outcome: DaemonOutcome,
    pub(super) completion: f32,
}

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

    pub(super) fn record_rating_session_event(
        &mut self,
        song: &Song,
        change: crate::rating::RatingChange,
    ) {
        let Some(signal) = crate::rating::session_signal(song, change) else {
            return;
        };
        let outcome = match signal {
            crate::rating::SessionRatingSignal::Like => DaemonOutcome::Like,
            crate::rating::SessionRatingSignal::Dislike => DaemonOutcome::Dislike,
        };
        self.record_session_event(
            &crate::signals::normalize_artist(&song.artist),
            outcome,
            self.playback_completion(),
        );
    }

    pub(super) fn session_cache_snapshot(&self) -> SessionCache {
        let mut cache = SessionCache::from_last_mode(self.last_mode);
        match self.last_mode {
            LastMode::Normal => {
                cache.normal_queue = Some(self.queue.snapshot());
                cache.radio_queue = self.inactive_radio_queue.as_deref().cloned();
                cache.local_queue = self.inactive_local_queue.as_deref().cloned();
            }
            LastMode::Radio => {
                cache.radio_queue = Some(self.queue.snapshot());
                cache.normal_queue = self.inactive_normal_queue.as_deref().cloned();
                cache.local_queue = self.inactive_local_queue.as_deref().cloned();
            }
            LastMode::Local => {
                cache.local_queue = Some(self.queue.snapshot());
                cache.normal_queue = self.inactive_normal_queue.as_deref().cloned();
                cache.radio_queue = self.inactive_radio_queue.as_deref().cloned();
            }
        }
        cache
    }
}

pub(super) fn data_dir() -> Option<PathBuf> {
    crate::paths::data_dir()
}
