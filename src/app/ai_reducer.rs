//! DJ Gem assistant reducer methods, split out of the monolithic `app.rs` (behaviour-preserving).

use super::*;

/// A DJ Gem assistant intent — or one of its off-path results — applied by the reducer.
/// Bucketed under [`Msg::Ai`] to keep the flat `Msg` lean. Constructed in `runtime.rs` from
/// the leaf `AiEvent`; never imported by a leaf actor (see `scripts/check-architecture.sh`).
pub enum AiMsg {
    /// The assistant started/finished a turn (drives the thinking spinner).
    Thinking(bool),
    /// Assistant chat text to append to the transcript.
    Chat(String),
    /// An DJ Gem error to surface in the transcript (also clears the spinner).
    Error(String),
    /// Replace the queue with these tracks and start playing (play_music/play_playlist).
    PlayTracks(Vec<Song>),
    /// Append these tracks to the queue (add_to_queue/start_streaming).
    Enqueue(Vec<Song>),
    /// Populate the pickable related-tracks list (get_suggestions).
    Suggestions(Vec<Song>),
    /// Turn autoplay/streaming on or off (start_streaming/stop_streaming).
    SetAutoplay(bool),
    /// Shape the active station from a free-text vibe (start_streaming with explore/avoid hints):
    /// set the adventurousness and the artists to keep out. `explore` is the model's raw string
    /// (tight/balanced/wide or a synonym), parsed leniently.
    SetStationProfile {
        query: String,
        explore: Option<String>,
        avoid_artists: Vec<String>,
    },
    /// Create a local playlist with this name (create_playlist).
    CreatePlaylist(String),
    /// Add these tracks to a local playlist by id or name (add_to_playlist).
    AddToPlaylist { playlist: String, songs: Vec<Song> },
    /// Play a local playlist by id or name (play_playlist).
    PlayPlaylist(String),
    /// Result of an off-path feedback summary (see [`Cmd::SummarizeFeedback`]): artists the
    /// listener kept skipping vs. warmed to, folded into the active station's avoid list. Always
    /// delivered (empty on failure) so the in-flight guard clears.
    StationPatch {
        down_artists: Vec<String>,
        boost_artists: Vec<String>,
    },
    /// Result of a batch title/artist romanization request. Empty `entries` means Gemini failed or
    /// produced nothing usable; `keys` still clears the reducer's in-flight guard for those tracks.
    RomanizedTitles {
        request_id: u64,
        keys: Vec<String>,
        entries: Vec<RomanizedResult>,
    },
}

impl From<AiMsg> for Msg {
    fn from(msg: AiMsg) -> Self {
        Msg::Ai(msg)
    }
}

impl App {
    // --- DJ Gem assistant -------------------------------------------------------

    /// Enter the DJ Gem assistant screen (input focused).
    pub(in crate::app) fn enter_ai(&mut self) {
        self.mode = Mode::Ai;
        self.ai.focus = AiFocus::Input;
        self.dropdowns.eq_open = false;
        self.dropdowns.streaming_open = false;
        self.dropdowns.search_source_open = false;
        self.ai.select_all = false;
        self.status.text.clear();
        self.bridges.ai_transcript_scroll.scroll_to_end();
        self.dirty = true;
    }

    /// Cycle the committed DJ Gem model from the chat screen. Unlike the Settings tab, this
    /// is not draft state: the visible model, saved config, and live actor all update now.
    pub(in crate::app) fn cycle_ai_model_from_chat(&mut self) -> Vec<Cmd> {
        let next = self.ai.model.cycled(true);
        self.ai.model = next;
        self.config.gemini_model = next;
        self.status.kind = StatusKind::Info;
        self.status.text = format!("DJ Gem model: {}", next.label());
        self.dirty = true;
        vec![
            Cmd::Persist(PersistCmd::Config(Box::new(self.config.clone()))),
            Cmd::SetAiModel(next),
        ]
    }

    pub(in crate::app) fn on_key_ai(&mut self, k: KeyEvent) -> Vec<Cmd> {
        match self.ai.focus {
            AiFocus::Input => {
                // Ctrl+A selects the whole prompt (desktop-style); idempotent re-select.
                if matches!(
                    self.keymap.action(KeyContext::AiInput, k.into()),
                    Some(Action::SelectAll)
                ) {
                    self.ai.select_all = !self.ai.input.is_empty();
                    self.dirty = true;
                    return Vec::new();
                }
                // With the prompt selected, the next key consumes the selection: a character
                // replaces it, Backspace clears it, anything else just deselects + falls through.
                if std::mem::take(&mut self.ai.select_all) {
                    self.dirty = true;
                    let chord = Chord::from(k);
                    if chord.is_typeable()
                        && let KeyCode::Char(c) = k.code
                    {
                        self.ai.input.clear();
                        self.ai.input.push(c);
                        return Vec::new();
                    }
                    if matches!(
                        self.keymap.action(KeyContext::AiInput, k.into()),
                        Some(Action::DeleteChar)
                    ) {
                        self.ai.input.clear();
                        return Vec::new();
                    }
                }
                let chord = Chord::from(k);
                if chord.is_typeable()
                    && let KeyCode::Char(c) = k.code
                {
                    self.ai.input.push(c);
                    self.dirty = true;
                    return Vec::new();
                }
                match self.keymap.action(KeyContext::AiInput, k.into()) {
                    Some(Action::Back) => {
                        self.mode = Mode::Player;
                        self.dirty = true;
                        return Vec::new();
                    }
                    Some(Action::Confirm) => return self.submit_ai_prompt(),
                    Some(Action::DeleteChar) => {
                        self.ai.input.pop();
                        self.dirty = true;
                        return Vec::new();
                    }
                    // Drop into the suggestions list (if any) to pick a track.
                    Some(Action::MoveDown | Action::FocusNext)
                        if !self.ai.suggestions.is_empty() =>
                    {
                        self.ai.focus = AiFocus::Suggestions;
                        self.dirty = true;
                        return Vec::new();
                    }
                    _ => {}
                }
                Vec::new()
            }
            AiFocus::Suggestions => match self.keymap.action(KeyContext::AiSuggestions, k.into()) {
                Some(Action::Back) => {
                    self.mode = Mode::Player;
                    self.dirty = true;
                    Vec::new()
                }
                Some(Action::MoveUp) => {
                    if self.ai.suggestions_selected == 0 {
                        self.ai.focus = AiFocus::Input;
                    } else {
                        self.ai.suggestions_selected -= 1;
                    }
                    self.dirty = true;
                    Vec::new()
                }
                Some(Action::MoveDown) => {
                    if self.ai.suggestions_selected + 1 < self.ai.suggestions.len() {
                        self.ai.suggestions_selected += 1;
                    }
                    self.dirty = true;
                    Vec::new()
                }
                Some(Action::FocusNext) => {
                    self.ai.focus = AiFocus::Input;
                    self.dirty = true;
                    Vec::new()
                }
                Some(Action::Confirm) => self.play_ai_suggestion(),
                _ => Vec::new(),
            },
        }
    }

