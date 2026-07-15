//! Collection-wide, offline-only Local Find reducer.

use std::collections::BTreeMap;

use super::*;
use crate::local::find::{
    LocalFindCommand, LocalFindCorpusOptions, LocalFindCorpusRevision, LocalFindHit,
    LocalFindHitId, LocalFindPlaylistEntryInput, LocalFindPlaylistInput, LocalFindQuery,
    LocalFindScope, LocalFindSort,
};
use crate::util::query::{MAX_SEARCH_QUERY_BYTES, try_insert_query_char};

const FIND_SCOPES: [LocalFindScope; 7] = [
    LocalFindScope::All,
    LocalFindScope::Tracks,
    LocalFindScope::Albums,
    LocalFindScope::Artists,
    LocalFindScope::Genres,
    LocalFindScope::Folders,
    LocalFindScope::Playlists,
];

const LOCAL_FIND_RESCAN: usize = 6;
const LOCAL_FIND_CLEAR_QUERY: usize = 7;
const LOCAL_FIND_ADD_FOLDER: usize = 8;
const LOCAL_FIND_SCAN_ERRORS: usize = 9;

impl App {
    /// Enter the shared Search navigation slot without touching normal/Radio Search state.
    pub(in crate::app) fn open_local_find(&mut self) -> Vec<Cmd> {
        if !self.local_dedicated_mode {
            return Vec::new();
        }
        if self.mode != Mode::Search {
            self.local_mode.find.return_section = self.local_mode.ui.section;
        }
        self.mode = Mode::Search;
        self.search_filter.close();
        self.dropdowns.search_source_open = false;
        self.local_mode.find.focus = LocalFindFocus::Input;
        self.local_mode.find.input_cursor = TextCursor::at_end(&self.local_mode.find.query);
        self.local_mode.find.select_all = false;
        self.local_mode.find.refine_popup.open = false;
        self.local_mode.find.pending_bulk_confirm = None;
        self.dirty = true;
        let mut commands = self.ensure_local_find_corpus();
        if commands.is_empty()
            && self.local_mode.find.snapshot.is_none()
            && !self.local_mode.find.query.trim().is_empty()
        {
            commands.extend(self.submit_local_find_query());
        }
        commands
    }

    pub(in crate::app) fn close_local_find(&mut self) -> Vec<Cmd> {
        self.local_mode.find.select_all = false;
        self.local_mode.find.refine_popup.open = false;
        self.local_mode.find.pending_bulk_confirm = None;
        self.local_mode.find.pending_rebuild_confirm = false;
        self.mode = Mode::Library;
        // Find is an overlay navigation surface: the underlying Local Deck section, drill,
        // cursor, pane and scroll never moved while it was open. Restoring through
        // `switch_local_section` would erase all of that state.
        self.local_mode.ui.section = self.local_mode.find.return_section;
        self.dirty = true;
        Vec::new()
    }

    fn local_find_revision(&self) -> LocalFindCorpusRevision {
        let download_dir = self.config.effective_download_dir();
        let download_dir = download_dir.to_string_lossy();
        LocalFindCorpusRevision {
            index: self.local_mode.index.revision,
            playlists: self.playlists.revision(),
            downloads: if self.local_mode.index.index.is_empty() {
                self.library_ui.downloaded_rev
            } else {
                0
            },
            options: crate::local::model::stable_hash_segments(&[download_dir.as_bytes()]),
        }
    }

    pub(in crate::app) fn local_find_corpus_is_current(&self) -> bool {
        self.local_mode
            .find
            .corpus
            .as_ref()
            .is_some_and(|corpus| corpus.revision() == self.local_find_revision())
    }

    pub(in crate::app) fn local_find_visible_revision_is_current(&self) -> bool {
        self.local_mode.find.drill.as_ref().map_or_else(
            || {
                self.local_mode
                    .find
                    .snapshot
                    .as_ref()
                    .is_some_and(|snapshot| {
                        snapshot.corpus_revision == self.local_find_revision()
                            && snapshot.generation == self.local_mode.find.request_id
                    })
            },
            |drill| drill.corpus_revision == self.local_find_revision(),
        )
    }

    fn note_local_find_refreshing(&mut self) -> Vec<Cmd> {
        self.status.kind = StatusKind::Info;
        self.status.text = t!(
            "Local Find is refreshing; try again in a moment",
            "로컬 찾기를 새로 고치는 중입니다. 잠시 후 다시 시도하세요"
        )
        .to_owned();
        self.dirty = true;
        Vec::new()
    }

    /// Rebuild only when an owned source revision changes; no corpus clone occurs per keypress.
    pub(in crate::app) fn ensure_local_find_corpus(&mut self) -> Vec<Cmd> {
        self.local_mode.find.selected = self
            .local_mode
            .find
            .selected
            .min(self.local_find_rows_len().saturating_sub(1));
        let revision = self.local_find_revision();
        if self
            .local_mode
            .find
            .corpus
            .as_ref()
            .is_some_and(|corpus| corpus.revision() == revision)
            || self.local_mode.find.building_revision == Some(revision)
        {
            return Vec::new();
        }

        self.local_mode.find.corpus_generation = self
            .local_mode
            .find
            .corpus_generation
            .wrapping_add(1)
            .max(1);
        let generation = self.local_mode.find.corpus_generation;
        self.local_mode.find.building_revision = Some(revision);
        self.local_mode.find.searching = !self.local_mode.find.query.trim().is_empty();
        self.dirty = true;

        let tracks = if self.local_mode.index.index.is_empty() {
            self.local_find_downloaded_fallback()
        } else {
            self.local_mode.index.index.tracks().to_vec()
        };
        let playlists = self.local_find_playlist_inputs(&tracks);
        let options = LocalFindCorpusOptions {
            downloaded_roots: vec![self.config.effective_download_dir()],
        };
        vec![Cmd::Local(LocalCmd::BuildFindCorpus {
            generation,
            tracks,
            playlists,
            revision,
            options,
        })]
    }

