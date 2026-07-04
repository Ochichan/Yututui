//! The "what's playing" (지듣노) overlay reducer: the open flow (with its no-spend
//! short-circuits), the identify round-trip, and the overlay's two actions.

use super::now_playing::{self};
use super::*;

impl App {
    /// Open (or toggle-close) the "what's playing" overlay. Ordered so every path that
    /// can answer without an API call does: not-radio / DJ Gem off → status note only;
    /// no usable title → `NoMetadata`; cache hit → instant `Identified`; a failure
    /// seconds ago → its error re-shown. Only a genuinely new title spends a call.
    pub(in crate::app) fn open_now_playing_overlay(&mut self) -> Vec<Cmd> {
        if self.now_playing_overlay.is_some() {
            self.close_now_playing_overlay();
            return Vec::new();
        }
        let Some(station) = self
            .queue
            .current()
            .filter(|song| song.is_radio_station())
            .cloned()
        else {
            self.status.kind = StatusKind::Info;
            self.status.text = t!(
                "Only for live radio stations",
                "라디오 방송에서만 쓸 수 있어요"
            )
            .to_owned();
            self.dirty = true;
            return Vec::new();
        };
        if !self.ai.available {
            self.status.kind = StatusKind::Info;
            self.status.text = t!(
                "DJ Gem is off — add a Gemini key in Settings to identify songs",
                "DJ Gem이 꺼져 있어요 — 설정에서 Gemini 키를 추가하면 곡을 알 수 있어요"
            )
            .to_owned();
            self.dirty = true;
            return Vec::new();
        }
        let station_label = self.display_song_label(&station);
        let raw = self
            .playback
            .stream_now_playing
            .as_ref()
            .map(|np| np.raw.as_str())
            .unwrap_or_default();
        let sanitized = now_playing::sanitize_stream_title(raw);
        self.now_playing_seq = self.now_playing_seq.wrapping_add(1);
        let mut overlay = NowPlayingOverlay {
            seq: self.now_playing_seq,
            station_id: station.video_id.clone(),
            station_label,
            raw_title: sanitized.clone(),
            state: NowPlayingOverlayState::Loading,
            resolved: None,
            resolving: false,
            resolve_seq: 0,
        };
        self.dirty = true;
        if !now_playing::is_identifiable(
            &sanitized,
            &[station.title.as_str(), overlay.station_label.as_str()],
        ) {
            overlay.state = NowPlayingOverlayState::NoMetadata;
            self.now_playing_overlay = Some(overlay);
            return Vec::new();
        }
        let key = now_playing::cache_key(&overlay.station_id, &sanitized);
        if let Some(entry) = self.now_playing_cache.get(&key) {
            overlay.state = NowPlayingOverlayState::Identified(entry.result.clone());
            overlay.resolved = entry.resolved.clone();
            self.now_playing_overlay = Some(overlay);
            return Vec::new();
        }
        if let Some(err) = self.now_playing_cache.recent_error(&key) {
            overlay.state = NowPlayingOverlayState::Error(err.to_owned());
            self.now_playing_overlay = Some(overlay);
            return Vec::new();
        }
        let cmd = Cmd::IdentifyNowPlaying {
            seq: overlay.seq,
            station: overlay.station_label.clone(),
            raw_title: sanitized,
        };
        self.now_playing_overlay = Some(overlay);
        vec![cmd]
    }

    pub(in crate::app) fn close_now_playing_overlay(&mut self) {
        self.now_playing_overlay = None;
        // Invalidate anything still in flight so a late reply can't reopen state.
        self.now_playing_seq = self.now_playing_seq.wrapping_add(1);
        self.dirty = true;
    }

    /// Fold an identify reply into the overlay (and the cache). Stale replies — the
    /// overlay closed, or the title moved on and re-keyed — are dropped by the seq guard.
    pub(in crate::app) fn on_now_playing_identified(
        &mut self,
        seq: u64,
        result: Result<IdentifiedNowPlaying, String>,
    ) {
        let Some(overlay) = self.now_playing_overlay.as_mut() else {
            return;
        };
        if overlay.seq != seq {
            return;
        }
        let key = now_playing::cache_key(&overlay.station_id, &overlay.raw_title);
        match result {
            Ok(identified) => {
                // Every successful verdict is cacheable — an "ad"/"unknown" answer for
                // this title is as reusable as a song.
                self.now_playing_cache.put(key, identified.clone());
                overlay.state = NowPlayingOverlayState::Identified(identified);
            }
            Err(message) => {
                // Failures get a short soft-negative window, never a cached result.
                self.now_playing_cache.note_error(key, message.clone());
                overlay.state = NowPlayingOverlayState::Error(message);
            }
        }
        self.dirty = true;
    }

