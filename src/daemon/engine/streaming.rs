//! Daemon autoplay extension, local candidate planning, and session-bias policy.
//!
//! This remains an implementation block on [`DaemonEngine`]: ownership and orchestration stay
//! in the engine while the coherent streaming policy is kept out of the main command surface.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use crate::api::Song;
use crate::playback_policy::{
    AUTOPLAY_COOLDOWN, AUTOPLAY_MAX_FAILURES, AUTOPLAY_THRESHOLD, STREAMING_FALLBACK_COUNT,
    STREAMING_POOL_COUNT,
};
use crate::signals::{self, Signals};
use crate::streaming::{self, CandidateSource, Cooc, StationState, StreamingMode};

use super::{DaemonEngine, DaemonOutcome, EngineEffect};

impl DaemonEngine {
    pub(super) fn maybe_autoplay_extend(&mut self) -> Vec<EngineEffect> {
        self.autoplay_extend(false)
    }

    pub(super) fn force_autoplay_extend(&mut self) -> Vec<EngineEffect> {
        self.autoplay_extend(true)
    }

    fn autoplay_extend(&mut self, force: bool) -> Vec<EngineEffect> {
        if !self.streaming || self.streaming_pending {
            return Vec::new();
        }
        if !force && self.queue.remaining() > AUTOPLAY_THRESHOLD {
            return Vec::new();
        }
        if !force
            && self
                .last_extend
                .is_some_and(|t| t.elapsed() < AUTOPLAY_COOLDOWN)
        {
            return Vec::new();
        }
        let Some(cur) = self.queue.current() else {
            return Vec::new();
        };
        if cur.is_radio_station() {
            return Vec::new();
        }

        let seed = format!("{} — {}", cur.title, cur.artist);
        let seed_video_id = cur.video_id.clone();
        let exclude_ids = self.streaming_exclude_ids(&seed_video_id);
        self.last_extend = Some(Instant::now());
        self.streaming_pending = true;
        vec![EngineEffect::StreamingFallback {
            seed,
            seed_video_id,
            exclude_ids,
            limit: STREAMING_POOL_COUNT,
            mode: self.config.streaming.mode,
            config: self.config.effective_search(),
        }]
    }

    pub(crate) fn streaming_exclude_ids(&self, seed_video_id: &str) -> Vec<String> {
        // Shared with the TUI App reducer — one implementation, so the two owners can never
        // drift on which already-heard/queued tracks an autoplay top-up excludes.
        crate::streaming::exclude_ids(
            &self.config.streaming,
            &self.queue,
            &self.library,
            seed_video_id,
        )
    }

    pub(super) fn plan_local_streaming(
        &mut self,
        seed_video_id: &str,
        mut candidates: Vec<(Song, CandidateSource)>,
    ) -> Vec<Song> {
        let st = self.build_station_state(seed_video_id);
        let cooc = Cooc::build(self.signals.play_log(), &self.config.streaming.cooc);
        self.augment_streaming_candidates(seed_video_id, &mut candidates);
        let pool = streaming::pool_from_tagged(candidates);
        streaming::plan_local(
            pool,
            &st,
            &self.signals,
            &cooc,
            &self.config.streaming,
            STREAMING_FALLBACK_COUNT,
            signals::unix_now(),
        )
    }

    pub(super) async fn extend_sanitized_streaming(
        &mut self,
        seed_video_id: &str,
        songs: Vec<Song>,
        fallback: &[Song],
    ) -> Vec<EngineEffect> {
        let sanitized = streaming::sanitize_final_picks(
            songs,
            fallback,
            self.config.streaming.mode,
            &self.config.streaming,
        );
        if !sanitized.is_empty()
            && streaming::final_preflight_needed(
                &sanitized,
                fallback,
                self.config.streaming.mode,
                &self.config.streaming,
            )
        {
            self.streaming_pending = true;
            return vec![EngineEffect::StreamingPreflight {
                seed_video_id: seed_video_id.to_owned(),
                picks: sanitized,
                fallback: fallback.to_vec(),
                mode: self.config.streaming.mode,
                config: self.config.streaming.clone(),
            }];
        }
        self.extend_queue_from_streaming(sanitized).await
    }

    pub(super) async fn extend_queue_from_streaming(
        &mut self,
        songs: Vec<Song>,
    ) -> Vec<EngineEffect> {
        let slot = self.streaming_mode_slot();
        self.extend_queue_from_picks(songs, &slot).await
    }

    /// Queue extension with why-gem pick provenance: one recording pass, labeled by the
    /// caller's slot (autoplay = the streaming mode's wire name; the DJ Gem chat labels
    /// its own enqueues). Recording candidates that dedup out is harmless — provenance
    /// only lights the "why?" affordance on rows that actually exist.
    pub(super) async fn extend_queue_from_picks(
        &mut self,
        songs: Vec<Song>,
        slot: &str,
    ) -> Vec<EngineEffect> {
        self.record_why_gem_picks(slot, &songs);
        let added = self.queue.extend(songs);
        if added == 0 {
            self.note_streaming_failure("autoplay streaming found no new tracks".to_owned());
            return Vec::new();
        }
        self.consecutive_streaming_failures = 0;
        self.save_session();
        if self.loaded_video_id.is_none() && self.queue.remaining() > 0 {
            self.queue.next(false);
            if let Err(e) = self.load_current().await {
                self.last_error = Some(e.to_string());
                self.stop_playback();
            }
        }
        Vec::new()
    }