    fn local_find_playlist_inputs(
        &self,
        tracks: &[crate::local::LocalTrack],
    ) -> Vec<LocalFindPlaylistInput> {
        let by_path: BTreeMap<_, _> = tracks
            .iter()
            .map(|track| (track.path.clone(), track.id.clone()))
            .collect();
        self.playlists
            .list()
            .iter()
            .map(|playlist| LocalFindPlaylistInput {
                id: playlist.id.clone(),
                name: playlist.name.clone(),
                entries: playlist
                    .songs
                    .iter()
                    .map(|song| {
                        let local_track_id = song
                            .local_path
                            .as_ref()
                            .and_then(|path| by_path.get(path))
                            .cloned();
                        let readable_local_path = song
                            .local_path
                            .as_ref()
                            .filter(|path| by_path.contains_key(*path))
                            .cloned();
                        let stable_keys = [
                            Some(song.video_id.as_str()),
                            song.yt_video_id.as_deref(),
                            song.origin_key.as_deref(),
                            song.origin_url.as_deref(),
                        ]
                        .into_iter()
                        .flatten()
                        .filter(|key| !key.trim().is_empty())
                        .map(str::to_owned)
                        .collect();
                        let artists = if song.artists.is_empty() {
                            if song.artist.trim().is_empty() {
                                Vec::new()
                            } else {
                                vec![song.artist.clone()]
                            }
                        } else {
                            song.artists.clone()
                        };
                        LocalFindPlaylistEntryInput {
                            local_track_id,
                            readable_local_path,
                            stable_keys,
                            isrc: song.isrc.clone(),
                            title: song.title.clone(),
                            artists,
                            album: song.album.clone(),
                            duration_ms: song.duration_secs.map(u64::from).map(|secs| secs * 1_000),
                        }
                    })
                    .collect(),
            })
            .collect()
    }

    /// Reuse the already-loaded Downloads snapshot before the scanner has produced an index.
    /// This is deliberately a metadata projection only: opening Find never stats or reads files.
    fn local_find_downloaded_fallback(&self) -> Vec<crate::local::LocalTrack> {
        let mut seen = std::collections::BTreeSet::new();
        let count = self.library_ui.downloaded.len();
        self.library_ui
            .downloaded
            .iter()
            .enumerate()
            .filter_map(|(index, song)| {
                let path = song.local_path.clone()?;
                let mut track = crate::local::LocalTrack::untagged(path, 0, 0);
                if !seen.insert(track.id.clone()) {
                    return None;
                }
                // Set ordering metadata after construction so it cannot perturb the stable ID.
                track.modified_at = i64::try_from(count.saturating_sub(index)).unwrap_or(i64::MAX);
                track.title = song.title.clone();
                track.artist = downloaded_song_artists(song);
                track.album = song.album.clone();
                let album_artists = song
                    .album_artists
                    .iter()
                    .map(|artist| artist.trim())
                    .filter(|artist| !artist.is_empty())
                    .collect::<Vec<_>>();
                track.album_artist = if album_artists.is_empty() {
                    song.album_artist.clone()
                } else {
                    Some(album_artists.join(", "))
                };
                track.year = song
                    .album_release_date
                    .as_deref()
                    .and_then(|date| date.get(..4))
                    .and_then(|year| year.parse().ok());
                track.disc_no = song.disc_number;
                track.track_no = song.track_number;
                track.isrc = song.isrc.clone();
                track.duration_ms = song
                    .duration_secs
                    .or_else(|| crate::streaming::candidate::parse_duration_secs(&song.duration))
                    .map(u64::from)
                    .map(|seconds| seconds.saturating_mul(1_000));
                track.linked_video_id = song.youtube_id().map(str::to_owned);
                track.origin_key = song.origin_key.clone();
                track.origin_url = song.origin_url.clone();
                track.import_session_id = song.import_session_id.clone();
                track.import_source_order = song.import_source_order.filter(|order| *order > 0);
                Some(track)
            })
            .collect()
    }

    pub(in crate::app) fn apply_local_find_corpus(
        &mut self,
        generation: u64,
        corpus: Arc<crate::local::find::LocalFindCorpus>,
    ) -> Vec<Cmd> {
        if generation != self.local_mode.find.corpus_generation {
            return Vec::new();
        }
        self.local_mode.find.building_revision = None;
        if corpus.revision() != self.local_find_revision() {
            return self.ensure_local_find_corpus();
        }
        let retained_drill = self.local_mode.find.drill.as_ref().and_then(|drill| {
            let selected = drill.track_ids.get(self.local_mode.find.selected).cloned();
            corpus
                .mix_for_hit(&drill.source)
                .filter(|ids| !ids.is_empty())
                .map(|track_ids| {
                    (
                        drill.title.clone(),
                        drill.source.clone(),
                        track_ids,
                        selected,
                    )
                })
        });
        let corpus_revision = corpus.revision();
        self.local_mode.find.corpus = Some(corpus);
        let preserve_drill = if let Some((title, source, track_ids, selected)) = retained_drill {
            self.local_mode.find.selected = selected
                .and_then(|selected| track_ids.iter().position(|id| *id == selected))
                .unwrap_or_else(|| {
                    self.local_mode
                        .find
                        .selected
                        .min(track_ids.len().saturating_sub(1))
                });
            self.local_mode.find.drill = Some(LocalFindDrill {
                title,
                source,
                track_ids,
                corpus_revision,
            });
            true
        } else {
            self.local_mode.find.drill = None;
            false
        };
        self.local_mode.find.searching = false;
        self.dirty = true;
        let reset_selection = self.local_mode.find.reset_selection_on_result;
        let commands = self.submit_local_find_query_inner(preserve_drill);
        self.local_mode.find.reset_selection_on_result = reset_selection
            && self.local_mode.find.parse_error.is_none()
            && self.local_mode.find.searching;
        commands
    }

