use std::borrow::Cow;

use super::*;

const ROMANIZE_BATCH: usize = 50;

impl App {
    pub fn display_title<'a>(&'a self, song: &'a Song) -> Cow<'a, str> {
        if self.config.effective_romanized_titles() {
            self.romanization.cache.display_title(song)
        } else {
            Cow::Borrowed(song.title.as_str())
        }
    }

    pub fn display_artist<'a>(&'a self, song: &'a Song) -> Cow<'a, str> {
        if self.config.effective_romanized_titles() {
            self.romanization.cache.display_artist(song)
        } else {
            Cow::Borrowed(song.artist.as_str())
        }
    }

    pub fn display_song_label(&self, song: &Song) -> String {
        let title = self.display_title(song);
        let artist = self.display_artist(song);
        if artist.trim().is_empty() {
            title.into_owned()
        } else {
            format!("{title} — {artist}")
        }
    }

    pub(in crate::app) fn request_romanization_for_songs(&mut self, songs: &[Song]) -> Vec<Cmd> {
        if !self.config.effective_romanized_titles() || songs.is_empty() {
            return Vec::new();
        }

        let mut cmds = Vec::new();
        let mut dirty_cache = false;
        let gemini_available = self.config.effective_gemini_api_key().is_some();
        let mut items = Vec::new();

        for song in songs {
            dirty_cache |= self.romanization.cache.ensure_local(song);
            if !gemini_available {
                continue;
            }
            let Some(item) = self.romanization.cache.gemini_candidate(song) else {
                continue;
            };
            if self.romanization.pending.insert(item.key.clone()) {
                items.push(item);
            }
        }

        if dirty_cache {
            self.dirty = true;
            cmds.push(Cmd::Persist(PersistCmd::RomanizedTitles));
        }

        for chunk in items.chunks(ROMANIZE_BATCH) {
            self.romanization.next_request_id = self.romanization.next_request_id.saturating_add(1);
            cmds.push(Cmd::RomanizeTitles {
                request_id: self.romanization.next_request_id,
                items: chunk.to_vec(),
            });
        }

        cmds
    }

    pub(in crate::app) fn request_current_surfaces_romanization(&mut self) -> Vec<Cmd> {
        if !self.config.effective_romanized_titles() {
            return Vec::new();
        }

        // Only rows that can actually need an overlay are worth cloning — both
        // `ensure_local` and `gemini_candidate` early-return on this same predicate,
        // so pre-filtering here is behavior-identical and skips the (typically
        // Latin-majority) bulk of the queue/search/library surfaces.
        let needs = |song: &&Song| crate::romanize::needs_latinization(&song.title, &song.artist);

        let mut songs = Vec::new();
        if let Some(song) = self.queue.current().filter(|s| needs(s)) {
            songs.push(song.clone());
        }
        songs.extend(self.queue.ordered_iter().filter(needs).cloned());
        songs.extend(self.search.results.iter().filter(needs).cloned());
        songs.extend(self.ai.suggestions.iter().filter(needs).cloned());

        let rows = self.library_rows();
        let viewport = self.bridges.library_scroll.viewport().max(50);
        let start = self.bridges.library_scroll.offset().min(rows.len());
        songs.extend(
            rows.into_iter()
                .skip(start)
                .take(viewport.min(100))
                .filter(|s| needs(s))
                .cloned(),
        );

        self.request_romanization_for_songs(&songs)
    }

    pub(in crate::app) fn apply_romanized_titles(
        &mut self,
        request_id: u64,
        keys: Vec<String>,
        entries: Vec<RomanizedResult>,
    ) -> Vec<Cmd> {
        if request_id < self.romanization.min_valid_request_id {
            return Vec::new();
        }

        for key in &keys {
            self.romanization.pending.remove(key);
        }

        let upgraded = self.romanization.cache.apply_gemini_results(&entries);

        if upgraded {
            self.dirty = true;
            vec![Cmd::Persist(PersistCmd::RomanizedTitles)]
        } else {
            Vec::new()
        }
    }
}
