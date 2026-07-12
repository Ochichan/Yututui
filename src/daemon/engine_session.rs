//! Session-scoped scoring, persistence, and playback-outcome bookkeeping.

use super::*;

impl DaemonEngine {
    pub(super) fn session_artist_bias(&self) -> HashMap<String, f32> {
        let mut out: HashMap<String, f32> = HashMap::new();
        for event in self.session_events.iter().rev().take(8) {
            let completion = event.completion.clamp(0.0, 1.0);
            let delta = match event.outcome {
                DaemonOutcome::FullPlay => 0.05 * completion.max(0.5),
                DaemonOutcome::Skip => -0.10 * (1.0 - completion).max(0.25),
                DaemonOutcome::QuickSkip => -0.20 * (1.0 - completion).max(0.5),
            };
            let entry = out.entry(event.artist_key.clone()).or_insert(0.0);
            *entry = (*entry + delta).clamp(-0.50, 0.35);
        }
        out
    }

    pub(super) fn streaming_skip_streak(&self) -> usize {
        self.session_events
            .iter()
            .rev()
            .take_while(|event| {
                matches!(
                    event.outcome,
                    DaemonOutcome::Skip | DaemonOutcome::QuickSkip
                )
            })
            .count()
    }

    pub(super) fn record_outgoing(&mut self, full: bool) {
        let Some(song) = self.queue.current().cloned() else {
            return;
        };
        if song.is_radio_station() {
            return;
        }
        let artist_key = signals::normalize_artist(&song.artist);
        let now = signals::unix_now();
        let (outcome, completion) = if full {
            self.signals
                .record_play(&song.video_id, &artist_key, 1.0, now);
            (DaemonOutcome::FullPlay, 1.0)
        } else {
            let completion = self.playback_completion();
            self.signals
                .record_skip(&song.video_id, &artist_key, completion, now, 0.6);
            let outcome = if completion < signals::STRONG_SKIP_FRAC {
                DaemonOutcome::QuickSkip
            } else {
                DaemonOutcome::Skip
            };
            (outcome, completion)
        };
        self.record_session_event(&artist_key, outcome, completion);
        if let Err(error) = self.signals.save() {
            tracing::warn!(error = %error, "failed to save daemon signals");
        }
    }

    pub(super) fn playback_completion(&self) -> f32 {
        match (self.playback.time_pos, self.playback.duration) {
            (Some(time), Some(duration)) if duration > 0.0 => {
                (time / duration).clamp(0.0, 1.0) as f32
            }
            _ => 0.5,
        }
    }

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

    pub(super) fn save_session(&self) {
        if let Err(error) = self.session_cache_snapshot().save() {
            tracing::warn!(error = %error, "failed to save daemon session");
        }
    }
}

pub(super) fn local_neighbor_score(song: &Song, seed_artist_key: &str, sig: &Signals) -> f32 {
    let artist_key = signals::normalize_artist(&song.artist);
    let seed_bonus = if artist_key == seed_artist_key {
        1.0
    } else {
        0.0
    };
    seed_bonus + sig.artist_weight(&artist_key)
}

pub(super) fn data_dir() -> Option<PathBuf> {
    crate::paths::data_dir()
}
