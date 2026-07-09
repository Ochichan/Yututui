//! Autoplay/streaming reducer methods, split out of the monolithic `app.rs` (behaviour-preserving).

use super::*;

/// A streaming/autoplay pipeline message: a prefetched/resolved direct URL, related-track
/// candidates, the metadata-preflighted picks, a fallback failure, and the DJ Gem reranker's
/// chosen picks. Bucketed under [`Msg::Streaming`] to keep the flat `Msg` lean. Constructed in
/// `runtime.rs` from the leaf `ApiEvent`/`ResolverEvent`/`AiEvent`; never imported by a leaf
/// actor (see `scripts/check-architecture.sh`).
pub enum StreamingMsg {
    /// A track's direct stream URL was prefetched (for instant skip).
    Resolved {
        video_id: String,
        stream_url: String,
    },
    /// Related tracks returned by the non-DJ Gem streaming fallback, each tagged with the source it
    /// came from (real YTM watch-playlist vs anonymous yt-dlp search) so the local engine can
    /// weight provenance and prefer the better source on dedup.
    Results {
        seed_video_id: String,
        candidates: Vec<(Song, CandidateSource)>,
    },
    /// Final streaming picks after the API actor has run any needed metadata preflight. This is the
    /// last gate before enqueueing; it can drop risky public-YouTube candidates and top up from
    /// fallback picks.
    Preflighted {
        seed_video_id: String,
        songs: Vec<Song>,
    },
    /// The non-DJ Gem streaming fallback failed to fetch related tracks.
    Error {
        seed_video_id: String,
        error: String,
    },
    /// The DJ Gem reranker's chosen picks (best-first), or empty on any failure. Each pick is an
    /// opaque pack `cid`; the reducer resolves cids→tracks via the stashed `cid_map`, validates
    /// against the shortlist, and tops up from the local pick.
    AiPicks {
        seed_video_id: String,
        picks: Vec<AiPick>,
        /// The model's self-reported confidence in [0,1], if it returned one.
        conf: Option<f32>,
    },
}

impl From<StreamingMsg> for Msg {
    fn from(msg: StreamingMsg) -> Self {
        Msg::Streaming(msg)
    }
}

impl App {
    /// Handle the DJ Gem reranker's chosen streaming picks: resolve cids, validate
    /// against the shortlist, cache the ordering, and enqueue. Extracted verbatim from
    /// the `StreamingMsg::AiPicks` dispatch arm.
    pub(in crate::app) fn on_streaming_ai_picks(
        &mut self,
        seed_video_id: String,
        picks: Vec<AiPick>,
        conf: Option<f32>,
    ) -> Vec<Cmd> {
        self.ai.thinking = false;
        self.dirty = true;
        // Only consume `pending_rerank` when this result is for it (a stale/duplicate
        // message for some other seed leaves the current rerank untouched). When it does
        // match but the seed is no longer queued (the user skipped/cleared mid-think),
        // the chain still drops the stale rerank without enqueuing.
        let ours = self
            .streaming
            .pending_rerank
            .as_ref()
            .is_some_and(|p| p.seed_video_id == seed_video_id);
        if ours
            && let Some(pending) = self.streaming.pending_rerank.take()
            && self.autoplay_streaming
            && self.queue.contains_video_id(&seed_video_id)
        {
            if let Some(conf) = conf {
                tracing::debug!(
                    conf,
                    picks = picks.len(),
                    "streaming DJ Gem rerank confidence"
                );
            }
            // Resolve the model's opaque cids back to real tracks once, keeping its order. A
            // cid that isn't in the pack (a hallucinated id) is dropped here; `merge_ai_picks`
            // then re-validates against the shortlist and tops up from the local pick. The
            // same resolution feeds the "Why DJ Gem" overlay (title + role + reasons), which must
            // outlive the `pending_rerank` we're about to drop.
            let resolved: Vec<(String, ExplainPick)> = picks
                .iter()
                .filter_map(|p| {
                    let vid = pending
                        .cid_map
                        .iter()
                        .find(|m| m.cid == p.cid)?
                        .video_id
                        .clone();
                    let song = pending.shortlist.iter().find(|s| s.video_id == vid)?;
                    let pick = ExplainPick {
                        title: self.display_title(song).into_owned(),
                        artist: self.display_artist(song).into_owned(),
                        role: p.role.clone(),
                        reasons: p.reasons.clone(),
                    };
                    Some((vid, pick))
                })
                .collect();
            let ids: Vec<String> = resolved.iter().map(|(vid, _)| vid.clone()).collect();
            let roles: Vec<Option<String>> =
                resolved.iter().map(|(_, pick)| pick.role.clone()).collect();
            let recipe_ok =
                streaming::ai_roles_match_recipe(&roles, pending.mode, &self.config.streaming);
            let effective_conf = if recipe_ok {
                conf
            } else {
                Some(conf.unwrap_or(0.35).min(0.40))
            };
            if !resolved.is_empty() {
                self.streaming.last_explain = Some(StreamingAiExplain {
                    conf: effective_conf,
                    picks: resolved.into_iter().map(|(_, p)| p).collect(),
                });
            }
            let picks = streaming::merge_ai_picks_with_confidence(
                &ids,
                &pending.shortlist,
                &pending.local_pick,
                self.config.streaming.ai.picks,
                effective_conf,
            );
            // Cache the validated ordering so a rapid identical refill replays it without a
            // second call. Skip empty results (a failed rerank) so the next refill retries.
            if !ids.is_empty() && recipe_ok && effective_conf.unwrap_or(0.0) >= 0.45 {
                self.ai_cache_store(pending.cache_key, ids);
            }
            return self.extend_sanitized_streaming(&seed_video_id, picks, &pending.local_pick);
        }
        Vec::new()
    }