    pub(in crate::app) fn apply_local_find_results(
        &mut self,
        request_id: u64,
        generation: u64,
        snapshot: crate::local::find::LocalFindSnapshot,
    ) -> Vec<Cmd> {
        if request_id != self.local_mode.find.request_id
            || generation != self.local_mode.find.corpus_generation
            || snapshot.generation != request_id
            || snapshot.corpus_revision != self.local_find_revision()
        {
            return Vec::new();
        }
        let preserve_drill = self
            .local_mode
            .find
            .drill
            .as_ref()
            .is_some_and(|drill| drill.corpus_revision == snapshot.corpus_revision);
        let previous_selected = self.local_mode.find.selected;
        let previous_id = if preserve_drill {
            None
        } else {
            self.local_find_stable_id_at(previous_selected)
                .map(|(id, _)| id)
        };
        self.local_mode.find.searching = false;
        self.local_mode.find.parse_error = None;
        if !preserve_drill {
            self.local_mode.find.selected = if self.local_mode.find.reset_selection_on_result {
                0
            } else {
                previous_id
                    .and_then(|id| snapshot.hits().position(|hit| hit.id == id))
                    .unwrap_or_else(|| previous_selected.min(snapshot.total_hits.saturating_sub(1)))
            };
        }
        self.local_mode.find.reset_selection_on_result = false;
        self.local_mode.find.snapshot = Some(snapshot);
        if !preserve_drill {
            self.local_mode.find.drill = None;
            self.bridges.local_find_scroll.reset();
        }
        self.dirty = true;
        Vec::new()
    }

    pub(in crate::app) fn submit_local_find_query(&mut self) -> Vec<Cmd> {
        self.submit_local_find_query_inner(false)
    }

    fn submit_local_find_query_inner(&mut self, preserve_drill: bool) -> Vec<Cmd> {
        let had_in_flight = self.local_mode.find.searching;
        // Query identity advances for every edit, including blank and invalid text. Otherwise a
        // result spawned for the previous valid query could arrive later and erase the parse
        // error or replace the deliberately retained last-valid snapshot.
        self.local_mode.find.request_id = self.local_mode.find.request_id.wrapping_add(1).max(1);
        self.local_mode.find.reset_selection_on_result = false;
        let request_id = self.local_mode.find.request_id;
        let query = match LocalFindQuery::parse(&self.local_mode.find.query) {
            Ok(query) => query,
            Err(error) => {
                self.local_mode.find.parse_error = Some(error.localized_message());
                self.local_mode.find.searching = false;
                self.dirty = true;
                return if had_in_flight {
                    vec![Cmd::Local(LocalCmd::CancelFindEvaluations)]
                } else {
                    Vec::new()
                };
            }
        };
        self.local_mode.find.parse_error = None;
        if !preserve_drill {
            self.local_mode.find.drill = None;
        }
        if query.is_blank() {
            self.local_mode.find.snapshot = None;
            self.local_mode.find.searching = false;
            self.local_mode.find.selected = 0;
            self.dirty = true;
            let mut commands = self.ensure_local_find_corpus();
            if had_in_flight {
                commands.push(Cmd::Local(LocalCmd::CancelFindEvaluations));
            }
            return commands;
        }

        let revision = self.local_find_revision();
        let Some(corpus) = self
            .local_mode
            .find
            .corpus
            .as_ref()
            .filter(|corpus| corpus.revision() == revision)
            .cloned()
        else {
            self.local_mode.find.searching = true;
            return self.ensure_local_find_corpus();
        };
        let generation = self.local_mode.find.corpus_generation;
        self.local_mode.find.searching = true;
        self.dirty = true;
        vec![Cmd::Local(LocalCmd::EvaluateFind {
            request_id,
            generation,
            corpus,
            query,
            scope: self.local_mode.find.scope,
            sort: self.local_mode.find.sort,
        })]
    }

    /// Explicit Search-bar submission differs from live typing: it commits the current query to
    /// the result pane and asks the accepted snapshot to start at its first row.
    pub(in crate::app) fn commit_local_find_query(&mut self) -> Vec<Cmd> {
        let commands = self.submit_local_find_query();
        if self.local_mode.find.parse_error.is_none() {
            self.local_mode.find.focus = LocalFindFocus::Results;
            self.local_mode.find.selected = 0;
            self.local_mode.find.select_all = false;
            self.local_mode.find.reset_selection_on_result = self.local_mode.find.searching;
            self.dirty = true;
        }
        commands
    }

