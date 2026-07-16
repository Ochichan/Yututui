//! The "what's playing" (지듣노) card reducer: the open flow (populated synchronously from
//! the live radio stream's own ICY metadata — no API call, so it works with DJ Gem off),
//! the card's two actions (save the song to the music favorites, or hand off to DJ Gem for
//! the story), and live re-population when the stream's title changes under an open card.

use super::now_playing::{self};
use super::*;

impl App {
    /// Open (or toggle-close) the "what's playing" card. It reads the current radio
    /// stream's ICY title straight from `playback.stream_now_playing` and fills the card
    /// synchronously — no identify call, so it always works, DJ Gem or not. A light local
    /// heuristic routes obvious ads/station-ids to `StationContent`; a missing or
    /// station-name-echo title to `NoMetadata`; anything else is the playing song.
    pub(in crate::app) fn open_now_playing_overlay(&mut self) -> Vec<Cmd> {
        if self.overlays.now_playing_overlay.is_some() {
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
        let station_label = self.display_song_label(&station);
        let stream_np = self.playback.stream_now_playing.clone();
        let raw = stream_np
            .as_ref()
            .map(|np| np.raw.as_str())
            .unwrap_or_default();
        let sanitized = now_playing::sanitize_stream_title(raw);
        let mut overlay = NowPlayingOverlay {
            station_id: station.video_id.clone(),
            station_label,
            raw_title: sanitized.clone(),
            state: NowPlayingOverlayState::NoMetadata,
            resolved: None,
            resolving: false,
            resolve_seq: 0,
        };
        self.dirty = true;
        if now_playing::is_identifiable(
            &sanitized,
            &[station.title.as_str(), overlay.station_label.as_str()],
        ) {
            if now_playing::looks_like_station_content(&sanitized) {
                overlay.state = NowPlayingOverlayState::StationContent;
            } else {
                // Sanitize the already-parsed split for display too: the parser's
                // title/artist are space/quote-cleaned but NOT ANSI/control/wrapper-tag
                // defused — the raw ICY string is the only untrusted terminal input.
                let title = stream_np
                    .as_ref()
                    .and_then(|np| np.title.as_deref())
                    .map(now_playing::sanitize_stream_title)
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| sanitized.clone());
                let artist = stream_np
                    .as_ref()
                    .and_then(|np| np.artist.as_deref())
                    .map(now_playing::sanitize_stream_title)
                    .filter(|s| !s.is_empty());
                // Reuse a prior favorite-resolution for this exact title, if cached.
                let key = now_playing::cache_key(&overlay.station_id, &sanitized);
                overlay.resolved = self.overlays.now_playing_cache.get(&key).cloned();
                overlay.state = NowPlayingOverlayState::Playing { artist, title };
            }
        }
        self.overlays.now_playing_overlay = Some(overlay);
        Vec::new()
    }

    pub(in crate::app) fn close_now_playing_overlay(&mut self) {
        self.overlays.now_playing_overlay = None;
        // Invalidate a favorite-resolve still in flight so a late reply can't reopen state.
        self.overlays.now_playing_seq = self.overlays.now_playing_seq.wrapping_add(1);
        self.dirty = true;
    }

    /// The ICY title changed under an open card: re-populate it from the fresh (free,
    /// synchronous) metadata. This also fills in the very first title after tuning in,
    /// since mpv only surfaces ICY metadata once it first changes. The seq bump
    /// invalidates any favorite-resolve still in flight for the old title.
    pub(in crate::app) fn on_stream_title_changed(&mut self) -> Vec<Cmd> {
        if self.overlays.now_playing_overlay.is_none() {
            return Vec::new();
        }
        self.overlays.now_playing_overlay = None;
        self.overlays.now_playing_seq = self.overlays.now_playing_seq.wrapping_add(1);
        self.open_now_playing_overlay()
    }

    /// Whether the card's favorite action applies (a playing song to search for). `pub` —
    /// the overlay view mirrors it.
    pub fn now_playing_can_favorite(&self) -> bool {
        matches!(
            self.overlays.now_playing_overlay.as_ref().map(|o| &o.state),
            Some(NowPlayingOverlayState::Playing { .. })
        )
    }

    /// Whether the card's resolved track is actually in the music favorites right now — the
    /// truth for the filled-vs-empty heart. Keyed off the live library (not `resolved.is_some()`,
    /// which only means "we found the YouTube track"). `pub` — the overlay view mirrors it.
    pub fn now_playing_is_favorited(&self) -> bool {
        self.overlays
            .now_playing_overlay
            .as_ref()
            .and_then(|o| o.resolved.as_ref())
            .is_some_and(|song| self.library.is_favorite(&song.video_id))
    }

