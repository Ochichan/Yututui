//! Daemon autoplay extension, local candidate planning, and session-bias policy.
//!
//! This remains an implementation block on [`DaemonEngine`]: ownership and orchestration stay
//! in the engine while the coherent streaming policy is kept out of the main command surface.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use crate::api::{ApiEvent, Song};
use crate::playback_policy::{
    AUTOPLAY_MAX_FAILURES, STREAMING_FALLBACK_COUNT, STREAMING_POOL_COUNT,
};
use crate::signals::{self, Signals};
use crate::streaming::{self, CandidateSource, Cooc, StationState, StreamingMode};

use super::{DaemonEngine, DaemonOutcome, EngineEffect};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StreamingRequestStage {
    Pool,
    Preflight,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PendingStreamingRequest {
    pub(super) request_id: u64,
    seed_video_id: String,
    mode: StreamingMode,
    source: crate::search_source::SearchSource,
    queue_rev: u64,
    owner_mode: crate::session::LastMode,
    pub(super) stage: StreamingRequestStage,
}

impl DaemonEngine {
    pub(super) async fn handle_streaming_api_event(
        &mut self,
        event: ApiEvent,
    ) -> Vec<EngineEffect> {
        match event {
            ApiEvent::StreamingResults {
                request_id,
                seed_video_id,
                candidates,
            } => {
                let Some(pending) = self.take_streaming_request(
                    request_id,
                    &seed_video_id,
                    StreamingRequestStage::Pool,
                ) else {
                    return Vec::new();
                };
                let picks = self.plan_local_streaming(&seed_video_id, candidates);
                self.extend_sanitized_streaming(pending, picks, &[]).await
            }
            ApiEvent::StreamingPreflighted {
                request_id,
                seed_video_id,
                songs,
            } => {
                let Some(pending) = self.take_streaming_request(
                    request_id,
                    &seed_video_id,
                    StreamingRequestStage::Preflight,
                ) else {
                    return Vec::new();
                };
                let slot = Self::streaming_mode_slot(pending.mode);
                self.extend_queue_from_picks(songs, slot).await
            }
            ApiEvent::StreamingError {
                request_id,
                seed_video_id,
                error,
            } => {
                if self
                    .take_streaming_request_for_error(request_id, &seed_video_id)
                    .is_some()
                {
                    self.note_streaming_failure(format!("autoplay streaming failed: {error}"));
                }
                Vec::new()
            }
            _ => unreachable!("non-streaming API event reached streaming reducer"),
        }
    }

    /// Effective autoplay state. `streaming` remains the user's saved normal-mode preference;
    /// Local Deck (and the existing dedicated Radio boundary) suppresses network top-ups without
    /// rewriting it.
    pub(super) fn streaming_active(&self) -> bool {
        self.streaming
            && !matches!(
                self.last_mode,
                crate::session::LastMode::Radio | crate::session::LastMode::Local
            )
            && !self.current_is_radio_stream()
    }

    pub(super) fn maybe_autoplay_extend(&mut self) -> Vec<EngineEffect> {
        self.autoplay_extend(false)
    }

    pub(super) fn force_autoplay_extend(&mut self) -> Vec<EngineEffect> {
        self.autoplay_extend(true)
    }

    fn autoplay_extend(&mut self, force: bool) -> Vec<EngineEffect> {
        self.reconcile_pending_streaming_request();
        let Some(refill) = streaming::plan_autoplay_refill(
            self.streaming_active(),
            self.streaming_pending,
            force,
            self.queue.remaining(),
            self.last_extend.map(|t| t.elapsed()),
            self.queue.current(),
        ) else {
            return Vec::new();
        };
        let exclude_ids = self.streaming_exclude_ids(&refill.seed_video_id);
        let config = self.config.effective_search();
        let request_id = self.begin_streaming_request(
            refill.seed_video_id.clone(),
            self.config.streaming.mode,
            config.streaming_source,
        );
        self.last_extend = Some(Instant::now());
        vec![EngineEffect::StreamingFallback {
            request_id,
            seed: refill.seed,
            seed_video_id: refill.seed_video_id,
            exclude_ids,
            limit: STREAMING_POOL_COUNT,
            mode: self.config.streaming.mode,
            config,
        }]
    }

    fn begin_streaming_request(
        &mut self,
        seed_video_id: String,
        mode: StreamingMode,
        source: crate::search_source::SearchSource,
    ) -> u64 {
        self.streaming_request_seq = self.streaming_request_seq.saturating_add(1);
        let request_id = self.streaming_request_seq;
        self.pending_streaming_request = Some(PendingStreamingRequest {
            request_id,
            seed_video_id,
            mode,
            source,
            queue_rev: self.queue.rev(),
            owner_mode: self.last_mode,
            stage: StreamingRequestStage::Pool,
        });
        self.streaming_pending = true;
        request_id
    }

    pub(in crate::daemon) fn cancel_pending_streaming_request(&mut self) {
        self.pending_streaming_request = None;
        self.streaming_pending = false;
    }

    fn pending_streaming_request_is_current(&self, pending: &PendingStreamingRequest) -> bool {
        self.streaming_active()
            && self.queue.rev() == pending.queue_rev
            && self.last_mode == pending.owner_mode
            && self.config.streaming.mode == pending.mode
            && self.config.effective_search().streaming_source == pending.source
            && self.queue.contains_video_id(&pending.seed_video_id)
    }

    /// Owner-turn reconciliation makes queue/mode/session mutations cancel an in-flight result
    /// even when the replacement queue happens to contain the same seed id.
    pub(in crate::daemon) fn reconcile_pending_streaming_request(&mut self) {
        let stale = self
            .pending_streaming_request
            .as_ref()
            .is_some_and(|pending| !self.pending_streaming_request_is_current(pending));
        if stale {
            self.cancel_pending_streaming_request();
        } else {
            self.streaming_pending = self.pending_streaming_request.is_some();
        }
    }

    pub(super) fn take_streaming_request(
        &mut self,
        request_id: u64,
        seed_video_id: &str,
        stage: StreamingRequestStage,
    ) -> Option<PendingStreamingRequest> {
        let pending = self.pending_streaming_request.as_ref()?;
        if pending.request_id != request_id
            || pending.seed_video_id != seed_video_id
            || pending.stage != stage
        {
            return None;
        }
        if !self.pending_streaming_request_is_current(pending) {
            self.cancel_pending_streaming_request();
            return None;
        }

        let pending = self
            .pending_streaming_request
            .take()
            .expect("streaming request was present");
        self.streaming_pending = false;
        Some(pending)
    }

    pub(super) fn take_streaming_request_for_error(
        &mut self,
        request_id: u64,
        seed_video_id: &str,
    ) -> Option<PendingStreamingRequest> {
        let pending = self.pending_streaming_request.as_ref()?;
        if pending.request_id != request_id || pending.seed_video_id != seed_video_id {
            return None;
        }
        if !self.pending_streaming_request_is_current(pending) {
            self.cancel_pending_streaming_request();
            return None;
        }

        let pending = self
            .pending_streaming_request
            .take()
            .expect("streaming request was present");
        self.streaming_pending = false;
        Some(pending)
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
        mut pending: PendingStreamingRequest,
        songs: Vec<Song>,
        fallback: &[Song],
    ) -> Vec<EngineEffect> {
        let sanitized =
            streaming::sanitize_final_picks(songs, fallback, pending.mode, &self.config.streaming);
        if !sanitized.is_empty()
            && streaming::final_preflight_needed(
                &sanitized,
                fallback,
                pending.mode,
                &self.config.streaming,
            )
        {
            pending.stage = StreamingRequestStage::Preflight;
            let request_id = pending.request_id;
            let seed_video_id = pending.seed_video_id.clone();
            let mode = pending.mode;
            self.pending_streaming_request = Some(pending);
            self.streaming_pending = true;
            return vec![EngineEffect::StreamingPreflight {
                request_id,
                seed_video_id,
                picks: sanitized,
                fallback: fallback.to_vec(),
                mode,
                config: self.config.streaming.clone(),
            }];
        }
        let slot = Self::streaming_mode_slot(pending.mode);
        self.extend_queue_from_picks(sanitized, slot).await
    }

    #[cfg(test)]
    pub(super) async fn extend_queue_from_streaming(
        &mut self,
        songs: Vec<Song>,
    ) -> Vec<EngineEffect> {
        let slot = Self::streaming_mode_slot(self.config.streaming.mode);
        self.extend_queue_from_picks(songs, slot).await
    }

    /// Queue extension with WhyGem provenance. Queue capacity decides the accepted prefix;
    /// rejected candidates never become fetchable explanations.
    pub(super) async fn extend_queue_from_picks(
        &mut self,
        songs: Vec<Song>,
        slot: &str,
    ) -> Vec<EngineEffect> {
        let video_ids: Vec<String> = songs.iter().map(|song| song.video_id.clone()).collect();
        let old_len = self.queue.len();
        let was_idle = self.loaded_video_id.is_none();
        let previous = was_idle.then(|| self.queue.snapshot());
        let added = self.queue.extend(songs);
        if added == 0 {
            self.note_streaming_failure("autoplay streaming found no new tracks".to_owned());
            return Vec::new();
        }

        // An idle queue must admit the first newly appended track to the player before any
        // recommendation provenance or success bookkeeping becomes observable. In particular,
        // an empty queue already selects its first appended row; calling `next` would either skip
        // that row or (for a single pick) fail to load anything at all.
        if was_idle {
            self.queue
                .goto(old_len.min(self.queue.len().saturating_sub(1)));
            if self
                .load_current_or_restore_queue(
                    previous.expect("idle streaming extension captured a queue snapshot"),
                )
                .await
                .is_err()
            {
                return Vec::new();
            }
        }

        self.record_why_gem_ids(slot, video_ids.into_iter().take(added));
        self.consecutive_streaming_failures = 0;
        if !was_idle {
            self.save_session();
        }
        Vec::new()
    }

    /// The local streaming origin shown by WhyGem. Gemini reason roles remain lower-case;
    /// this source label intentionally follows the user-facing mode names.
    pub(super) fn streaming_mode_slot(mode: StreamingMode) -> &'static str {
        match mode {
            StreamingMode::Focused => "Focused",
            StreamingMode::Balanced => "Balanced",
            StreamingMode::Discovery => "Discovery",
        }
    }

    pub(super) fn note_streaming_failure(&mut self, status: String) {
        self.last_error = Some(status);
        if self.streaming {
            self.consecutive_streaming_failures =
                self.consecutive_streaming_failures.saturating_add(1);
            if self.consecutive_streaming_failures >= AUTOPLAY_MAX_FAILURES {
                self.streaming = false;
                self.cancel_pending_streaming_request();
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
