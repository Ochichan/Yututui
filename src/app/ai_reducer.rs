//! DJ Gem assistant reducer methods, split out of the monolithic `app.rs` (behaviour-preserving).

use super::*;

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
        self.dirty = true;
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
        self.ai.input.clear();
        self.ai.select_all = false;
        self.push_ai_message(AiRole::User, prompt.clone());
        self.dirty = true;
        if !self.ai.available {
            self.push_ai_message(
                AiRole::Error,
                // Saving a key in Settings now brings the assistant up live (no restart).
                "No Gemini API key. Add one in Settings (press ,) or set GEMINI_API_KEY."
                    .to_owned(),
            );
            return Vec::new();
        }
        // Ignore a new prompt while one is in flight (the spinner is showing).
        if self.ai.thinking {
            return Vec::new();
        }
        self.ai.thinking = true;
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
        self.queue.set(requested_songs.clone(), start);
        self.status.text.clear();
        let song = self.queue.current().cloned();
        let mut cmds = self.load_song(song);
        cmds.extend(self.request_romanization_for_songs(&requested_songs));
        cmds
    }

    /// Append a line to the DJ Gem transcript, bounding its length.
    pub(in crate::app) fn push_ai_message(&mut self, role: AiRole, text: String) {
        self.ai.messages.push(AiMessage { role, text });
        if self.ai.messages.len() > AI_HISTORY_MAX {
            let overflow = self.ai.messages.len() - AI_HISTORY_MAX;
            self.ai.messages.drain(0..overflow);
        }
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
            authenticated: self.authenticated,
            autoplay_streaming: self.autoplay_streaming,
        }
    }
}
