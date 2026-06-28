//! Autoplay/radio reducer methods, split out of the monolithic `app.rs` (behaviour-preserving).

use super::*;

impl App {
    /// If autoplay/radio is on and the queue is running low, top it up. Both the AI and non-AI
    /// paths fetch the *same* local candidate pool first; the AI reranker (when a key is
    /// configured) then reorders it in [`Msg::RadioResults`]. The AI never invents tracks.
    pub(in crate::app) fn maybe_autoplay_extend(&mut self) -> Vec<Cmd> {
        if !self.autoplay_radio {
            return Vec::new();
        }
        if self.queue.remaining() > AUTOPLAY_THRESHOLD {
            return Vec::new();
        }
        // One refill in flight at a time: the pool fetch (`radio_pending`) or, when the AI
        // reranks the fetched pool, that rerank call (`ai_thinking`).
        if self.radio.pending || (self.ai.available && self.ai.thinking) {
            return Vec::new();
        }
        let cooled = match self.radio.last_extend {
            Some(t) => t.elapsed() >= AUTOPLAY_COOLDOWN,
            None => true,
        };
        if !cooled {
            return Vec::new();
        }
        let Some(cur) = self.queue.current() else {
            return Vec::new();
        };
        let seed = format!("{} — {}", cur.title, cur.artist);
        let seed_video_id = cur.video_id.clone();
        let exclude_ids = self.radio_exclude_ids(&seed_video_id);
        self.radio.last_extend = Some(Instant::now());
        self.radio.pending = true;
        self.status = t!("Autoplay radio: finding related tracks", "자동재생 라디오: 관련 곡을 찾는 중").to_owned();
        self.dirty = true;
        vec![Cmd::RadioFallback {
            seed,
            seed_video_id,
            exclude_ids,
        }]
    }

    /// Stage 2 of the AI radio path: rank the fetched pool locally (the guaranteed `local_pick`
    /// fallback) and hand a diverse shortlist to the assistant to rerank by id. Stashes both in
    /// `pending_rerank` for [`Msg::RadioAiPicks`] to validate against, and emits the rerank
    /// command. If the pool yields no rerankable shortlist, it enqueues the local pick directly.
    pub(in crate::app) fn start_ai_rerank(
        &mut self,
        seed_video_id: &str,
        candidates: Vec<(Song, CandidateSource)>,
    ) -> Vec<Cmd> {
        let st = self.build_station_state(seed_video_id);
        let cooc = Cooc::build(self.signals.play_log(), &self.config.radio.cooc);
        let pool = radio::pool_from_tagged(candidates);
        self.log_radio_gate(&st, &pool);
        let now = signals::unix_now();
        let local_pick = radio::plan_local(
            pool.clone(),
            &st,
            &self.signals,
            &cooc,
            &self.config.radio,
            self.config.radio.ai.picks,
            now,
        );
        let shortlist = radio::shortlist_for_ai(
            pool,
            &st,
            &self.signals,
            &cooc,
            &self.config.radio,
            self.config.radio.ai.shortlist,
            now,
        );
        if shortlist.is_empty() {
            // Nothing to rerank → fall straight to the local pick (itself possibly empty, which
            // trips the circuit breaker via `extend_queue_from_radio`).
            return self.extend_queue_from_radio(local_pick);
        }
        let prompt = self.ai_rerank_prompt(seed_video_id, &shortlist);
        let shortlist_songs: Vec<Song> = shortlist.into_iter().map(|c| c.song).collect();
        self.radio.pending_rerank = Some(PendingRerank {
            seed_video_id: seed_video_id.to_owned(),
            shortlist: shortlist_songs,
            local_pick,
        });
        self.ai.thinking = true;
        self.status = t!("Autoplay radio: AI reranking", "자동재생 라디오: AI가 순위를 매기는 중").to_owned();
        self.dirty = true;
        vec![Cmd::AiRerank {
            seed_video_id: seed_video_id.to_owned(),
            prompt,
        }]
    }