    pub(crate) fn local_find_rows_len(&self) -> usize {
        if self.local_mode.find.query.trim().is_empty() && self.local_mode.find.drill.is_none() {
            return if self.local_mode.index.index.is_empty() {
                self.local_find_recovery_actions(false).len()
            } else {
                7
            };
        }
        if self.local_mode.find.drill.is_none()
            && !self.local_mode.find.searching
            && self
                .local_mode
                .find
                .snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.total_hits == 0)
        {
            return self.local_find_recovery_actions(true).len();
        }
        self.local_mode.find.drill.as_ref().map_or_else(
            || {
                self.local_mode
                    .find
                    .snapshot
                    .as_ref()
                    .map_or(0, |snapshot| snapshot.total_hits)
            },
            |drill| drill.track_ids.len(),
        )
    }

    pub(crate) fn local_find_hit_at(&self, index: usize) -> Option<&LocalFindHit> {
        self.local_mode.find.snapshot.as_ref()?.hit_at(index)
    }

    /// Stable row identity plus an optional drill owner. The owner prevents a delayed menu from
    /// redirecting a shared track ID after the user moves between two collection drills.
    pub(crate) fn local_find_stable_id_at(
        &self,
        index: usize,
    ) -> Option<(LocalFindHitId, Option<LocalFindHitId>)> {
        if let Some(drill) = &self.local_mode.find.drill {
            let id = drill.track_ids.get(index)?.clone();
            return Some((LocalFindHitId::Track(id), Some(drill.source.clone())));
        }
        self.local_find_hit_at(index)
            .map(|hit| (hit.id.clone(), None))
    }

    pub(crate) fn local_find_has_scan_errors(&self) -> bool {
        !self.local_mode.index.load_errors.is_empty() || !self.local_mode.index.errors.is_empty()
    }

    pub(crate) fn local_find_recovery_actions(&self, include_clear: bool) -> Vec<usize> {
        let mut actions = Vec::with_capacity(4);
        if include_clear {
            actions.push(LOCAL_FIND_CLEAR_QUERY);
        }
        actions.extend([LOCAL_FIND_RESCAN, LOCAL_FIND_ADD_FOLDER]);
        if !include_clear || self.local_find_has_scan_errors() {
            actions.push(LOCAL_FIND_SCAN_ERRORS);
        }
        actions
    }

    pub(crate) fn local_find_drill_track_at(
        &self,
        index: usize,
    ) -> Option<&crate::local::LocalTrack> {
        let drill = self.local_mode.find.drill.as_ref()?;
        let corpus = self.local_mode.find.corpus.as_ref()?;
        if drill.corpus_revision != corpus.revision() {
            return None;
        }
        corpus.local_track(drill.track_ids.get(index)?)
    }

    pub(crate) fn local_find_action_generation(&self) -> u64 {
        self.local_mode
            .find
            .snapshot
            .as_ref()
            .map_or(self.local_mode.find.corpus_generation, |snapshot| {
                snapshot.generation
            })
    }

    pub(in crate::app) fn local_find_select(&mut self, index: usize, generation: u64) {
        if generation != self.local_find_action_generation() || index >= self.local_find_rows_len()
        {
            return;
        }
        self.local_mode.find.selected = index;
        self.local_mode.find.select_all = false;
        self.dirty = true;
    }

    pub(in crate::app) fn local_find_activate_index(
        &mut self,
        index: usize,
        generation: u64,
    ) -> Vec<Cmd> {
        self.local_find_select(index, generation);
        if self.local_mode.find.selected != index
            || generation != self.local_find_action_generation()
        {
            return Vec::new();
        }
        self.local_find_activate_selected()
    }

    pub(in crate::app) fn local_find_enqueue_index(
        &mut self,
        index: usize,
        generation: u64,
    ) -> Vec<Cmd> {
        self.local_find_select(index, generation);
        if self.local_mode.find.selected != index
            || generation != self.local_find_action_generation()
        {
            return Vec::new();
        }
        self.local_find_selected_bulk(LocalFindBulkAction::Enqueue)
    }

    pub(in crate::app) fn local_find_play_index(
        &mut self,
        index: usize,
        generation: u64,
    ) -> Vec<Cmd> {
        self.local_find_select(index, generation);
        if self.local_mode.find.selected != index
            || generation != self.local_find_action_generation()
        {
            return Vec::new();
        }
        self.local_find_selected_bulk(LocalFindBulkAction::Play)
    }

    fn local_find_activate_selected(&mut self) -> Vec<Cmd> {
        self.local_mode.find.selected = self
            .local_mode
            .find
            .selected
            .min(self.local_find_rows_len().saturating_sub(1));
        if self.local_mode.find.query.trim().is_empty() && self.local_mode.find.drill.is_none() {
            let action = if self.local_mode.index.index.is_empty() {
                self.local_find_recovery_actions(false)
                    .get(self.local_mode.find.selected)
                    .copied()
            } else {
                Some(self.local_mode.find.selected)
            };
            return action.map_or_else(Vec::new, |action| {
                self.activate_local_find_launchpad(action)
            });
        }
        if self.local_mode.find.drill.is_none()
            && self
                .local_mode
                .find
                .snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.total_hits == 0)
        {
            return self
                .local_find_recovery_actions(true)
                .get(self.local_mode.find.selected)
                .copied()
                .map_or_else(Vec::new, |action| {
                    self.activate_local_find_launchpad(action)
                });
        }
        if self.local_mode.find.drill.is_some() {
            if !self.local_find_corpus_is_current() {
                return self.ensure_local_find_corpus();
            }
            return self.local_find_selected_bulk(LocalFindBulkAction::Play);
        }
        if !self.local_find_corpus_is_current() {
            return self.ensure_local_find_corpus();
        }
        if !self.local_find_visible_revision_is_current() {
            return self.note_local_find_refreshing();
        }
        let Some(hit) = self
            .local_find_hit_at(self.local_mode.find.selected)
            .cloned()
        else {
            return Vec::new();
        };
        if !hit.is_playable() && !matches!(&hit.id, LocalFindHitId::Command(_)) {
            self.status.kind = StatusKind::Error;
            self.status.text = t!(
                "This result has no playable local tracks",
                "이 결과에는 재생 가능한 로컬 곡이 없습니다"
            )
            .to_owned();
            self.dirty = true;
            return Vec::new();
        }
        match hit.id {
            LocalFindHitId::Command(command) => self.activate_local_find_command(command),
            LocalFindHitId::Track(_) => self.local_find_selected_bulk(LocalFindBulkAction::Play),
            source => {
                let Some(corpus) = self.local_mode.find.corpus.as_ref() else {
                    return Vec::new();
                };
                let Some(track_ids) = corpus.mix_for_hit(&source) else {
                    return Vec::new();
                };
                self.local_mode.find.drill = Some(LocalFindDrill {
                    title: hit.label,
                    source,
                    track_ids,
                    corpus_revision: corpus.revision(),
                });
                self.local_mode.find.selected = 0;
                self.bridges.local_find_scroll.reset();
                self.dirty = true;
                Vec::new()
            }
        }
    }

    fn activate_local_find_command(&mut self, command: LocalFindCommand) -> Vec<Cmd> {
        match command {
            LocalFindCommand::Rescan => self.request_local_scan(false),
            LocalFindCommand::Rebuild => {
                self.local_mode.find.pending_rebuild_confirm = true;
                self.dirty = true;
                Vec::new()
            }
            LocalFindCommand::Queue => {
                self.mode = Mode::Player;
                self.queue_popup.open = true;
                self.dirty = true;
                Vec::new()
            }
            LocalFindCommand::ScanErrors => {
                self.mode = Mode::Library;
                self.switch_local_section(LocalSection::ScanErrors);
                Vec::new()
            }
            LocalFindCommand::Tracks => self.set_local_find_scope(LocalFindScope::Tracks),
            LocalFindCommand::Albums => self.set_local_find_scope(LocalFindScope::Albums),
            LocalFindCommand::Artists => self.set_local_find_scope(LocalFindScope::Artists),
            LocalFindCommand::Genres => self.set_local_find_scope(LocalFindScope::Genres),
            LocalFindCommand::Folders => self.set_local_find_scope(LocalFindScope::Folders),
            LocalFindCommand::Playlists => self.set_local_find_scope(LocalFindScope::Playlists),
        }
    }

    fn set_local_find_scope(&mut self, scope: LocalFindScope) -> Vec<Cmd> {
        self.local_mode.find.scope = scope;
        // A scope command is an action, not a persistent query. Clear it after activation so
        // Enter cannot repeatedly execute the same command and the requested scope opens on its
        // launchpad immediately.
        self.local_mode.find.query.clear();
        self.local_mode.find.input_cursor = TextCursor::default();
        self.local_mode.find.select_all = false;
        self.local_mode.find.focus = LocalFindFocus::Input;
        self.local_mode.find.selected = 0;
        self.bridges.local_find_scroll.reset();
        self.submit_local_find_query()
    }

    fn local_find_selected_ids(&self) -> Vec<crate::local::LocalTrackId> {
        if self.local_mode.find.query.trim().is_empty() {
            return Vec::new();
        }
        if let Some(drill) = &self.local_mode.find.drill {
            if drill.corpus_revision != self.local_find_revision() {
                return Vec::new();
            }
            return drill
                .track_ids
                .get(self.local_mode.find.selected)
                .cloned()
                .into_iter()
                .collect();
        }
        let Some(hit) = self.local_find_hit_at(self.local_mode.find.selected) else {
            return Vec::new();
        };
        self.local_mode
            .find
            .corpus
            .as_ref()
            .and_then(|corpus| corpus.mix_for_hit(&hit.id))
            .unwrap_or_default()
    }

    fn local_find_all_ids(&self) -> Vec<crate::local::LocalTrackId> {
        if self.local_mode.find.query.trim().is_empty() {
            return Vec::new();
        }
        if let Some(drill) = &self.local_mode.find.drill {
            if drill.corpus_revision != self.local_find_revision() {
                return Vec::new();
            }
            return drill.track_ids.clone();
        }
        let Some(snapshot) = self.local_mode.find.snapshot.as_ref() else {
            return Vec::new();
        };
        self.local_mode
            .find
            .corpus
            .as_ref()
            .and_then(|corpus| corpus.mix_for_snapshot(snapshot))
            .unwrap_or_default()
    }

    fn local_find_selected_bulk(&mut self, action: LocalFindBulkAction) -> Vec<Cmd> {
        if !self.local_find_corpus_is_current() {
            return self.ensure_local_find_corpus();
        }
        if !self.local_find_visible_revision_is_current() {
            return self.note_local_find_refreshing();
        }
        let ids = self.local_find_selected_ids();
        self.prepare_local_find_bulk(action, ids)
    }

    fn local_find_all_bulk(&mut self, action: LocalFindBulkAction) -> Vec<Cmd> {
        if !self.local_find_corpus_is_current() {
            return self.ensure_local_find_corpus();
        }
        if !self.local_find_visible_revision_is_current() {
            return self.note_local_find_refreshing();
        }
        let ids = self.local_find_all_ids();
        self.prepare_local_find_bulk(action, ids)
    }

    fn prepare_local_find_bulk(
        &mut self,
        action: LocalFindBulkAction,
        ids: Vec<crate::local::LocalTrackId>,
    ) -> Vec<Cmd> {
        if ids.is_empty() {
            self.status.kind = StatusKind::Error;
            self.status.text =
                t!("No playable local tracks", "재생 가능한 로컬 곡이 없습니다").to_owned();
            self.dirty = true;
            return Vec::new();
        }
        let accepted_count = match action {
            LocalFindBulkAction::Play | LocalFindBulkAction::Enqueue => {
                self.queue.remaining_capacity()
            }
            LocalFindBulkAction::ShufflePlay => Queue::max_len(),
        };
        if accepted_count == 0 {
            self.status.kind = StatusKind::Error;
            self.status.text = t!("Queue is full", "큐가 가득 찼어요").to_owned();
            self.dirty = true;
            return Vec::new();
        }
        if ids.len() > accepted_count {
            self.local_mode.find.pending_bulk_confirm = Some(LocalFindBulkConfirm {
                action,
                track_ids: ids,
                accepted_count,
                result_generation: self.local_find_action_generation(),
                corpus_revision: self.local_find_revision(),
                queue_revision: self.queue.rev(),
                capacity_recalculated: false,
            });
            self.dirty = true;
            return Vec::new();
        }
        self.apply_local_find_bulk(action, ids)
    }

    pub(in crate::app) fn confirm_local_find_bulk(&mut self) -> Vec<Cmd> {
        let Some(mut confirm) = self.local_mode.find.pending_bulk_confirm.take() else {
            return Vec::new();
        };
        if confirm.result_generation != self.local_find_action_generation()
            || confirm.corpus_revision != self.local_find_revision()
            || !self.local_find_corpus_is_current()
            || !self.local_find_visible_revision_is_current()
        {
            self.status.kind = StatusKind::Info;
            self.status.text = t!(
                "Local Find changed — review the refreshed results",
                "로컬 찾기 결과가 바뀌었습니다 — 새 결과를 확인하세요"
            )
            .to_owned();
            self.dirty = true;
            return self.ensure_local_find_corpus();
        }
        if confirm.queue_revision != self.queue.rev() {
            let capacity = match confirm.action {
                LocalFindBulkAction::Play | LocalFindBulkAction::Enqueue => {
                    self.queue.remaining_capacity()
                }
                LocalFindBulkAction::ShufflePlay => Queue::max_len(),
            };
            if capacity == 0 {
                self.status.kind = StatusKind::Error;
                self.status.text = t!("Queue is full", "큐가 가득 찼어요").to_owned();
                self.dirty = true;
                return Vec::new();
            }
            confirm.accepted_count = confirm.track_ids.len().min(capacity);
            confirm.queue_revision = self.queue.rev();
            confirm.capacity_recalculated = true;
            self.local_mode.find.pending_bulk_confirm = Some(confirm);
            self.status.kind = StatusKind::Info;
            self.status.text = t!(
                "Queue changed — review the updated capacity",
                "큐가 바뀌었습니다 — 갱신된 용량을 확인하세요"
            )
            .to_owned();
            self.dirty = true;
            return Vec::new();
        }
        let ids = confirm
            .track_ids
            .into_iter()
            .take(confirm.accepted_count)
            .collect();
        self.apply_local_find_bulk(confirm.action, ids)
    }

    fn apply_local_find_bulk(
        &mut self,
        action: LocalFindBulkAction,
        ids: Vec<crate::local::LocalTrackId>,
    ) -> Vec<Cmd> {
        let Some(corpus) = self.local_mode.find.corpus.as_ref() else {
            return Vec::new();
        };
        let mut songs: Vec<_> = ids
            .into_iter()
            .filter_map(|id| {
                corpus
                    .local_track(&id)
                    .map(crate::local::LocalTrack::to_song)
            })
            .collect();
        match action {
            LocalFindBulkAction::Play => self.play_now_many(songs),
            LocalFindBulkAction::Enqueue => self.enqueue_many(songs),
            LocalFindBulkAction::ShufflePlay => {
                if songs.is_empty() {
                    return Vec::new();
                }
                fastrand::Rng::new().shuffle(&mut songs);
                let shuffle_changed = !self.queue.shuffle;
                self.replace_queue_and_load(
                    songs,
                    0,
                    Some(true),
                    QueueReplacementOptions {
                        player_mode: true,
                        romanize_all: true,
                        persist_playback_modes: shuffle_changed,
                        ..QueueReplacementOptions::default()
                    },
                )
            }
        }
    }

    fn move_local_find_cursor(&mut self, up: bool, step: usize) {
        let len = self.local_find_rows_len();
        if len == 0 {
            self.local_mode.find.selected = 0;
        } else if up {
            self.local_mode.find.selected = self.local_mode.find.selected.saturating_sub(step);
        } else {
            self.local_mode.find.selected = self
                .local_mode
                .find
                .selected
                .saturating_add(step)
                .min(len - 1);
        }
        self.local_mode.find.select_all = false;
        self.dirty = true;
    }

    fn cycle_local_find_scope(&mut self, forward: bool) -> Vec<Cmd> {
        let current = FIND_SCOPES
            .iter()
            .position(|scope| *scope == self.local_mode.find.scope)
            .unwrap_or_default();
        self.local_mode.find.scope = FIND_SCOPES[cycle_index(current, FIND_SCOPES.len(), forward)];
        self.local_mode.find.selected = 0;
        self.bridges.local_find_scroll.reset();
        self.submit_local_find_query()
    }

    fn local_find_back(&mut self) -> Vec<Cmd> {
        if let Some(drill) = self.local_mode.find.drill.take() {
            self.local_mode.find.selected = self
                .local_mode
                .find
                .snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.hits().position(|hit| hit.id == drill.source))
                .unwrap_or_default();
            self.bridges.local_find_scroll.reset();
            self.dirty = true;
            return Vec::new();
        }
        if !self.local_mode.find.query.trim().is_empty() {
            self.local_mode.find.query.clear();
            self.local_mode.find.input_cursor = TextCursor::default();
            self.local_mode.find.focus = LocalFindFocus::Input;
            self.local_mode.find.selected = 0;
            self.bridges.local_find_scroll.reset();
            return self.submit_local_find_query();
        }
        self.close_local_find()
    }

    pub(in crate::app) fn open_local_find_refine(&mut self) -> Vec<Cmd> {
        self.local_mode.find.refine_popup.open = true;
        self.local_mode.find.refine_popup.row = 0;
        self.local_mode.find.refine_popup.draft_scope = self.local_mode.find.scope;
        self.local_mode.find.refine_popup.draft_sort = self.local_mode.find.sort;
        self.local_mode.find.refine_popup.help_scroll.reset();
        self.dirty = true;
        Vec::new()
    }

    pub(in crate::app) fn local_find_refine_click(&mut self, row: usize) -> Vec<Cmd> {
        if !self.local_mode.find.refine_popup.open || row > 3 {
            return Vec::new();
        }
        self.local_mode.find.refine_popup.row = row;
        self.dirty = true;
        if row < 2 {
            self.on_key_local_find_refine(KeyEvent::new(KeyCode::Right, KeyModifiers::empty()))
        } else {
            self.on_key_local_find_refine(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()))
        }
    }

    pub(in crate::app) fn activate_local_find_launchpad(&mut self, index: usize) -> Vec<Cmd> {
        let query = match index {
            0 => "sort:recent",
            1 => "is:local-only",
            2 => "is:lossless",
            3 => "missing:artist",
            4 => "missing:album",
            5 => "missing:cover",
            LOCAL_FIND_RESCAN => return self.request_local_scan(false),
            LOCAL_FIND_CLEAR_QUERY => {
                self.local_mode.find.query.clear();
                self.local_mode.find.input_cursor = TextCursor::default();
                self.local_mode.find.focus = LocalFindFocus::Input;
                self.local_mode.find.selected = 0;
                self.bridges.local_find_scroll.reset();
                return self.submit_local_find_query();
            }
            LOCAL_FIND_ADD_FOLDER => {
                self.open_settings();
                if let Some(settings) = self.settings.as_mut() {
                    settings.tab = SettingsTab::General;
                    settings.row = settings
                        .fields()
                        .iter()
                        .position(|field| *field == Field::LocalMusicRoot)
                        .unwrap_or_default();
                }
                self.dirty = true;
                return Vec::new();
            }
            LOCAL_FIND_SCAN_ERRORS => {
                self.mode = Mode::Library;
                self.switch_local_section(LocalSection::ScanErrors);
                return Vec::new();
            }
            _ => return Vec::new(),
        };
        // Smart launchpad rows describe track collections. Preserve the user's chosen scope for
        // ordinary blank-query browsing, but normalize these actions to their track semantics.
        self.local_mode.find.scope = LocalFindScope::Tracks;
        self.local_mode.find.query = query.to_owned();
        self.local_mode.find.input_cursor = TextCursor::at_end(&self.local_mode.find.query);
        self.local_mode.find.select_all = false;
        self.local_mode.find.focus = LocalFindFocus::Results;
        self.submit_local_find_query()
    }

    pub(in crate::app) fn on_key_local_find_refine(&mut self, key: KeyEvent) -> Vec<Cmd> {
        match key.code {
            KeyCode::Esc => {
                self.local_mode.find.refine_popup.open = false;
                self.local_mode.find.refine_popup.rect.set(None);
                self.dirty = true;
                Vec::new()
            }
            KeyCode::Up => {
                self.local_mode.find.refine_popup.row =
                    self.local_mode.find.refine_popup.row.saturating_sub(1);
                self.dirty = true;
                Vec::new()
            }
            KeyCode::Down => {
                self.local_mode.find.refine_popup.row =
                    (self.local_mode.find.refine_popup.row + 1).min(3);
                self.dirty = true;
                Vec::new()
            }
            KeyCode::PageUp | KeyCode::PageDown => {
                let scroll = &self.local_mode.find.refine_popup.help_scroll;
                let page = scroll.viewport().saturating_sub(1).max(1);
                scroll.wheel(key.code == KeyCode::PageUp, page, usize::MAX);
                self.dirty = true;
                Vec::new()
            }
            KeyCode::Left | KeyCode::Right => {
                let forward = key.code == KeyCode::Right;
                match self.local_mode.find.refine_popup.row {
                    0 => {
                        let current = FIND_SCOPES
                            .iter()
                            .position(|scope| {
                                *scope == self.local_mode.find.refine_popup.draft_scope
                            })
                            .unwrap_or_default();
                        let next = cycle_index(current, FIND_SCOPES.len(), forward);
                        self.local_mode.find.refine_popup.draft_scope = FIND_SCOPES[next];
                    }
                    1 => {
                        let current = LocalFindSort::ALL
                            .iter()
                            .position(|sort| *sort == self.local_mode.find.refine_popup.draft_sort)
                            .unwrap_or_default();
                        let next = cycle_index(current, LocalFindSort::ALL.len(), forward);
                        self.local_mode.find.refine_popup.draft_sort = LocalFindSort::ALL[next];
                    }
                    _ => {}
                }
                self.dirty = true;
                Vec::new()
            }
            KeyCode::Enter if self.local_mode.find.refine_popup.row == 3 => {
                self.local_mode.find.refine_popup.open = false;
                self.dirty = true;
                Vec::new()
            }
            KeyCode::Enter => {
                self.local_mode.find.scope = self.local_mode.find.refine_popup.draft_scope;
                self.local_mode.find.sort = self.local_mode.find.refine_popup.draft_sort;
                self.local_mode.find.refine_popup.open = false;
                self.local_mode.find.selected = 0;
                self.submit_local_find_query()
            }
            _ => Vec::new(),
        }
    }

    pub(in crate::app) fn on_key_local_find(&mut self, key: KeyEvent) -> Vec<Cmd> {
        if self.local_mode.find.pending_rebuild_confirm {
            return match key.code {
                KeyCode::Enter => self.confirm_local_find_rebuild(),
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.local_mode.find.pending_rebuild_confirm = false;
                    self.dirty = true;
                    Vec::new()
                }
                _ => Vec::new(),
            };
        }
        if self.local_mode.find.refine_popup.open {
            return self.on_key_local_find_refine(key);
        }
        if self.local_mode.find.focus == LocalFindFocus::Input {
            let chord = Chord::from(key);
            if let Some(action) = self.keymap.text_edit_action(chord) {
                if std::mem::take(&mut self.local_mode.find.select_all) {
                    match action {
                        Action::DeleteChar | Action::DeleteWord => {
                            self.local_mode.find.query.clear();
                            self.local_mode.find.input_cursor = TextCursor::default();
                            return self.submit_local_find_query();
                        }
                        Action::MoveCursorLeft | Action::MoveCursorWordLeft => {
                            self.local_mode.find.input_cursor.move_to_start();
                        }
                        Action::MoveCursorRight | Action::MoveCursorWordRight => {
                            self.local_mode
                                .find
                                .input_cursor
                                .move_to_end(&self.local_mode.find.query);
                        }
                        _ => {}
                    }
                    self.dirty = true;
                    return Vec::new();
                }
                match apply_text_edit_action(
                    action,
                    &mut self.local_mode.find.input_cursor,
                    &mut self.local_mode.find.query,
                ) {
                    Some(TextEditResult::BufferChanged(true)) => {
                        return self.submit_local_find_query();
                    }
                    Some(TextEditResult::CursorMoved(true)) => self.dirty = true,
                    Some(
                        TextEditResult::BufferChanged(false) | TextEditResult::CursorMoved(false),
                    )
                    | None => {}
                }
                return Vec::new();
            }
            if matches!(
                self.keymap.action(KeyContext::SearchInput, chord),
                Some(Action::SelectAll)
            ) {
                self.local_mode.find.select_all = !self.local_mode.find.query.is_empty();
                self.dirty = true;
                return Vec::new();
            }
            if std::mem::take(&mut self.local_mode.find.select_all) {
                self.dirty = true;
                if chord.is_typeable()
                    && let KeyCode::Char(ch) = key.code
                {
                    self.local_mode.find.query.clear();
                    self.local_mode.find.input_cursor = TextCursor::default();
                    if let Err(reason) = try_insert_query_char(
                        &mut self.local_mode.find.query,
                        &mut self.local_mode.find.input_cursor,
                        ch,
                        MAX_SEARCH_QUERY_BYTES,
                    ) {
                        self.set_query_reject_status(reason);
                        return Vec::new();
                    }
                    return self.submit_local_find_query();
                }
            }
            match key.code {
                KeyCode::Enter => return self.commit_local_find_query(),
                KeyCode::Tab => return self.cycle_local_find_scope(true),
                KeyCode::Down if self.local_find_rows_len() > 0 => {
                    self.local_mode.find.focus = LocalFindFocus::Results;
                    self.dirty = true;
                    return Vec::new();
                }
                KeyCode::BackTab if self.local_find_rows_len() > 0 => {
                    self.local_mode.find.focus = LocalFindFocus::Results;
                    self.dirty = true;
                    return Vec::new();
                }
                KeyCode::Esc => return self.local_find_back(),
                KeyCode::Char(ch) if chord.is_typeable() => {
                    if let Err(reason) = try_insert_query_char(
                        &mut self.local_mode.find.query,
                        &mut self.local_mode.find.input_cursor,
                        ch,
                        MAX_SEARCH_QUERY_BYTES,
                    ) {
                        self.set_query_reject_status(reason);
                        return Vec::new();
                    }
                    return self.submit_local_find_query();
                }
                _ => return Vec::new(),
            }
        }

        if key.code == KeyCode::Enter {
            return self.local_find_activate_selected();
        }
        if self.local_mode.find.query.trim().is_empty() && self.local_mode.find.drill.is_none() {
            match key.code {
                KeyCode::Char(ch @ '1'..='6') if !self.local_mode.index.index.is_empty() => {
                    return self.activate_local_find_launchpad((ch as u8 - b'1') as usize);
                }
                KeyCode::Char('r') => {
                    return self.activate_local_find_launchpad(LOCAL_FIND_RESCAN);
                }
                KeyCode::Char('+') => {
                    return self.activate_local_find_launchpad(LOCAL_FIND_ADD_FOLDER);
                }
                KeyCode::Char('!') => {
                    return self.activate_local_find_launchpad(LOCAL_FIND_SCAN_ERRORS);
                }
                _ => {}
            }
        } else if self.local_mode.find.drill.is_none()
            && self
                .local_mode
                .find
                .snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.total_hits == 0)
        {
            match key.code {
                KeyCode::Char('c') => {
                    return self.activate_local_find_launchpad(LOCAL_FIND_CLEAR_QUERY);
                }
                KeyCode::Char('r') => {
                    return self.activate_local_find_launchpad(LOCAL_FIND_RESCAN);
                }
                KeyCode::Char('+') => {
                    return self.activate_local_find_launchpad(LOCAL_FIND_ADD_FOLDER);
                }
                KeyCode::Char('!') if self.local_find_has_scan_errors() => {
                    return self.activate_local_find_launchpad(LOCAL_FIND_SCAN_ERRORS);
                }
                _ => {}
            }
        }
        match key.code {
            KeyCode::Char('P') => self.local_find_selected_bulk(LocalFindBulkAction::Play),
            KeyCode::Char('a') | KeyCode::Char('\\') => {
                self.local_find_selected_bulk(LocalFindBulkAction::Enqueue)
            }
            KeyCode::Char('A') => self.local_find_all_bulk(LocalFindBulkAction::Enqueue),
            KeyCode::Char('s') => self.local_find_all_bulk(LocalFindBulkAction::ShufflePlay),
            KeyCode::Char('/') => self.open_local_find_refine(),
            KeyCode::Esc => self.local_find_back(),
            KeyCode::Char('q') => self.close_local_find(),
            KeyCode::Tab | KeyCode::BackTab => {
                self.local_mode.find.focus = LocalFindFocus::Input;
                self.dirty = true;
                Vec::new()
            }
            KeyCode::Up => {
                if self.local_mode.find.selected == 0 {
                    self.local_mode.find.focus = LocalFindFocus::Input;
                    self.dirty = true;
                } else {
                    let step = self.nav_repeat_step(Action::MoveUp);
                    self.move_local_find_cursor(true, step);
                }
                Vec::new()
            }
            KeyCode::Down => {
                let step = self.nav_repeat_step(Action::MoveDown);
                self.move_local_find_cursor(false, step);
                Vec::new()
            }
            KeyCode::PageUp => {
                self.move_local_find_cursor(true, self.page_step());
                Vec::new()
            }
            KeyCode::PageDown => {
                self.move_local_find_cursor(false, self.page_step());
                Vec::new()
            }
            KeyCode::Home => {
                self.local_mode.find.selected = 0;
                self.dirty = true;
                Vec::new()
            }
            KeyCode::End => {
                self.local_mode.find.selected = self.local_find_rows_len().saturating_sub(1);
                self.dirty = true;
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    pub(in crate::app) fn confirm_local_find_rebuild(&mut self) -> Vec<Cmd> {
        if !std::mem::take(&mut self.local_mode.find.pending_rebuild_confirm) {
            return Vec::new();
        }
        self.dirty = true;
        self.request_local_scan(true)
    }
}

fn downloaded_song_artists(song: &Song) -> Vec<String> {
    let artists: Vec<_> = song
        .artists
        .iter()
        .filter(|artist| !artist.trim().is_empty())
        .cloned()
        .collect();
    if !artists.is_empty() {
        return artists;
    }
    let artist = song.artist.trim();
    if artist.is_empty() || artist.eq_ignore_ascii_case("Local file") {
        return Vec::new();
    }
    artist
        .split([';', '/', ','])
        .map(str::trim)
        .filter(|artist| !artist.is_empty())
        .map(str::to_owned)
        .collect()
}

fn cycle_index(current: usize, len: usize, forward: bool) -> usize {
    if forward {
        (current + 1) % len
    } else if current == 0 {
        len - 1
    } else {
        current - 1
    }
}