    /// The autoplay slot label for why-gem provenance: the streaming mode's wire name.
    fn streaming_mode_slot(&self) -> String {
        serde_json::to_value(self.config.streaming.mode)
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_else(|| "autoplay".to_owned())
    }

    pub(super) fn note_streaming_failure(&mut self, status: String) {
        self.last_error = Some(status);
        if self.streaming {
            self.consecutive_streaming_failures =
                self.consecutive_streaming_failures.saturating_add(1);
            if self.consecutive_streaming_failures >= AUTOPLAY_MAX_FAILURES {
                self.streaming = false;
                self.streaming_pending = false;
                self.config.autoplay_streaming = Some(false);
                self.save_config("daemon streaming circuit-breaker");
            }
        }
    }

    fn augment_streaming_candidates(
        &self,
        seed_video_id: &str,
        candidates: &mut Vec<(Song, CandidateSource)>,
    ) {
        let mode = self.config.streaming.mode;
        let profile = mode.profile(&self.config.streaming);
        let seed_artist = self.streaming_seed_artist_key(seed_video_id);
        let mut seen: HashSet<String> = candidates
            .iter()
            .map(|(song, _)| song.video_id.clone())
            .collect();
        seen.extend(
            self.queue
                .ordered_iter()
                .filter(|song| !song.is_radio_station())
                .map(|song| song.video_id.clone()),
        );
        seen.insert(seed_video_id.to_owned());

        let (liked_cap, history_cap) = match mode {
            StreamingMode::Focused => (14, 8),
            StreamingMode::Balanced => (10, 14),
            StreamingMode::Discovery => (6, 24),
        };

        let mut favorites: Vec<Song> = self
            .library
            .favorites
            .iter()
            .filter(|song| !song.is_radio_station())
            .cloned()
            .collect();
        favorites.sort_by(|a, b| {
            local_neighbor_score(b, &seed_artist, &self.signals).total_cmp(&local_neighbor_score(
                a,
                &seed_artist,
                &self.signals,
            ))
        });
        for song in favorites.into_iter().take(liked_cap) {
            if seen.insert(song.video_id.clone()) {
                candidates.push((song, CandidateSource::LikedNeighbor));
            }
        }

        let mut added_history = 0usize;
        for song in self
            .library
            .history
            .iter()
            .filter(|song| !song.is_radio_station())
            .skip(profile.history_block_horizon)
        {
            if seen.insert(song.video_id.clone()) {
                candidates.push((song.clone(), CandidateSource::HistoryCooc));
                added_history += 1;
                if added_history >= history_cap {
                    break;
                }
            }
        }
    }

    fn build_station_state(&self, seed_video_id: &str) -> StationState {
        let profile = self.config.streaming.mode.profile(&self.config.streaming);
        // Single-sourced with the App reducer so the two owners can't drift.
        let (recent_track_ids, recent_artist_keys) =
            streaming::station_recent_context(&self.queue, &self.library, &profile);

        let favorite_artist_keys: HashSet<String> = self
            .library
            .favorites
            .iter()
            .filter(|s| !s.is_radio_station())
            .map(|s| signals::normalize_artist(&s.artist))
            .collect();
        let skip_streak = self.streaming_skip_streak();
        let temporary_novelty_boost =
            if self.config.streaming.mode == StreamingMode::Focused && skip_streak >= 2 {
                0.12
            } else {
                0.0
            };
        let temporary_familiarity_boost =
            if self.config.streaming.mode == StreamingMode::Discovery && skip_streak >= 2 {
                0.20
            } else {
                0.0
            };

        StationState {
            mode: self.config.streaming.mode,
            seed_video_id: seed_video_id.to_owned(),
            seed_artist_key: self.streaming_seed_artist_key(seed_video_id),
            recent_track_ids,
            recent_artist_keys,
            banned_track_ids: HashSet::new(),
            banned_artist_keys: self.station.avoid_artist_keys().into_iter().collect(),
            favorite_artist_keys,
            session_artist_bias: self.session_artist_bias(),
            temporary_novelty_boost,
            temporary_familiarity_boost,
        }
    }

    fn streaming_seed_artist_key(&self, seed_video_id: &str) -> String {
        if let Some(cur) = self.queue.current()
            && cur.video_id == seed_video_id
            && !cur.is_radio_station()
        {
            return signals::normalize_artist(&cur.artist);
        }
        self.library
            .history
            .iter()
            .filter(|s| !s.is_radio_station())
            .find(|s| s.video_id == seed_video_id)
            .map(|s| signals::normalize_artist(&s.artist))
            .unwrap_or_default()
    }

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
            .take_while(|e| matches!(e.outcome, DaemonOutcome::Skip | DaemonOutcome::QuickSkip))
            .count()
    }

    pub(super) fn playback_completion(&self) -> f32 {
        match (self.playback.time_pos, self.playback.duration) {
            (Some(t), Some(d)) if d > 0.0 => (t / d).clamp(0.0, 1.0) as f32,
            _ => 0.5,
        }
    }
}

fn local_neighbor_score(song: &Song, seed_artist_key: &str, sig: &Signals) -> f32 {
    let artist_key = signals::normalize_artist(&song.artist);
    let seed_bonus = if artist_key == seed_artist_key {
        1.0
    } else {
        0.0
    };
    seed_bonus + sig.artist_weight(&artist_key)
}