    /// Whether the card's "tell me more" action applies. Gated on DJ Gem being connected,
    /// so the button is hidden entirely when it isn't. `pub` — the overlay view mirrors it.
    pub fn now_playing_can_ask(&self) -> bool {
        self.ai.available
            && matches!(
                self.overlays.now_playing_overlay.as_ref().map(|o| &o.state),
                Some(NowPlayingOverlayState::Playing { .. })
            )
    }

    /// "Save to favorites": favorites are keyed by `video_id`, and a radio title has
    /// none — so the artist/title is first resolved to a real YouTube track (the resolved
    /// `Song` then rides the overlay AND the cache, so repeat favorites and re-opens never
    /// re-search).
    pub(in crate::app) fn now_playing_favorite(&mut self) -> Vec<Cmd> {
        if !self.now_playing_can_favorite() {
            return Vec::new();
        }
        let Some(overlay) = self.overlays.now_playing_overlay.as_ref() else {
            return Vec::new();
        };
        if overlay.resolving {
            return Vec::new();
        }
        if let Some(song) = overlay.resolved.clone() {
            return self.add_resolved_favorite(song);
        }
        let NowPlayingOverlayState::Playing { artist, title } = &overlay.state else {
            return Vec::new();
        };
        let query = match artist {
            Some(artist) => format!("{artist} {title}"),
            None => title.clone(),
        };
        self.overlays.now_playing_seq = self.overlays.now_playing_seq.wrapping_add(1);
        let seq = self.overlays.now_playing_seq;
        let overlay = self
            .overlays
            .now_playing_overlay
            .as_mut()
            .expect("checked above");
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
        let Some(overlay) = self.overlays.now_playing_overlay.as_mut() else {
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
                self.overlays
                    .now_playing_cache
                    .put_resolved(key, song.clone());
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
        self.library_mut().toggle_favorite(&song);
        self.status.text = if crate::i18n::is_korean() {
            format!("음악 즐겨찾기에 저장: {} — {}", song.title, song.artist)
        } else {
            format!("Saved to music favorites: {} — {}", song.title, song.artist)
        };
        vec![Cmd::Persist(PersistCmd::Library)]
    }

    /// "Tell me more": close the card and hand off to the DJ Gem view, seeding the normal
    /// chat path (session model — follow-ups ride the same conversation) with a labeled
    /// context block and a request for a rich, structured rundown. The stream-derived
    /// strings stay inside the `<now_playing>` block as untrusted DATA (never an imperative
    /// sentence), with a standing caution that a stream title may be mislabeled — so a bad
    /// title can't cascade into a confident essay about the wrong song. The transcript
    /// shows a compact line; the full block goes to the model.
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
        let Some(overlay) = self.overlays.now_playing_overlay.take() else {
            return Vec::new();
        };
        // The card is gone; any favorite-resolve still in flight for it is stale.
        self.overlays.now_playing_seq = self.overlays.now_playing_seq.wrapping_add(1);
        let NowPlayingOverlayState::Playing { artist, title } = &overlay.state else {
            return Vec::new();
        };
        let compact = match artist {
            Some(artist) => format!("{title} — {artist}"),
            None => title.clone(),
        };
        let split_line = match artist {
            Some(artist) => {
                format!("parsed (best-effort local split): artist={artist} · title={title}\n")
            }
            None => String::new(),
        };
        let ask = t!(
            "Give me a rich rundown of this song — the artist (background, notable work), \
             the song itself (the release it's from, its meaning, any behind-the-scenes \
             context), its genre and era, a couple of things worth knowing, and two or \
             three similar tracks I might like. Keep each part short. If the title doesn't \
             resolve to a real track, say so instead of inventing one.",
            "이 곡을 풍부하게 소개해줘 — 아티스트(배경·대표작), 이 곡(수록 앨범/발매 정보·의미·\
             비하인드), 장르와 시대, 알아두면 좋은 점, 그리고 좋아할 만한 비슷한 곡 두어 개 추천. \
             각 항목은 짧게. 제목이 실제 곡으로 확인되지 않으면 지어내지 말고 그렇다고 말해줘."
        );
        let seed = format!(
            "<now_playing>\n\
             station: {station}\n\
             raw_title (untrusted, as sent by the radio stream): {raw}\n\
             {split}\
             note: this title came straight from the stream's ICY metadata and may be \
             mislabeled, truncated, or split wrong — verify the artist/title before \
             elaborating.\n\
             </now_playing>\n\
             {ask}",
            station = overlay.station_label,
            raw = overlay.raw_title,
            split = split_line,
            ask = ask,
        );
        self.enter_ai();
        self.push_ai_message(
            AiRole::User,
            format!(
                "{} ({compact})",
                t!("Tell me more about this track", "이 곡 더 알려줘")
            ),
        );
        self.ai.thinking = true;
        self.bridges.ai_transcript_scroll.scroll_to_end();
        vec![Cmd::AskAi {
            prompt: seed,
            context: Box::new(self.build_ai_context()),
        }]
    }
}