    /// Compact, ids-only rerank prompt: the recent session context plus the candidate shortlist
    /// (id + metadata). The model picks ids from this list only — it never sees a track that
    /// isn't already a playable local candidate.
    pub(in crate::app) fn ai_rerank_prompt(&self, seed_video_id: &str, shortlist: &[radio::Candidate]) -> String {
        let mut s = String::from("Session so far (most recent last):\n");
        let labels = self
            .queue
            .current()
            .filter(|c| c.video_id == seed_video_id)
            .map(|c| self.radio_context_labels(c))
            .unwrap_or_default();
        for (role, label) in labels.iter().rev() {
            s.push_str(&format!("- {role}: {label}\n"));
        }
        s.push_str("\nCandidates (id | title | artist | source):\n");
        for c in shortlist {
            s.push_str(&format!(
                "{} | {} | {} | {:?}\n",
                c.video_id(),
                c.song.title,
                c.song.artist,
                c.source
            ));
        }
        s.push_str(&format!(
            "\nReturn JSON {{\"ids\":[...]}} with up to {} candidate ids in the best listening order.",
            self.config.radio.ai.picks
        ));
        s
    }

    pub(in crate::app) fn radio_context_labels(&self, current: &Song) -> Vec<(&'static str, String)> {
        let mut seen = HashSet::new();
        seen.insert(current.video_id.clone());
        let mut labels = vec![("Current", song_label(current))];

        for song in &self.library.history {
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

    pub(in crate::app) fn extend_queue_from_radio(&mut self, songs: Vec<Song>) -> Vec<Cmd> {
        let added = self.queue.extend(songs);
        if added == 0 {
            self.note_radio_failure(
                t!("Autoplay radio found no new tracks", "자동재생 라디오가 새 곡을 찾지 못했어요").to_owned(),
            );
            return Vec::new();
        }
        self.radio.consecutive_failures = 0;
        self.status = if crate::i18n::is_korean() {
            format!("{added}곡을 대기열에 추가함")
        } else {
            format!("Queued {added} track(s)")
        };
        // A successful top-up is a positive confirmation, not an error — render it green.
        self.status_kind = StatusKind::Info;
        self.dirty = true;
        // If the seed track ended before this refill landed (e.g. a 1-song queue with radio
        // on), the player is idle — pick up the freshly queued track so playback resumes
        // instead of staying stopped at the finished song.
        if self.prefetch.loaded_video_id.is_none() && self.queue.remaining() > 0 {
            return self.advance(true);
        }
        // Still playing: pre-resolve the now-known next track's stream so the EOF→next hop is
        // instant (mirrors load_song's peek-next prefetch).
        let mut cmds = Vec::new();
        if let Some(next) = self.queue.peek_next()
            && !next.is_local()
        {
            let video_id = next.video_id.clone();
            let watch_url = next.watch_url();
            if !self.prefetch.resolved.contains_key(&video_id) {
                cmds.push(Cmd::Resolve { video_id, watch_url });
            }
        }
        cmds
    }

    pub(in crate::app) fn note_radio_failure(&mut self, status: String) {
        if self.autoplay_radio {
            self.radio.consecutive_failures = self.radio.consecutive_failures.saturating_add(1);
            if self.radio.consecutive_failures >= AUTOPLAY_MAX_FAILURES {
                self.autoplay_radio = false;
                self.radio.pending = false;
                self.status = t!(
                    "Autoplay radio stopped (no related tracks found)",
                    "자동재생 라디오를 멈췄어요 (관련 곡을 찾지 못함)"
                )
                .to_owned();
            } else {
                self.status = status;
            }
        }
        self.dirty = true;
    }

    pub(in crate::app) fn radio_exclude_ids(&self, seed_video_id: &str) -> Vec<String> {
        let mut ids: HashSet<String> = self.queue.video_ids().map(str::to_owned).collect();
        ids.insert(seed_video_id.to_owned());
        ids.extend(self.library.history.iter().map(|s| s.video_id.clone()));
        ids.into_iter().collect()
    }

    /// Rank a raw candidate pool (from the anonymous related-tracks search) through the local
    /// radio engine, returning the final picks. The engine applies hard filters (already-heard,
    /// disliked, banned, bad-duration), a normalized additive base score, MMR diversity,
    /// artist/album cooldown, and softmax sampling — a dramatic upgrade over the old
    /// dedup-and-take. The deduped exclusion set is folded in via [`Self::build_station_state`].
    pub(in crate::app) fn plan_local_radio(
        &self,
        seed_video_id: &str,
        candidates: Vec<(Song, CandidateSource)>,
    ) -> Vec<Song> {
        let st = self.build_station_state(seed_video_id);
        let cooc = Cooc::build(self.signals.play_log(), &self.config.radio.cooc);
        let pool = radio::pool_from_tagged(candidates);
        self.log_radio_gate(&st, &pool);
        radio::plan_local(
            pool,
            &st,
            &self.signals,
            &cooc,
            &self.config.radio,
            RADIO_FALLBACK_COUNT,
            signals::unix_now(),
        )
    }

    /// Emit a one-line `tracing` summary (plus per-drop `debug` lines) explaining what the
    /// MusicGate did to the freshly-fetched radio pool — the low-friction "why did the radio
    /// pick these?" view. Lands in `ytm-tui.log` at the default `info` level; per-candidate
    /// detail needs `RUST_LOG=debug`. Purely observational — it never changes what is enqueued.
    pub(in crate::app) fn log_radio_gate(&self, st: &StationState, pool: &[radio::Candidate]) {
        if !self.config.radio.gate.enabled || pool.is_empty() {
            return;
        }
        let verdicts: Vec<radio::GateVerdict> =
            radio::classify_pool(pool, st, &self.signals, &self.config.radio);
        let kept = verdicts.iter().filter(|v| v.kept).count();
        let dropped = verdicts.len() - kept;
        if dropped == 0 {
            tracing::info!(pool = verdicts.len(), kept, "radio gate: every candidate passed");
            return;
        }
        let mut reasons: std::collections::BTreeMap<&str, u32> = std::collections::BTreeMap::new();
        for v in verdicts.iter().filter(|v| !v.kept) {
            *reasons.entry(v.reason).or_default() += 1;
        }
        let summary = reasons.iter().map(|(r, n)| format!("{r}×{n}")).collect::<Vec<_>>().join(", ");
        tracing::info!(pool = verdicts.len(), kept, dropped, %summary, "radio gate filtered the pool");
        for v in verdicts.iter().filter(|v| !v.kept) {
            tracing::debug!(
                reason = v.reason,
                source = ?v.source,
                id = %v.video_id,
                title = %v.title,
                "radio gate drop"
            );
        }
    }

    /// Snapshot the current playback context into a [`StationState`] the pure engine ranks
    /// against: the seed, recently-heard tracks/artists (already-played filtering + cooldown),
    /// and favorite artists (a seed-affinity boost). Dislikes are read straight from `signals`.
    pub(in crate::app) fn build_station_state(&self, seed_video_id: &str) -> StationState {
        let mut recent_track_ids: Vec<String> = self.queue.video_ids().map(str::to_owned).collect();
        recent_track_ids.extend(self.library.history.iter().map(|s| s.video_id.clone()));

        // Cooldown window wants most-recent *last*; history is newest-first, so reverse it.
        let mut recent_artist_keys: Vec<String> = self
            .library
            .history
            .iter()
            .take(RADIO_RECENT_ARTISTS)
            .map(|s| signals::normalize_artist(&s.artist))
            .collect();
        recent_artist_keys.reverse();

        let favorite_artist_keys: HashSet<String> = self
            .library
            .favorites
            .iter()
            .map(|s| signals::normalize_artist(&s.artist))
            .collect();

        StationState {
            mode: self.config.radio.mode,
            seed_video_id: seed_video_id.to_owned(),
            seed_artist_key: self.radio_seed_artist_key(seed_video_id),
            recent_track_ids,
            recent_artist_keys,
            banned_track_ids: HashSet::new(),
            banned_artist_keys: HashSet::new(),
            favorite_artist_keys,
        }
    }

    /// The normalized artist key of the radio seed (usually the current track), for the
    /// seed-artist affinity boost. Falls back to a history lookup, then empty.
    pub(in crate::app) fn radio_seed_artist_key(&self, seed_video_id: &str) -> String {
        if let Some(cur) = self.queue.current()
            && cur.video_id == seed_video_id
        {
            return signals::normalize_artist(&cur.artist);
        }
        self.library
            .history
            .iter()
            .find(|s| s.video_id == seed_video_id)
            .map(|s| signals::normalize_artist(&s.artist))
            .unwrap_or_default()
    }

}