    /// If autoplay/streaming is on and the queue is running low, top it up. Both the DJ Gem and non-DJ Gem
    /// paths fetch the *same* local candidate pool first; the DJ Gem reranker (when a key is
    /// configured) then reorders it in [`StreamingMsg::Results`]. The DJ Gem never invents tracks.
    pub(in crate::app) fn maybe_autoplay_extend(&mut self) -> Vec<Cmd> {
        self.autoplay_extend(false)
    }

    pub(in crate::app) fn force_autoplay_extend(&mut self) -> Vec<Cmd> {
        self.autoplay_extend(true)
    }

    fn autoplay_extend(&mut self, force: bool) -> Vec<Cmd> {
        // `streaming_active()` (not the raw preference) so a top-up never fires in dedicated
        // Radio mode or while a live station plays — the station early-return below stays as a
        // defensive backstop.
        if !self.streaming_active() {
            return Vec::new();
        }
        if !force && self.queue.remaining() > AUTOPLAY_THRESHOLD {
            return Vec::new();
        }
        // One refill in flight at a time: the pool fetch (`streaming.pending`) or, when the DJ Gem
        // reranks the fetched pool, that rerank call (`ai_thinking`).
        if self.streaming.pending || (self.ai.available && self.ai.thinking) {
            return Vec::new();
        }
        let cooled = match self.streaming.last_extend {
            Some(t) => t.elapsed() >= AUTOPLAY_COOLDOWN,
            None => true,
        };
        if !force && !cooled {
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
        self.streaming.last_extend = Some(Instant::now());
        self.streaming.pending = true;
        self.status.text = t!(
            "Autoplay: finding related tracks",
            "자동재생: 관련 곡을 찾는 중"
        )
        .to_owned();
        self.dirty = true;
        vec![Cmd::StreamingFallback {
            seed,
            seed_video_id,
            exclude_ids,
            mode: self.config.streaming.mode,
            config: self.config.effective_search(),
        }]
    }

    /// Stage 2 of the DJ Gem streaming path: rank the fetched pool locally (the guaranteed `local_pick`
    /// fallback) and hand a diverse shortlist to the assistant to rerank by id. Stashes both in
    /// `pending_rerank` for [`StreamingMsg::AiPicks`] to validate against, and emits the rerank
    /// command. If the pool yields no rerankable shortlist, it enqueues the local pick directly.
    pub(in crate::app) fn start_ai_rerank(
        &mut self,
        seed_video_id: &str,
        mut candidates: Vec<(Song, CandidateSource)>,
    ) -> Vec<Cmd> {
        let st = self.build_station_state(seed_video_id);
        self.ensure_cooc_cache();
        let cooc = &self
            .streaming
            .cooc_cache
            .as_ref()
            .expect("cooc cache populated")
            .1;
        self.augment_streaming_candidates(seed_video_id, &mut candidates);
        let pool = streaming::pool_from_tagged(candidates);
        self.log_streaming_gate(&st, &pool);
        let now = signals::unix_now();
        let local_pick = streaming::plan_local(
            pool.clone(),
            &st,
            &self.signals,
            cooc,
            &self.config.streaming,
            self.config.streaming.ai.picks,
            now,
        );
        let shortlist = streaming::shortlist_for_ai(
            pool,
            &st,
            &self.signals,
            cooc,
            &self.config.streaming,
            self.config.streaming.ai.shortlist,
            now,
        );
        if shortlist.is_empty() {
            // Nothing to rerank → fall straight to the local pick (itself possibly empty, which
            // trips the circuit breaker via `extend_queue_from_streaming`).
            return self.extend_sanitized_streaming(seed_video_id, local_pick, &[]);
        }
        // Smart gate: skip the DJ Gem call when the local pick is already confident and the listener
        // isn't skipping — saves the spend + latency. `smart_gate=false` restores always-on DJ Gem.
        let skip_streak = self.streaming_skip_streak();
        if !streaming::should_call_ai(&shortlist, skip_streak, &self.config.streaming) {
            tracing::debug!(skip_streak, "streaming DJ Gem gated → confident local pick");
            return self.extend_sanitized_streaming(seed_video_id, local_pick, &[]);
        }
        // Result cache: a rapid identical refill (same seed artist, mode, recent ids, and candidate
        // set) replays the last resolved ordering instead of spending another call. Keyed on the
        // query, valued by the DJ Gem's chosen video ids (re-validated through `merge_ai_picks`).
        let cand_ids: Vec<String> = shortlist.iter().map(|c| c.video_id().to_owned()).collect();
        let recovery_line = self.streaming_recovery_line();
        let station_query = self
            .station
            .active
            .as_ref()
            .map(|p| p.query.as_str())
            .unwrap_or("");
        let avoid_artist_keys = self.station.avoid_artist_keys();
        let profile = st.mode.profile(&self.config.streaming);
        let recipe_hash = streaming::ai_recipe_hash(profile.ai_recipe);
        let cache_key = streaming::ai_cache_key(streaming::AiCacheKeyParts {
            seed_artist: &st.seed_artist_key,
            mode: st.mode,
            recent_ids: &st.recent_track_ids,
            candidate_ids: &cand_ids,
            station_query,
            avoid_artist_keys: &avoid_artist_keys,
            recovery_line: recovery_line.as_deref(),
            skip_streak,
            profile_version: profile.profile_version,
            prompt_recipe_hash: recipe_hash,
        });
        if let Some(cached_ids) = self.ai_cache_lookup(cache_key) {
            tracing::debug!("streaming DJ Gem cache hit → replaying cached order");
            let shortlist_songs: Vec<Song> = shortlist.iter().map(|c| c.song.clone()).collect();
            let merged = streaming::merge_ai_picks(
                &cached_ids,
                &shortlist_songs,
                &local_pick,
                self.config.streaming.ai.picks,
            );
            return self.extend_sanitized_streaming(seed_video_id, merged, &local_pick);
        }
        let (prompt, cid_map) =
            self.ai_rerank_prompt(seed_video_id, &shortlist, st.mode, recovery_line.as_deref());
        let shortlist_songs: Vec<Song> = shortlist.into_iter().map(|c| c.song).collect();
        self.streaming.pending_rerank = Some(PendingRerank {
            seed_video_id: seed_video_id.to_owned(),
            mode: st.mode,
            shortlist: shortlist_songs,
            local_pick,
            cid_map,
            cache_key,
        });
        self.ai.thinking = true;
        self.status.text = t!(
            "Autoplay: DJ Gem reranking",
            "자동재생: DJ Gem이 순위를 매기는 중"
        )
        .to_owned();
        self.dirty = true;
        vec![Cmd::AiRerank {
            seed_video_id: seed_video_id.to_owned(),
            prompt,
        }]
    }

    /// A cached DJ Gem rerank ordering for `key`, if one is stored and still within [`AI_CACHE_TTL`].
    fn ai_cache_lookup(&self, key: u64) -> Option<Vec<String>> {
        self.streaming
            .ai_cache
            .get(&key)
            .filter(|(_, at)| at.elapsed() < AI_CACHE_TTL)
            .map(|(ids, _)| ids.clone())
    }

    /// Store a resolved DJ Gem rerank ordering, pruning expired entries first so the map stays tiny.
    pub(in crate::app) fn ai_cache_store(&mut self, key: u64, ids: Vec<String>) {
        self.streaming
            .ai_cache
            .retain(|_, (_, at)| at.elapsed() < AI_CACHE_TTL);
        self.streaming.ai_cache.insert(key, (ids, Instant::now()));
    }

    /// The compact, evidence-rich rerank prompt plus the `cid → video_id` map for resolving the
    /// model's choices. The candidate pack ([`streaming::pack::build_cands_block`]) emits each track
    /// as one terse line of 0-100 feature scores under an opaque, shuffled `cid` — so the model
    /// reranks on evidence, not on title or position. It can only pick cids from this pack, so it
    /// never sees a track that isn't already a playable local candidate.
    pub(in crate::app) fn ai_rerank_prompt(
        &self,
        seed_video_id: &str,
        shortlist: &[streaming::Candidate],
        mode: streaming::StreamingMode,
        recovery_line: Option<&str>,
    ) -> (String, Vec<streaming::PackedCand>) {
        let picks = self.config.streaming.ai.picks;
        let profile = mode.profile(&self.config.streaming);
        let recipe = profile.ai_recipe;
        let (cands_block, cid_map) = streaming::pack::build_cands_block(shortlist, seed_video_id);

        let mut s = String::new();
        s.push_str(&format!("TASK|streaming_next|n={picks}|mode={mode:?}\n"));
        s.push_str(&format!(
            "RECIPE|familiar_min={}|bridge_min={}|discovery_min={}|familiar_max={}|discovery_max={}|max_same_artist={}\n",
            recipe.familiar_min,
            recipe.bridge_min,
            recipe.discovery_min,
            recipe.familiar_max,
            recipe.discovery_max,
            recipe.max_same_artist,
        ));
        s.push_str(match mode {
            streaming::StreamingMode::Focused => {
                "POLICY|prefer_canonical=1|old_liked_ok=1|avoid_live_remix_cover=1|avoid_sped_up=1\n"
            }
            streaming::StreamingMode::Balanced => {
                "POLICY|prefer_canonical=1|allow_bridge=1|avoid_sped_up=1\n"
            }
            streaming::StreamingMode::Discovery => {
                "POLICY|prefer_new_artists=1|allow_live_acoustic=1|allow_deep_cuts=1|avoid_sped_up=1\n"
            }
        });
        s.push_str("RULE|candidate_only=1|json_only=1|ignore_candidate_position=1\n");
        s.push_str("RECENT (most recent last):\n");
        let labels = self
            .queue
            .current()
            .filter(|c| c.video_id == seed_video_id)
            .map(|c| self.streaming_context_labels(c))
            .unwrap_or_default();
        for (role, label) in labels.iter().rev() {
            s.push_str(&format!("- {role}: {label}\n"));
        }
        if let Some(recovery) = recovery_line {
            s.push_str(recovery);
            s.push('\n');
        }
        s.push_str(
            "NOTE|candidates_unordered=1|use_scores_not_order=1|cids_from_CANDS_only=1\n\
             Scores are 0-100: co=co-occurrence with recent, tr=transition from current, \
             u=your affinity, nov=novelty, cont=source continuation, comp=completion rate, \
             m=official-music tier.\n",
        );
        s.push_str(&cands_block);
        s.push_str(&format!(
            "\nReturn JSON {{\"ids\":[cid,...],\"roles\":[role,...],\"reasons\":[[code,...],...],\
             \"conf\":0.0}} choosing up to {picks} cids best-first.",
        ));
        (s, cid_map)
    }

    /// A compact `RECOVERY|…` line summarizing how the recent session has been going, or `None`
    /// when there's nothing actionable. Keys off the *artist* (the engine already excludes the
    /// just-played track) and the trailing skip streak, so the model can react to the arc: widen
    /// after a skip, stay close after a like, recover after a streak.
    pub(in crate::app) fn streaming_recovery_line(&self) -> Option<String> {
        let events = &self.streaming.session_events;
        let last_skip = events
            .iter()
            .rev()
            .find(|e| matches!(e.outcome, Outcome::Skip | Outcome::QuickSkip));
        let last_like = events.iter().rev().find(|e| e.outcome == Outcome::Like);
        let last_dislike = events.iter().rev().find(|e| e.outcome == Outcome::Dislike);
        let streak = self.streaming_skip_streak();

        let mut parts: Vec<String> = Vec::new();
        if let Some(s) = last_skip {
            let quick = if s.outcome == Outcome::QuickSkip {
                ",quick"
            } else {
                ""
            };
            parts.push(format!(
                "last_skip={}({}%{quick})",
                s.artist_key,
                (s.completion * 100.0).round() as i32
            ));
        }
        if let Some(l) = last_like {
            parts.push(format!("last_like={}", l.artist_key));
        }
        if let Some(d) = last_dislike {
            parts.push(format!("disliked={}", d.artist_key));
        }
        if streak >= 2 {
            parts.push(format!("skip_streak={streak}"));
        }
        (!parts.is_empty()).then(|| format!("RECOVERY|{}", parts.join("|")))
    }

    /// How many of the most recent session outcomes were skips, counting back from the newest
    /// until the first non-skip. `0` means the last track played through (the user is content).
    pub(in crate::app) fn streaming_skip_streak(&self) -> usize {
        self.streaming
            .session_events
            .iter()
            .rev()
            .take_while(|e| matches!(e.outcome, Outcome::Skip | Outcome::QuickSkip))
            .count()
    }

    /// Off the hot path: when the listener has clearly been rejecting the active station's
    /// direction (a trailing skip streak), hand the recent session log to the assistant to distill
    /// into an artist avoid/boost patch ([`Cmd::SummarizeFeedback`] → [`AiMsg::StationPatch`]).
    /// Returns `None` (a no-op) unless every gate passes: there's an active station to refine, the
    /// DJ Gem is configured, no summary is already in flight, the skip streak has reached
    /// [`FEEDBACK_STREAK`], the cooldown has elapsed, and the digest is non-empty. Sets the
    /// in-flight + cooldown guards so it fires at most once per streak/window.
    pub(in crate::app) fn maybe_summarize_feedback(&mut self) -> Option<Cmd> {
        if self.station.active.is_none() || !self.ai.available || self.streaming.feedback_in_flight
        {
            return None;
        }
        if self.streaming_skip_streak() < FEEDBACK_STREAK {
            return None;
        }
        if let Some(at) = self.streaming.last_feedback_at
            && at.elapsed() < FEEDBACK_COOLDOWN
        {
            return None;
        }
        let digest = self.feedback_digest()?;
        self.streaming.feedback_in_flight = true;
        self.streaming.last_feedback_at = Some(Instant::now());
        tracing::debug!("streaming feedback summary dispatched");
        Some(Cmd::SummarizeFeedback { digest })
    }

    /// Render the active station and its recent session outcomes into the compact digest the
    /// feedback summarizer reads. `None` when there's no station or no events to summarize. The
    /// `STATION`/`ALREADY_AVOIDING` lines anchor the model to *this* station; the `SESSION` lines
    /// are the recent arc (oldest first), each naming the artist key and what happened.
    fn feedback_digest(&self) -> Option<String> {
        let profile = self.station.active.as_ref()?;
        if self.streaming.session_events.is_empty() {
            return None;
        }
        let mut s = String::new();
        s.push_str(&format!(
            "STATION|vibe={}|explore={:?}\n",
            profile.query, profile.explore
        ));
        if !profile.avoid_artist_keys.is_empty() {
            s.push_str(&format!(
                "ALREADY_AVOIDING|{}\n",
                profile.avoid_artist_keys.join(",")
            ));
        }
        s.push_str("SESSION (oldest first):\n");
        for e in &self.streaming.session_events {
            let outcome = match e.outcome {
                Outcome::FullPlay => "played",
                Outcome::Skip => "skipped",
                Outcome::QuickSkip => "skipped_fast",
                Outcome::Like => "liked",
                Outcome::Dislike => "disliked",
            };
            s.push_str(&format!("- {}: {}\n", e.artist_key, outcome));
        }
        Some(s)
    }

    pub(in crate::app) fn streaming_context_labels(
        &self,
        current: &Song,
    ) -> Vec<(&'static str, String)> {
        let mut seen = HashSet::new();
        seen.insert(current.video_id.clone());
        let mut labels = vec![("Current", song_label(current))];

        for song in self
            .library
            .history
            .iter()
            .filter(|song| !song.is_radio_station())
        {
            if seen.insert(song.video_id.clone()) {
                let role = match labels.len() {
                    1 => "Previous 1",
                    2 => "Previous 2",
                    _ => break,
                };
                labels.push((role, song_label(song)));
            }
            if labels.len() >= 3 {
                break;
            }
        }

        labels
    }

    pub(in crate::app) fn extend_queue_from_streaming(&mut self, songs: Vec<Song>) -> Vec<Cmd> {
        let queued_songs = songs.clone();
        let added = self.queue.extend(songs);
        if added == 0 {
            return self.note_streaming_failure(
                t!(
                    "Autoplay found no new tracks",
                    "자동재생이 새 곡을 찾지 못했어요"
                )
                .to_owned(),
            );
        }
        self.streaming.consecutive_failures = 0;
        self.status.text = if crate::i18n::is_korean() {
            format!("{added}곡을 대기열에 추가함")
        } else {
            format!("Queued {added} track(s)")
        };
        // A successful top-up is a positive confirmation, not an error — render it green.
        self.status.kind = StatusKind::Info;
        self.dirty = true;
        // If the seed track ended before this refill landed (e.g. a 1-song queue with streaming
        // on), the player is idle — pick up the freshly queued track so playback resumes
        // instead of staying stopped at the finished song.
        if self.prefetch.loaded_video_id.is_none() && self.queue.remaining() > 0 {
            let mut cmds = self.advance(true);
            cmds.extend(self.request_romanization_for_songs(&queued_songs));
            return cmds;
        }
        // Still playing: pre-resolve the now-known next track's stream so the EOF→next hop is
        // instant (mirrors load_song's peek-next prefetch).
        let mut cmds = self.request_romanization_for_songs(&queued_songs);
        if self.prefetch.enabled()
            && let Some(next) = self.queue.peek_next()
            && let Some(watch_url) = next.prefetch_target()
        {
            let video_id = next.video_id.clone();
            if !self.prefetch.resolved.contains_fresh(&video_id) {
                cmds.push(Cmd::Resolve {
                    video_id,
                    watch_url,
                });
            }
        }
        cmds
    }

    /// Set autoplay-streaming, resetting the failure circuit-breaker whenever it is (re)enabled
    /// so a stale count left over from a previous auto-disable can't immediately trip it off
    /// again. The single home all three enable sites (key toggle, remote, DJ-Gem) route through,
    /// mirroring the daemon's `set_streaming`.
    pub(in crate::app) fn set_autoplay_streaming(&mut self, on: bool) {
        self.autoplay_streaming = on;
        if on {
            self.streaming.consecutive_failures = 0;
        }
    }

    pub(in crate::app) fn note_streaming_failure(&mut self, status: String) -> Vec<Cmd> {
        let mut disabled = false;
        if self.autoplay_streaming {
            self.streaming.consecutive_failures =
                self.streaming.consecutive_failures.saturating_add(1);
            if self.streaming.consecutive_failures >= AUTOPLAY_MAX_FAILURES {
                self.autoplay_streaming = false;
                disabled = true;
                self.streaming.pending = false;
                self.status.text = t!(
                    "Autoplay stopped (no related tracks found)",
                    "자동재생을 멈췄어요 (관련 곡을 찾지 못함)"
                )
                .to_owned();
            } else {
                self.status.text = status;
            }
        }
        self.dirty = true;
        if disabled {
            vec![self.save_playback_modes_cmd()]
        } else {
            Vec::new()
        }
    }

    pub(crate) fn streaming_exclude_ids(&self, seed_video_id: &str) -> Vec<String> {
        // Shared with the headless daemon engine — one implementation, so the two owners
        // can never drift on which already-heard/queued tracks a top-up excludes.
        crate::streaming::exclude_ids(
            &self.config.streaming,
            &self.queue,
            &self.library,
            seed_video_id,
        )
    }

    /// Rank a raw candidate pool (from the anonymous related-tracks search) through the local
    /// streaming engine, returning the final picks. The engine applies hard filters (already-heard,
    /// disliked, banned, bad-duration), a normalized additive base score, MMR diversity,
    /// artist/album cooldown, and softmax sampling — a dramatic upgrade over the old
    /// dedup-and-take. The deduped exclusion set is folded in via [`Self::build_station_state`].
    pub(in crate::app) fn plan_local_streaming(
        &mut self,
        seed_video_id: &str,
        mut candidates: Vec<(Song, CandidateSource)>,
    ) -> Vec<Song> {
        let st = self.build_station_state(seed_video_id);
        self.ensure_cooc_cache();
        let cooc = &self
            .streaming
            .cooc_cache
            .as_ref()
            .expect("cooc cache populated")
            .1;
        self.augment_streaming_candidates(seed_video_id, &mut candidates);
        let pool = streaming::pool_from_tagged(candidates);
        self.log_streaming_gate(&st, &pool);
        streaming::plan_local(
            pool,
            &st,
            &self.signals,
            cooc,
            &self.config.streaming,
            STREAMING_FALLBACK_COUNT,
            signals::unix_now(),
        )
    }

    pub(in crate::app) fn extend_sanitized_streaming(
        &mut self,
        seed_video_id: &str,
        songs: Vec<Song>,
        fallback: &[Song],
    ) -> Vec<Cmd> {
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
            self.streaming.pending = true;
            self.status.kind = StatusKind::Info;
            self.status.text =
                t!("Autoplay: checking tracks", "자동재생: 곡을 확인하는 중").to_owned();
            self.dirty = true;
            return vec![Cmd::StreamingPreflight {
                seed_video_id: seed_video_id.to_owned(),
                picks: sanitized,
                fallback: fallback.to_vec(),
                mode: self.config.streaming.mode,
                config: self.config.streaming.clone(),
            }];
        }
        self.extend_queue_from_streaming(sanitized)
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
            if added_history >= history_cap {
                break;
            }
            if seen.insert(song.video_id.clone()) {
                candidates.push((song.clone(), CandidateSource::HistoryCooc));
                added_history += 1;
            }
        }
    }

    fn ensure_cooc_cache(&mut self) {
        let generation = self.signals.play_log_generation();
        let fresh = self
            .streaming
            .cooc_cache
            .as_ref()
            .is_some_and(|(cached_generation, _)| *cached_generation == generation);
        if !fresh {
            self.streaming.cooc_cache = Some((
                generation,
                Cooc::build(self.signals.play_log(), &self.config.streaming.cooc),
            ));
        }
    }

    /// Emit a one-line `tracing` summary (plus per-drop `debug` lines) explaining what the
    /// MusicGate did to the freshly-fetched streaming pool — the low-friction "why did streaming
    /// pick these?" view. Lands in `yututui.log` at the default `info` level; per-candidate
    /// detail needs `RUST_LOG=debug`. Purely observational — it never changes what is enqueued.
    pub(in crate::app) fn log_streaming_gate(
        &self,
        st: &StationState,
        pool: &[streaming::Candidate],
    ) {
        if !self.config.streaming.gate.enabled || pool.is_empty() {
            return;
        }
        if !tracing::enabled!(tracing::Level::INFO) && !tracing::enabled!(tracing::Level::DEBUG) {
            return;
        }
        let verdicts: Vec<streaming::GateVerdict> =
            streaming::classify_pool(pool, st, &self.signals, &self.config.streaming);
        let kept = verdicts.iter().filter(|v| v.kept).count();
        let dropped = verdicts.len() - kept;
        if dropped == 0 {
            tracing::info!(
                pool = verdicts.len(),
                kept,
                "streaming gate: every candidate passed"
            );
            return;
        }
        let mut reasons: std::collections::BTreeMap<&str, u32> = std::collections::BTreeMap::new();
        for v in verdicts.iter().filter(|v| !v.kept) {
            *reasons.entry(v.reason).or_default() += 1;
        }
        let summary = reasons
            .iter()
            .map(|(r, n)| format!("{r}×{n}"))
            .collect::<Vec<_>>()
            .join(", ");
        tracing::info!(pool = verdicts.len(), kept, dropped, %summary, "streaming gate filtered the pool");
        if tracing::enabled!(tracing::Level::DEBUG) {
            for v in verdicts.iter().filter(|v| !v.kept) {
                tracing::debug!(
                    reason = v.reason,
                    source = ?v.source,
                    id = %v.video_id,
                    title = %v.title,
                    "streaming gate drop"
                );
            }
        }
    }

    /// Snapshot the current playback context into a [`StationState`] the pure engine ranks
    /// against: the seed, recently-heard tracks/artists (already-played filtering + cooldown),
    /// and favorite artists (a seed-affinity boost). Dislikes are read straight from `signals`.
    pub(in crate::app) fn build_station_state(&self, seed_video_id: &str) -> StationState {
        let profile = self.config.streaming.mode.profile(&self.config.streaming);
        // Single-sourced with the daemon engine so the two owners can't drift.
        let (recent_track_ids, recent_artist_keys) =
            crate::streaming::station_recent_context(&self.queue, &self.library, &profile);

        let favorite_artist_keys: HashSet<String> = self
            .library
            .favorites
            .iter()
            .filter(|s| !s.is_radio_station())
            .map(|s| signals::normalize_artist(&s.artist))
            .collect();
        let session_artist_bias = self.session_artist_bias();
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
            // The active natural-language station's avoided artists are kept out of every refill.
            banned_artist_keys: self.station.avoid_artist_keys().into_iter().collect(),
            favorite_artist_keys,
            session_artist_bias,
            temporary_novelty_boost,
            temporary_familiarity_boost,
        }
    }

    fn session_artist_bias(&self) -> HashMap<String, f32> {
        let mut out: HashMap<String, f32> = HashMap::new();
        for event in self.streaming.session_events.iter().rev().take(8) {
            let delta = match event.outcome {
                Outcome::FullPlay => 0.05,
                Outcome::Like => 0.15,
                Outcome::Skip => -0.10,
                Outcome::QuickSkip => -0.20,
                Outcome::Dislike => -0.40,
            };
            let entry = out.entry(event.artist_key.clone()).or_insert(0.0);
            *entry = (*entry + delta).clamp(-0.50, 0.35);
        }
        out
    }

    /// Sync the active station profile's adventurousness onto the live engine mode. The avoid
    /// list is read live in [`App::build_station_state`], so only the mode needs applying here
    /// (called at startup after the persisted profile loads).
    pub fn apply_station_profile(&mut self) {
        if let Some(profile) = &self.station.active {
            self.config.streaming.mode = profile.explore.to_mode();
        }
    }

    /// The normalized artist key of the streaming seed (usually the current track), for the
    /// seed-artist affinity boost. Falls back to a history lookup, then empty.
    pub(in crate::app) fn streaming_seed_artist_key(&self, seed_video_id: &str) -> String {
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
