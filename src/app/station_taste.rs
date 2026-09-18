use super::*;
use crate::streaming::{TasteEdit, TasteError, TasteOutcome};

impl App {
    pub(in crate::app) fn ban_current_track(&mut self) -> Vec<Cmd> {
        let Some(song) = self.queue.current() else {
            return Vec::new();
        };
        match TasteEdit::ban_track(song) {
            Ok(edit) => self.commit_taste_ban(edit),
            Err(TasteError::MissingTrackId) => Vec::new(),
            Err(_) => Vec::new(),
        }
    }

    pub(in crate::app) fn ban_current_artist(&mut self) -> Vec<Cmd> {
        let Some(song) = self.queue.current().cloned() else {
            return Vec::new();
        };
        match TasteEdit::ban_artist(&song) {
            Ok(edit) => self.commit_taste_ban(edit),
            Err(TasteError::MissingArtist) => {
                self.set_status_info(t!(
                    "This track has no artist to ban",
                    "이 곡에는 차단할 아티스트가 없어요",
                    "この曲には禁止できるアーティストがありません"
                ));
                Vec::new()
            }
            Err(_) => Vec::new(),
        }
    }

    fn commit_taste_ban(&mut self, edit: TasteEdit) -> Vec<Cmd> {
        self.cancel_pending_streaming_recommendation();
        let Some((mutation, outcome)) = self.queue.prepare_purge(|song| edit.rejects(song)) else {
            let _ = self.streaming.taste.apply(edit);
            self.dirty = true;
            return self.force_autoplay_extend();
        };
        let detached = matches!(outcome.playback(), crate::queue::QueueRemovalPlayback::Stop)
            .then(|| self.queue.current().cloned())
            .flatten();
        let post_commit = super::track_transition::TrackPostCommit {
            taste: Some(edit),
            force_autoplay_extend: true,
            detached_refill: detached,
            ..super::track_transition::TrackPostCommit::default()
        };
        self.apply_queue_removal(mutation, outcome, post_commit, Some(false))
    }

    pub(in crate::app) fn apply_taste_edit(&mut self, edit: TasteEdit) -> Vec<Cmd> {
        match self.streaming.taste.apply(edit) {
            TasteOutcome::Applied => {
                self.cancel_pending_streaming_recommendation();
                self.dirty = true;
                if self.streaming_active() {
                    self.force_autoplay_extend()
                } else {
                    Vec::new()
                }
            }
            TasteOutcome::AlreadyApplied => {
                self.dirty = true;
                Vec::new()
            }
            TasteOutcome::SeedLimitReached => {
                self.set_status_info(t!(
                    "At most 12 seed terms this session",
                    "이번 세션 시드는 최대 12개예요",
                    "このセッションのシードは最大12個です"
                ));
                Vec::new()
            }
        }
    }

    pub(in crate::app) fn open_station_card(&mut self) -> Vec<Cmd> {
        if self.overlays.station_card.is_none() {
            self.overlays.station_card = Some(StationCard::default());
            self.dirty = true;
        }
        Vec::new()
    }

    pub(in crate::app) fn close_station_card(&mut self) {
        if self.overlays.station_card.take().is_some() {
            self.dirty = true;
        }
    }

    pub(in crate::app) fn station_card_key(&mut self, k: KeyEvent, chord: Chord) -> Vec<Cmd> {
        let close = k.code == KeyCode::Esc
            || matches!(
                self.keymap.action(KeyContext::Common, chord),
                Some(Action::Back)
            );
        if close {
            self.close_station_card();
            return Vec::new();
        }
        if matches!(
            self.keymap.context_action(KeyContext::Station, chord),
            Some(Action::OpenStationCard)
        ) {
            self.close_station_card();
            return Vec::new();
        }

        match self.keymap.action(KeyContext::Common, chord) {
            Some(Action::MoveUp) => {
                if let Some(card) = self.overlays.station_card.as_mut() {
                    card.selected = card.selected.saturating_sub(1);
                    self.dirty = true;
                }
                return Vec::new();
            }
            Some(Action::MoveDown) => {
                if let Some(card) = self.overlays.station_card.as_mut() {
                    let last = self.streaming.taste.entries().len().saturating_sub(1);
                    card.selected = (card.selected + 1).min(last);
                    self.dirty = true;
                }
                return Vec::new();
            }
            Some(Action::Confirm) => return self.station_card_commit(),
            Some(action) => {
                if let Some(card) = self.overlays.station_card.as_mut()
                    && apply_text_edit_action(action, &mut card.cursor, &mut card.input).is_some()
                {
                    self.dirty = true;
                    return Vec::new();
                }
            }
            None => {}
        }

        if matches!(
            self.keymap.context_action(KeyContext::StationCard, chord),
            Some(Action::StationForget)
        ) {
            return self.station_card_forget();
        }

        if chord.is_typeable()
            && let KeyCode::Char(c) = k.code
            && let Some(card) = self.overlays.station_card.as_mut()
        {
            card.cursor.insert_char(&mut card.input, c);
            self.dirty = true;
        }
        Vec::new()
    }

    pub(in crate::app) fn station_card_commit(&mut self) -> Vec<Cmd> {
        let Some(card) = self.overlays.station_card.as_mut() else {
            return Vec::new();
        };
        let raw = std::mem::take(&mut card.input);
        card.cursor = TextCursor::default();
        let edit = if raw.trim().is_empty() {
            let Some(song) = self.queue.current() else {
                return Vec::new();
            };
            match TasteEdit::seed_current_artist(song) {
                Ok(edit) => edit,
                Err(_) => return Vec::new(),
            }
        } else {
            match TasteEdit::parse_seed(&raw) {
                Ok(edit) => edit,
                Err(_) => return Vec::new(),
            }
        };
        self.apply_taste_edit(edit)
    }

    pub(in crate::app) fn station_card_forget(&mut self) -> Vec<Cmd> {
        let Some(index) = self.overlays.station_card.as_ref().map(|c| c.selected) else {
            return Vec::new();
        };
        if !self.streaming.taste.forget_at(index) {
            return Vec::new();
        }
        if let Some(card) = self.overlays.station_card.as_mut() {
            let last = self.streaming.taste.entries().len().saturating_sub(1);
            card.selected = card.selected.min(last);
        }
        self.cancel_pending_streaming_recommendation();
        self.dirty = true;
        if self.streaming_active() {
            self.force_autoplay_extend()
        } else {
            Vec::new()
        }
    }
}