    /// The ICY title changed under an open overlay. A `Loading` overlay re-runs the open
    /// flow for the fresh title (the cache may answer for free); anything already
    /// displayed stays — the song just ended, and the card is still what was asked for.
    /// The seq bump in either path invalidates the in-flight reply for the old title.
    pub(in crate::app) fn on_stream_title_changed(&mut self) -> Vec<Cmd> {
        match self.now_playing_overlay.as_ref().map(|o| &o.state) {
            Some(NowPlayingOverlayState::Loading) => {
                self.now_playing_overlay = None;
                self.now_playing_seq = self.now_playing_seq.wrapping_add(1);
                self.open_now_playing_overlay()
            }
            Some(_) => {
                self.now_playing_seq = self.now_playing_seq.wrapping_add(1);
                Vec::new()
            }
            None => Vec::new(),
        }
    }

    /// Whether the overlay's favorite action applies to its current state (an identified
    /// song with at least a title to search for). `pub` — the overlay view mirrors it.
    pub fn now_playing_can_favorite(&self) -> bool {
        matches!(
            self.now_playing_overlay.as_ref().map(|o| &o.state),
            Some(NowPlayingOverlayState::Identified(id))
                if id.kind == IdentifiedKind::Song && id.title.is_some()
        )
    }

    /// Whether the overlay's "tell me more" action applies (anything but station
    /// content — an uncertain identification is still worth asking about). `pub` — the
    /// overlay view mirrors it.
    pub fn now_playing_can_ask(&self) -> bool {
        match self.now_playing_overlay.as_ref().map(|o| &o.state) {
            Some(NowPlayingOverlayState::Identified(id)) => id.kind != IdentifiedKind::Ad,
            _ => false,
        }
    }

    /// "Save to favorites": favorites are keyed by `video_id`, and a radio title has
    /// none — so the identified artist/title is first resolved to a real YouTube track
    /// (the resolved `Song` then rides the overlay AND the cache entry, so repeat
    /// favorites and re-opens never re-search). A title-only entry is never inserted.
    pub(in crate::app) fn now_playing_favorite(&mut self) -> Vec<Cmd> {
        if !self.now_playing_can_favorite() {
            return Vec::new();
        }
        let Some(overlay) = self.now_playing_overlay.as_ref() else {
            return Vec::new();
        };
        if overlay.resolving {
            return Vec::new();
        }
        if let Some(song) = overlay.resolved.clone() {
            return self.add_resolved_favorite(song);
        }
        let NowPlayingOverlayState::Identified(id) = &overlay.state else {
            return Vec::new();
        };
        let query = match (&id.artist, &id.title) {
            (Some(artist), Some(title)) => format!("{artist} {title}"),
            (None, Some(title)) => title.clone(),
            _ => return Vec::new(),
        };
        self.now_playing_seq = self.now_playing_seq.wrapping_add(1);
        let seq = self.now_playing_seq;
        let overlay = self.now_playing_overlay.as_mut().expect("checked above");
        overlay.resolving = true;
        overlay.resolve_seq = seq;
        self.dirty = true;
        vec![Cmd::ResolveTrack {
            seq,
            query,
            config: self.config.effective_search(),
        }]
    }

    /// Fold a track-resolve reply into the overlay: attach the best match (overlay +
    /// cache) and complete the favorite add. Stale/duplicate replies are dropped.
    pub(in crate::app) fn on_track_resolved(
        &mut self,
        seq: u64,
        result: Result<Vec<Song>, String>,
    ) -> Vec<Cmd> {
        let Some(overlay) = self.now_playing_overlay.as_mut() else {
            return Vec::new();
        };
        if overlay.resolve_seq != seq || !overlay.resolving {
            return Vec::new();
        }
        overlay.resolving = false;
        self.dirty = true;
        match result {
            Ok(songs) => {
                let Some(song) = songs.into_iter().next() else {
                    self.status.kind = StatusKind::Error;
                    self.status.text = t!(
                        "Couldn't find this track on YouTube",
                        "YouTube에서 이 곡을 찾지 못했어요"
                    )
                    .to_owned();
                    return Vec::new();
                };
                overlay.resolved = Some(song.clone());
                let key = now_playing::cache_key(&overlay.station_id, &overlay.raw_title);
                self.now_playing_cache.attach_resolved(&key, song.clone());
                self.add_resolved_favorite(song)
            }
            Err(error) => {
                self.status.kind = StatusKind::Error;
                self.status.text = error;
                Vec::new()
            }
        }
    }