    /// Submit the typed prompt to the assistant (or show onboarding if no key).
    pub(in crate::app) fn submit_ai_prompt(&mut self) -> Vec<Cmd> {
        let prompt = self.ai.input.trim().to_owned();
        if prompt.is_empty() {
            return Vec::new();
        }
        // Cap the prompt well above any realistically-typed message so it never fires in normal
        // use, but a pasted wall of text (or a script-driven prompt) can't bloat the request,
        // the transcript, or the logs. History trimming already runs on the response side.
        const PROMPT_MAX: usize = 16 * 1024;
        let prompt = if prompt.len() > PROMPT_MAX {
            let end = prompt.floor_char_boundary(PROMPT_MAX);
            prompt[..end].to_owned()
        } else {
            prompt
        };
        self.ai.input.clear();
        self.ai.select_all = false;
        self.push_ai_message(AiRole::User, prompt.clone());
        self.dirty = true;
        if !self.ai.available {
            self.push_ai_message(
                AiRole::Error,
                // Saving a key in Settings now brings the assistant up live (no restart).
                "No Gemini API key. Add one under Settings > DJ Gem or set GEMINI_API_KEY."
                    .to_owned(),
            );
            return Vec::new();
        }
        // Ignore a new prompt while one is in flight (the spinner is showing).
        if self.ai.thinking {
            return Vec::new();
        }
        self.ai.thinking = true;
        self.bridges.ai_transcript_scroll.scroll_to_end();
        vec![Cmd::AskAi {
            prompt,
            context: Box::new(self.build_ai_context()),
        }]
    }

    /// Play the highlighted suggestion, queuing the whole list from that point.
    pub(in crate::app) fn play_ai_suggestion(&mut self) -> Vec<Cmd> {
        if self.ai.suggestions.is_empty() {
            return Vec::new();
        }
        let start = self
            .ai
            .suggestions_selected
            .min(self.ai.suggestions.len() - 1);
        let requested_songs = self.ai.suggestions.clone();
        self.replace_queue_and_load(
            requested_songs,
            start,
            None,
            QueueReplacementOptions {
                romanize_all: true,
                ..QueueReplacementOptions::default()
            },
        )
    }

    /// Append a line to the DJ Gem transcript, bounding its length.
    pub(in crate::app) fn push_ai_message(&mut self, role: AiRole, text: String) {
        self.ai.messages.push(AiMessage { role, text });
        if self.ai.messages.len() > AI_HISTORY_MAX {
            let overflow = self.ai.messages.len() - AI_HISTORY_MAX;
            self.ai.messages.drain(0..overflow);
        }
        self.ai.transcript_revision = self.ai.transcript_revision.wrapping_add(1);
        self.bridges.ai_transcript_scroll.scroll_to_end();
    }

    /// Snapshot the read-only state the DJ Gem actor needs to answer its read tools.
    pub(in crate::app) fn build_ai_context(&self) -> AiContext {
        let fmt = |s: &Song| format!("{} — {}", s.title, s.artist);
        let current_radio_station = self
            .queue
            .current()
            .filter(|song| song.is_radio_station())
            .map(|song| self.display_song_label(song));
        let current_radio_now_playing = current_radio_station
            .as_ref()
            .and(self.playback.stream_now_playing.as_ref())
            .map(StreamNowPlaying::label);
        AiContext {
            current_track: self.queue.current().map(fmt),
            current_radio_station,
            current_radio_now_playing,
            queue_upcoming: self.queue.upcoming(10).into_iter().map(fmt).collect(),
            queue_len: self.queue.len(),
            queue_remaining: self.queue.remaining(),
            recent_history: self.library.history.iter().take(5).map(fmt).collect(),
            favorites: self.library.favorites.iter().take(20).map(fmt).collect(),
            playlists: self
                .playlists
                .list()
                .iter()
                .map(|p| PlaylistInfo {
                    id: p.id.clone(),
                    name: p.name.clone(),
                    count: p.songs.len(),
                })
                .collect(),
            search: self.config.effective_search(),
            authenticated: self.authenticated,
            autoplay_streaming: self.autoplay_streaming,
        }
    }
}