    /// Add a resolved YouTube track to the music-mode favorites, idempotently.
    /// `toggle_favorite` is a toggle — the `is_favorite` precheck keeps a repeat press
    /// from *removing* the song. A Youtube-source track routes to the music favorites
    /// (not `radio_favorites`) by `Library`'s own rules. The toast names exactly what
    /// was saved, which library it went to, and doubles as the wrong-song tripwire.
    fn add_resolved_favorite(&mut self, song: Song) -> Vec<Cmd> {
        self.status.kind = StatusKind::Info;
        self.dirty = true;
        if self.library.is_favorite(&song.video_id) {
            self.status.text =
                t!("Already in music favorites", "이미 음악 즐겨찾기에 있어요").to_owned();
            return Vec::new();
        }
        self.library.toggle_favorite(&song);
        self.status.text = if crate::i18n::is_korean() {
            format!("음악 즐겨찾기에 저장: {} — {}", song.title, song.artist)
        } else {
            format!("Saved to music favorites: {} — {}", song.title, song.artist)
        };
        vec![Cmd::SaveLibrary]
    }

    /// "Tell me more": close the card and hand off to the DJ Gem view, seeding the
    /// normal chat path (session model — follow-ups ride the same conversation) with a
    /// labeled context block. The block, not an imperative sentence, carries the
    /// stream-derived strings — they stay untrusted data even after identification —
    /// and a medium/low confidence swaps in an acknowledge-the-uncertainty preamble so
    /// a misidentification can't cascade into a confident essay about the wrong artist.
    /// The transcript shows only a compact line; the full block goes to the model.
    pub(in crate::app) fn now_playing_ask_ai(&mut self) -> Vec<Cmd> {
        if !self.now_playing_can_ask() {
            return Vec::new();
        }
        if self.ai.thinking {
            self.status.kind = StatusKind::Info;
            self.status.text = t!(
                "DJ Gem is busy — try again in a moment",
                "DJ Gem이 응답 중이에요 — 잠시 후 다시 시도해 주세요"
            )
            .to_owned();
            self.dirty = true;
            return Vec::new();
        }
        let Some(overlay) = self.now_playing_overlay.take() else {
            return Vec::new();
        };
        // The card is gone; anything still in flight for it is stale.
        self.now_playing_seq = self.now_playing_seq.wrapping_add(1);
        let NowPlayingOverlayState::Identified(id) = &overlay.state else {
            return Vec::new();
        };
        let identified_line = match (&id.artist, &id.title) {
            (Some(artist), Some(title)) => format!("{title} — {artist}"),
            (None, Some(title)) => title.clone(),
            (Some(artist), None) => artist.clone(),
            (None, None) => "unknown".to_owned(),
        };
        let confidence = match id.confidence {
            IdentifyConfidence::High => "high",
            IdentifyConfidence::Medium => "medium",
            IdentifyConfidence::Low => "low",
        };
        let kind = match id.kind {
            IdentifiedKind::Song => "song",
            IdentifiedKind::Ad => "ad",
            IdentifiedKind::Unknown => "unknown",
        };
        let ask = t!(
            "Tell me more about this song and its artist.",
            "이 곡과 아티스트에 대해 더 알려줘."
        );
        let preamble = if matches!(
            id.confidence,
            IdentifyConfidence::Medium | IdentifyConfidence::Low
        ) || id.kind != IdentifiedKind::Song
        {
            t!(
                "The identification is uncertain — acknowledge that first. ",
                "곡 식별이 불확실해요 — 먼저 그 점을 짚고 시작해줘. "
            )
        } else {
            ""
        };
        let seed = format!(
            "<now_playing>\n\
             station: {}\n\
             raw_title (untrusted, as sent by the stream): {}\n\
             identified: {} (confidence: {}, kind: {})\n\
             </now_playing>\n\
             {}{}",
            overlay.station_label,
            overlay.raw_title,
            identified_line,
            confidence,
            kind,
            preamble,
            ask
        );
        self.enter_ai();
        self.push_ai_message(AiRole::User, format!("{ask} ({identified_line})"));
        if !self.ai.available {
            // Same onboarding path as a typed prompt (submit_ai_prompt).
            self.push_ai_message(
                AiRole::Error,
                "No Gemini API key. Add one in Settings (press ,) or set GEMINI_API_KEY."
                    .to_owned(),
            );
            return Vec::new();
        }
        self.ai.thinking = true;
        self.bridges.ai_transcript_scroll.scroll_to_end();
        vec![Cmd::AskAi {
            prompt: seed,
            context: Box::new(self.build_ai_context()),
        }]
    }
}
