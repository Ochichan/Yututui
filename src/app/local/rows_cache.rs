//! Cached Local Deck row projections and sparse formatted values.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, hash_map::DefaultHasher};
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;

use super::super::local_format::*;
use super::super::local_import_helpers::import_session_row_status_label;
use super::super::*;
use super::import_fingerprint::{local_import_files_fingerprint, stable_import_cache_key};

const MISSING_LOCAL_TRACK_INDEX: usize = usize::MAX;
const LOCAL_VISIBLE_VALUE_CACHE_CAP: usize = 512;
const LOCAL_SELECTED_VALUE_CACHE_CAP: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LocalIndexRowsFingerprint {
    updated_at: i64,
    len: usize,
    representative: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LocalDownloadRowsFingerprint {
    revision: u64,
    len: usize,
    representative: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LocalErrorRowsFingerprint {
    load_len: usize,
    scan_len: usize,
    representative: u64,
}

struct LocalAlbumDisplay {
    title: String,
    album_artist: String,
    year: Option<i32>,
    track_count: usize,
    duration_ms: Option<u64>,
    embedded_cover_count: usize,
}

struct LocalArtistDisplay {
    name: String,
    album_count: usize,
    track_count: usize,
}

struct LocalImportRowDisplay {
    status: &'static str,
    title: String,
    artists: Vec<String>,
}

enum LocalRowsKind {
    Empty,
    Tracks(Box<[usize]>),
    DownloadSeeds,
    Albums(Box<[Option<LocalAlbumDisplay>]>),
    Artists(Box<[Option<LocalArtistDisplay>]>),
    Counted(Box<[usize]>),
    ImportSessions(Box<[usize]>),
    ImportRows(Box<[Option<LocalImportRowDisplay>]>),
    ScanErrors,
}

/// Immutable row ids + compact data needed to format them without rebuilding the LocalIndex's
/// album/artist/group projections. String/detail caches are sparse: only rows that actually become
/// visible (or selected) allocate formatted output.
pub(in crate::app) struct LocalRowsData {
    pub(in crate::app) rows: Arc<[crate::local::LocalRowId]>,
    kind: LocalRowsKind,
    row_text: RefCell<HashMap<usize, Arc<str>>>,
    row_details: RefCell<HashMap<usize, Arc<[String]>>>,
    import_deletable: RefCell<HashMap<usize, bool>>,
    import_action_hint: RefCell<HashMap<usize, Option<Arc<str>>>>,
}

/// The Local Deck only renders one section/query/drill path at a time, so retaining one entry keeps
/// memory bounded while making all repeated reads in that state O(1).
pub(in crate::app) struct LocalRowsCache {
    revision: u64,
    section: LocalSection,
    drill: Vec<LocalDrill>,
    query: String,
    lang: crate::i18n::Language,
    index: LocalIndexRowsFingerprint,
    downloads: LocalDownloadRowsFingerprint,
    errors: LocalErrorRowsFingerprint,
    config: u64,
    romanize: u64,
    import_files: Option<u64>,
    /// Set only after a full terminal draw successfully presented this exact cached projection.
    rendered: Cell<bool>,
    data: Rc<LocalRowsData>,
    total_len: usize,
}

impl App {
    pub(crate) fn local_rows_snapshot(&self) -> LocalRowsSnapshot {
        let revision = self.local_mode.rows_revision.get();
        let section = self.local_mode.ui.section;
        let drill = self.local_mode.ui.drill.as_slice();
        let query = self.local_mode.ui.filter_query.as_str();
        let import_before = local_rows_read_import_files(section, drill.last()).then(|| {
            local_import_files_fingerprint(
                &mut self.local_mode.import_files_fingerprint_cache.borrow_mut(),
            )
        });
        let lang = crate::i18n::current();
        let index = local_index_rows_fingerprint(&self.local_mode.index.index);
        let downloads = local_download_rows_fingerprint(
            self.library_ui.downloaded_rev,
            &self.library_ui.downloaded,
        );
        let errors = local_error_rows_fingerprint(
            &self.local_mode.index.load_errors,
            &self.local_mode.index.errors,
        );
        let config = local_config_rows_fingerprint(&self.config);
        let romanize = self.romanization.cache.rev();

        if let Some(cache) = self.local_mode.rows_cache.borrow().as_ref()
            && cache.revision == revision
            && cache.section == section
            && cache.drill.as_slice() == drill
            && cache.query == query
            && cache.lang == lang
            && cache.index == index
            && cache.downloads == downloads
            && cache.errors == errors
            && cache.config == config
            && cache.romanize == romanize
            && import_before.is_none_or(|fingerprint| fingerprint.reliable())
            && cache.import_files == import_before.map(|fingerprint| fingerprint.digest())
        {
            return LocalRowsSnapshot {
                data: Rc::clone(&cache.data),
                total_len: cache.total_len,
            };
        }

        let snapshot = self.build_local_rows_snapshot(query);
        let import_after = import_before.map(|_| {
            local_import_files_fingerprint(
                &mut self.local_mode.import_files_fingerprint_cache.borrow_mut(),
            )
        });
        let Some(import_files) = stable_import_cache_key(import_before, import_after) else {
            self.local_mode.rows_cache.borrow_mut().take();
            return snapshot;
        };
        *self.local_mode.rows_cache.borrow_mut() = Some(LocalRowsCache {
            revision,
            section,
            drill: drill.to_vec(),
            query: query.to_owned(),
            lang,
            index,
            downloads,
            errors,
            config,
            romanize,
            import_files,
            rendered: Cell::new(false),
            data: Rc::clone(&snapshot.data),
            total_len: snapshot.total_len,
        });
        snapshot
    }

    /// Record that the current import-backed Local rows cache reached the terminal in a successful
    /// full draw. This intentionally does not touch the filesystem again: the fast-path freshness
    /// probe compares the recorded cache digest with a new reliable fingerprint immediately before
    /// every scrub, catching writes performed later in the same render (such as action hints).
    pub(crate) fn mark_local_rows_rendered(&self) {
        if !self.local_import_rows_view_active() {
            return;
        }
        let cache = self.local_mode.rows_cache.borrow();
        let Some(cache) = cache.as_ref() else {
            return;
        };
        if cache.import_files.is_some()
            && self.local_rows_cache_matches_current_state(cache, cache.import_files)
        {
            cache.rendered.set(true);
        }
    }

    /// Whether terminal-only IME scrubbing may reuse the currently displayed Local projection.
    /// Import artifacts are external mutable state, so this fails closed unless a reliable current
    /// fingerprint matches a row cache that a successful full draw has already presented. The
    /// runner calls this only after its reducer-turn/dirty/clear gates prove owner-loop state could
    /// not have changed, avoiding repeated in-memory projection fingerprints on the 80 ms clock.
    pub(crate) fn ime_scrub_local_projection_fresh(&self) -> bool {
        if !self.local_import_rows_view_active() {
            return true;
        }
        let fingerprint = local_import_files_fingerprint(
            &mut self.local_mode.import_files_fingerprint_cache.borrow_mut(),
        );
        if !fingerprint.reliable() {
            return false;
        }
        self.local_mode
            .rows_cache
            .borrow()
            .as_ref()
            .is_some_and(|cache| {
                cache.rendered.get() && cache.import_files == Some(fingerprint.digest())
            })
    }

    fn local_import_rows_view_active(&self) -> bool {
        self.local_dedicated_mode
            && self.mode == Mode::Library
            && local_rows_read_import_files(
                self.local_mode.ui.section,
                self.local_mode.ui.drill.last(),
            )
    }

    fn local_rows_cache_matches_current_state(
        &self,
        cache: &LocalRowsCache,
        import_files: Option<u64>,
    ) -> bool {
        cache.revision == self.local_mode.rows_revision.get()
            && cache.section == self.local_mode.ui.section
            && cache.drill.as_slice() == self.local_mode.ui.drill.as_slice()
            && cache.query == self.local_mode.ui.filter_query
            && cache.lang == crate::i18n::current()
            && cache.index == local_index_rows_fingerprint(&self.local_mode.index.index)
            && cache.downloads
                == local_download_rows_fingerprint(
                    self.library_ui.downloaded_rev,
                    &self.library_ui.downloaded,
                )
            && cache.errors
                == local_error_rows_fingerprint(
                    &self.local_mode.index.load_errors,
                    &self.local_mode.index.errors,
                )
            && cache.config == local_config_rows_fingerprint(&self.config)
            && cache.romanize == self.romanization.cache.rev()
            && cache.import_files == import_files
    }

    fn build_local_rows_snapshot(&self, query: &str) -> LocalRowsSnapshot {
        let rows = self.local_rows_for_query(query);
        let total_len = if query.is_empty() {
            rows.len()
        } else {
            self.local_rows_for_query("").len()
        };
        let kind = self.local_rows_kind(&rows);
        LocalRowsSnapshot {
            data: Rc::new(LocalRowsData {
                rows: Arc::from(rows),
                kind,
                row_text: RefCell::new(HashMap::new()),
                row_details: RefCell::new(HashMap::new()),
                import_deletable: RefCell::new(HashMap::new()),
                import_action_hint: RefCell::new(HashMap::new()),
            }),
            total_len,
        }
    }

    pub(crate) fn local_visible_rows(&self) -> Arc<[crate::local::LocalRowId]> {
        Arc::clone(&self.local_rows_snapshot().data.rows)
    }

    #[cfg(test)]
    pub(crate) fn local_uncached_rows_snapshot(&self) -> LocalRowsSnapshot {
        let query = self.local_mode.ui.filter_query.as_str();
        let rows = self.local_rows_for_query(query);
        let total_len = if query.is_empty() {
            rows.len()
        } else {
            self.local_rows_for_query("").len()
        };
        LocalRowsSnapshot {
            data: Rc::new(LocalRowsData {
                rows: Arc::from(rows),
                // Empty deliberately routes formatting/details through the exact legacy lookup
                // paths; equivalence tests compare that surface with the cached projections.
                kind: LocalRowsKind::Empty,
                row_text: RefCell::new(HashMap::new()),
                row_details: RefCell::new(HashMap::new()),
                import_deletable: RefCell::new(HashMap::new()),
                import_action_hint: RefCell::new(HashMap::new()),
            }),
            total_len,
        }
    }

    fn local_rows_kind(&self, rows: &[crate::local::LocalRowId]) -> LocalRowsKind {
        let Some(first) = rows.first() else {
            return LocalRowsKind::Empty;
        };
        match first {
            crate::local::LocalRowId::Track(_) => {
                let positions: HashMap<_, _> = self
                    .local_mode
                    .index
                    .index
                    .tracks()
                    .iter()
                    .enumerate()
                    .map(|(index, track)| (&track.id, index))
                    .collect();
                let indices = rows
                    .iter()
                    .map(|row| match row {
                        crate::local::LocalRowId::Track(id) => positions
                            .get(id)
                            .copied()
                            .unwrap_or(MISSING_LOCAL_TRACK_INDEX),
                        _ => MISSING_LOCAL_TRACK_INDEX,
                    })
                    .collect::<Vec<_>>()
                    .into_boxed_slice();
                LocalRowsKind::Tracks(indices)
            }
            crate::local::LocalRowId::DownloadSeed(_) => LocalRowsKind::DownloadSeeds,
            crate::local::LocalRowId::Album(_) => {
                let covered: HashSet<_> = self
                    .local_mode
                    .index
                    .index
                    .tracks()
                    .iter()
                    .filter(|track| track.embedded_art_key.is_some())
                    .map(|track| track.id.as_str())
                    .collect();
                let mut albums: HashMap<_, _> = self
                    .local_mode
                    .index
                    .index
                    .albums()
                    .into_iter()
                    .map(|album| {
                        let embedded_cover_count = album
                            .track_ids
                            .iter()
                            .filter(|id| covered.contains(id.as_str()))
                            .count();
                        (
                            album.id,
                            LocalAlbumDisplay {
                                title: album.title,
                                album_artist: album.album_artist,
                                year: album.year,
                                track_count: album.track_count,
                                duration_ms: album.duration_ms,
                                embedded_cover_count,
                            },
                        )
                    })
                    .collect();
                let albums = rows
                    .iter()
                    .map(|row| match row {
                        crate::local::LocalRowId::Album(id) => albums.remove(id),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .into_boxed_slice();
                LocalRowsKind::Albums(albums)
            }
            crate::local::LocalRowId::Artist(_) => {
                let mut artists: HashMap<_, _> = self
                    .local_mode
                    .index
                    .index
                    .artists()
                    .into_iter()
                    .map(|artist| {
                        (
                            artist.id,
                            LocalArtistDisplay {
                                name: artist.name,
                                album_count: artist.album_ids.len(),
                                track_count: artist.track_ids.len(),
                            },
                        )
                    })
                    .collect();
                let artists = rows
                    .iter()
                    .map(|row| match row {
                        crate::local::LocalRowId::Artist(id) => artists.remove(id),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .into_boxed_slice();
                LocalRowsKind::Artists(artists)
            }
            crate::local::LocalRowId::Genre(_) => {
                let mut counts = HashMap::<String, usize>::new();
                for track in self.local_mode.index.index.tracks() {
                    let mut seen = HashSet::new();
                    for genre in &track.genre {
                        let genre = genre.trim();
                        if !genre.is_empty() {
                            let key = crate::local::model::normalize_key(genre);
                            if seen.insert(key.clone()) {
                                *counts.entry(key).or_default() += 1;
                            }
                        }
                    }
                }
                LocalRowsKind::Counted(
                    rows.iter()
                        .map(|row| match row {
                            crate::local::LocalRowId::Genre(genre) => counts
                                .get(&crate::local::model::normalize_key(genre))
                                .copied()
                                .unwrap_or_default(),
                            _ => 0,
                        })
                        .collect::<Vec<_>>()
                        .into_boxed_slice(),
                )
            }
            crate::local::LocalRowId::Folder(_) => {
                let mut counts = HashMap::<&Path, usize>::new();
                for track in self.local_mode.index.index.tracks() {
                    if let Some(parent) = track.path.parent() {
                        *counts.entry(parent).or_default() += 1;
                    }
                }
                LocalRowsKind::Counted(
                    rows.iter()
                        .map(|row| match row {
                            crate::local::LocalRowId::Folder(folder) => {
                                counts.get(folder.as_path()).copied().unwrap_or_default()
                            }
                            _ => 0,
                        })
                        .collect::<Vec<_>>()
                        .into_boxed_slice(),
                )
            }
            crate::local::LocalRowId::Smart(_) => {
                let download_dir = self.config.effective_download_dir();
                LocalRowsKind::Counted(
                    rows.iter()
                        .map(|row| match row {
                            crate::local::LocalRowId::Smart(smart) => {
                                self.local_smart_track_count(*smart, &download_dir)
                            }
                            _ => 0,
                        })
                        .collect::<Vec<_>>()
                        .into_boxed_slice(),
                )
            }
            crate::local::LocalRowId::ImportSession(_) => {
                let mut counts = HashMap::<&str, usize>::new();
                for track in self.local_mode.index.index.tracks() {
                    if let Some(session_id) = track.import_session_id.as_deref() {
                        *counts.entry(session_id).or_default() += 1;
                    }
                }
                LocalRowsKind::ImportSessions(
                    rows.iter()
                        .map(|row| match row {
                            crate::local::LocalRowId::ImportSession(session_id) => {
                                counts.get(session_id.as_str()).copied().unwrap_or_default()
                            }
                            _ => 0,
                        })
                        .collect::<Vec<_>>()
                        .into_boxed_slice(),
                )
            }
            crate::local::LocalRowId::ImportSessionRow { .. } => {
                LocalRowsKind::ImportRows(self.local_import_row_displays(rows))
            }
            crate::local::LocalRowId::ScanError(_) => LocalRowsKind::ScanErrors,
        }
    }

    fn local_import_row_displays(
        &self,
        rows: &[crate::local::LocalRowId],
    ) -> Box<[Option<LocalImportRowDisplay>]> {
        let mut needed = HashMap::<String, HashSet<u32>>::new();
        for row in rows {
            if let crate::local::LocalRowId::ImportSessionRow {
                session_id,
                source_order,
            } = row
            {
                needed
                    .entry(session_id.clone())
                    .or_default()
                    .insert(*source_order);
            }
        }
        let mut displays = HashMap::<String, HashMap<u32, LocalImportRowDisplay>>::new();
        for (session_id, orders) in needed {
            let Ok(session) = crate::transfer::session::ImportSession::load(&session_id) else {
                continue;
            };
            let by_order = session
                .rows
                .into_iter()
                .filter(|row| orders.contains(&row.source_order))
                .map(|row| {
                    let source_order = row.source_order;
                    let status = import_session_row_status_label(&row);
                    (
                        source_order,
                        LocalImportRowDisplay {
                            status,
                            title: row.title,
                            artists: row.artists,
                        },
                    )
                })
                .collect();
            displays.insert(session_id, by_order);
        }
        rows.iter()
            .map(|row| match row {
                crate::local::LocalRowId::ImportSessionRow {
                    session_id,
                    source_order,
                } => displays
                    .get_mut(session_id)
                    .and_then(|rows| rows.remove(source_order)),
                _ => None,
            })
            .collect::<Vec<_>>()
            .into_boxed_slice()
    }

    pub(crate) fn local_row_text_at(&self, snapshot: &LocalRowsSnapshot, index: usize) -> Arc<str> {
        if let Some(text) = snapshot.data.row_text.borrow().get(&index).cloned() {
            return text;
        }
        let Some(row) = snapshot.rows().get(index) else {
            return Arc::from("");
        };
        let text = self.local_row_text_from_snapshot(snapshot, index, row);
        let text: Arc<str> = Arc::from(text);
        insert_bounded_local_value(
            &snapshot.data.row_text,
            index,
            Arc::clone(&text),
            LOCAL_VISIBLE_VALUE_CACHE_CAP,
        );
        text
    }

    fn local_row_text_from_snapshot(
        &self,
        snapshot: &LocalRowsSnapshot,
        index: usize,
        row: &crate::local::LocalRowId,
    ) -> String {
        match (&snapshot.data.kind, row) {
            (LocalRowsKind::Tracks(indices), crate::local::LocalRowId::Track(_)) => indices
                .get(index)
                .and_then(|track_index| self.local_mode.index.index.tracks().get(*track_index))
                .map(|track| local_track_text(self, track))
                .unwrap_or_else(|| self.local_row_text_uncached(row)),
            (
                LocalRowsKind::DownloadSeeds,
                crate::local::LocalRowId::DownloadSeed(download_index),
            ) => self
                .library_ui
                .downloaded
                .get(*download_index)
                .map(|song| local_song_text(self, song))
                .unwrap_or_else(|| t!("Missing track", "없는 곡", "不明な曲").to_owned()),
            (LocalRowsKind::Albums(albums), crate::local::LocalRowId::Album(_)) => albums
                .get(index)
                .and_then(Option::as_ref)
                .map(|album| {
                    let duration = album.duration_ms.map(format_local_duration_ms);
                    let year = album.year.map(|year| year.to_string()).unwrap_or_default();
                    let suffix = match (year.is_empty(), duration) {
                        (false, Some(duration)) => format!("  {year} - {duration}"),
                        (false, None) => format!("  {year}"),
                        (true, Some(duration)) => format!("  {duration}"),
                        (true, None) => String::new(),
                    };
                    format!(
                        "{} - {}  ({} {}){}",
                        album.title,
                        album.album_artist,
                        album.track_count,
                        t!("tracks", "곡", "曲"),
                        suffix
                    )
                })
                .unwrap_or_else(|| t!("Missing album", "없는 앨범", "不明なアルバム").to_owned()),
            (LocalRowsKind::Artists(artists), crate::local::LocalRowId::Artist(_)) => artists
                .get(index)
                .and_then(Option::as_ref)
                .map(|artist| {
                    format!(
                        "{}  ({} {}, {} {})",
                        artist.name,
                        artist.album_count,
                        t!("albums", "앨범", "アルバム"),
                        artist.track_count,
                        t!("tracks", "곡", "曲")
                    )
                })
                .unwrap_or_else(|| {
                    t!("Missing artist", "없는 아티스트", "不明なアーティスト").to_owned()
                }),
            (LocalRowsKind::Counted(counts), crate::local::LocalRowId::Genre(genre)) => {
                let count = counts.get(index).copied().unwrap_or_default();
                format!("{genre}  ({count} {})", t!("tracks", "곡", "曲"))
            }
            (LocalRowsKind::Counted(counts), crate::local::LocalRowId::Folder(folder)) => {
                let count = counts.get(index).copied().unwrap_or_default();
                format!(
                    "{}  ({count} {})",
                    folder.display(),
                    t!("tracks", "곡", "曲")
                )
            }
            (LocalRowsKind::Counted(counts), crate::local::LocalRowId::Smart(smart)) => {
                let count = counts.get(index).copied().unwrap_or_default();
                format!("{}  ({count} {})", smart.label(), t!("tracks", "곡", "曲"))
            }
            (
                LocalRowsKind::ImportSessions(track_counts),
                crate::local::LocalRowId::ImportSession(session_id),
            ) => local_import_session_text(
                session_id,
                track_counts.get(index).copied().unwrap_or_default(),
            ),
            (
                LocalRowsKind::ImportRows(displays),
                crate::local::LocalRowId::ImportSessionRow { source_order, .. },
            ) => displays
                .get(index)
                .and_then(Option::as_ref)
                .map(|display| {
                    let artist = if display.artists.is_empty() {
                        t!("Local file", "로컬 파일", "ローカルファイル").to_owned()
                    } else {
                        display.artists.join(", ")
                    };
                    let title = display.title.trim();
                    if title.is_empty() {
                        format!("#{source_order} {} - {artist}", display.status)
                    } else {
                        format!("#{source_order} {} {title} - {artist}", display.status)
                    }
                })
                .unwrap_or_else(|| self.local_row_text_uncached(row)),
            (LocalRowsKind::ScanErrors, crate::local::LocalRowId::ScanError(issue_index)) => self
                .local_scan_issue(*issue_index)
                .map(|error| format!("{} - {}", error.path.display(), error.message))
                .unwrap_or_else(|| {
                    t!(
                        "Missing scan error",
                        "없는 스캔 오류",
                        "不明なスキャンエラー"
                    )
                    .to_owned()
                }),
            _ => self.local_row_text_uncached(row),
        }
    }

    #[cfg(test)]
    pub(crate) fn local_row_text(&self, row: &crate::local::LocalRowId) -> String {
        let snapshot = self.local_rows_snapshot();
        if let Some(index) = snapshot
            .rows()
            .iter()
            .position(|candidate| candidate == row)
        {
            return self.local_row_text_at(&snapshot, index).to_string();
        }
        self.local_row_text_uncached(row)
    }

    fn local_row_text_uncached(&self, row: &crate::local::LocalRowId) -> String {
        match row {
            crate::local::LocalRowId::Track(id) => self
                .local_track_by_id(id)
                .map(|track| local_track_text(self, track))
                .unwrap_or_else(|| t!("Missing track", "없는 곡", "不明な曲").to_owned()),
            crate::local::LocalRowId::DownloadSeed(index) => self
                .library_ui
                .downloaded
                .get(*index)
                .map(|song| local_song_text(self, song))
                .unwrap_or_else(|| t!("Missing track", "없는 곡", "不明な曲").to_owned()),
            crate::local::LocalRowId::Album(id) => self
                .local_album_by_id(id)
                .map(|album| {
                    let duration = album.duration_ms.map(format_local_duration_ms);
                    let year = album.year.map(|year| year.to_string()).unwrap_or_default();
                    let suffix = match (year.is_empty(), duration) {
                        (false, Some(duration)) => format!("  {year} - {duration}"),
                        (false, None) => format!("  {year}"),
                        (true, Some(duration)) => format!("  {duration}"),
                        (true, None) => String::new(),
                    };
                    format!(
                        "{} - {}  ({} {}){}",
                        album.title,
                        album.album_artist,
                        album.track_count,
                        t!("tracks", "곡", "曲"),
                        suffix
                    )
                })
                .unwrap_or_else(|| t!("Missing album", "없는 앨범", "不明なアルバム").to_owned()),
            crate::local::LocalRowId::Artist(id) => self
                .local_artist_by_id(id)
                .map(|artist| {
                    format!(
                        "{}  ({} {}, {} {})",
                        artist.name,
                        artist.album_ids.len(),
                        t!("albums", "앨범", "アルバム"),
                        artist.track_ids.len(),
                        t!("tracks", "곡", "曲")
                    )
                })
                .unwrap_or_else(|| {
                    t!("Missing artist", "없는 아티스트", "不明なアーティスト").to_owned()
                }),
            crate::local::LocalRowId::Genre(genre) => {
                let count = self.local_tracks_for_genre(genre).len();
                format!("{genre}  ({count} {})", t!("tracks", "곡", "曲"))
            }
            crate::local::LocalRowId::Folder(folder) => {
                let count = self.local_tracks_for_folder(folder).len();
                format!(
                    "{}  ({count} {})",
                    folder.display(),
                    t!("tracks", "곡", "曲")
                )
            }
            crate::local::LocalRowId::Smart(smart) => {
                let count = self.local_tracks_for_smart(*smart).len();
                format!("{}  ({count} {})", smart.label(), t!("tracks", "곡", "曲"))
            }
            crate::local::LocalRowId::ImportSession(session_id) => local_import_session_text(
                session_id,
                self.local_tracks_for_import_session(session_id).len(),
            ),
            crate::local::LocalRowId::ImportSessionRow {
                session_id,
                source_order,
            } => self.local_import_session_row_text(session_id, *source_order),
            crate::local::LocalRowId::ScanError(index) => self
                .local_scan_issue(*index)
                .map(|error| format!("{} - {}", error.path.display(), error.message))
                .unwrap_or_else(|| {
                    t!(
                        "Missing scan error",
                        "없는 스캔 오류",
                        "不明なスキャンエラー"
                    )
                    .to_owned()
                }),
        }
    }

    #[cfg(test)]
    pub(crate) fn local_details_lines(&self) -> Vec<String> {
        let snapshot = self.local_rows_snapshot();
        self.local_details_lines_for_snapshot(&snapshot)
    }

    pub(crate) fn local_details_lines_for_snapshot(
        &self,
        snapshot: &LocalRowsSnapshot,
    ) -> Vec<String> {
        let mut lines = Vec::new();
        lines.push(t!("Selected", "선택", "選択").to_owned());
        if snapshot.rows().get(self.local_mode.ui.selected).is_some() {
            lines.extend(
                self.local_row_details_at(snapshot, self.local_mode.ui.selected)
                    .iter()
                    .cloned(),
            );
        } else {
            lines.push(
                t!(
                    "No local item selected.",
                    "선택된 로컬 항목이 없습니다.",
                    "ローカル項目が選択されていません。"
                )
                .to_owned(),
            );
        }

        lines.push(String::new());
        self.push_local_queue_details(&mut lines);
        lines
    }

    pub(crate) fn local_details_summary_for_snapshot(
        &self,
        snapshot: &LocalRowsSnapshot,
    ) -> String {
        let selected = if snapshot.rows().get(self.local_mode.ui.selected).is_some() {
            self.local_row_text_at(snapshot, self.local_mode.ui.selected)
                .to_string()
        } else {
            t!("No selection", "선택 없음", "選択なし").to_owned()
        };
        let Some(current) = self.queue.current() else {
            return format!("{}: {selected}", t!("Selected", "선택", "選択"));
        };
        format!(
            "{}: {selected}  |  {}: {}",
            t!("Selected", "선택", "選択"),
            t!("Now", "재생 중", "再生中"),
            local_song_text(self, current)
        )
    }

    fn local_row_details_at(&self, snapshot: &LocalRowsSnapshot, index: usize) -> Arc<[String]> {
        if let Some(lines) = snapshot.data.row_details.borrow().get(&index).cloned() {
            return lines;
        }
        let Some(row) = snapshot.rows().get(index) else {
            return Arc::from(Vec::<String>::new());
        };
        let mut lines = Vec::new();
        match (&snapshot.data.kind, row) {
            (LocalRowsKind::Tracks(indices), crate::local::LocalRowId::Track(_)) => {
                if let Some(track) = indices
                    .get(index)
                    .and_then(|track_index| self.local_mode.index.index.tracks().get(*track_index))
                {
                    self.push_local_track_details(&mut lines, track);
                } else {
                    self.push_local_row_details(&mut lines, row);
                }
            }
            (
                LocalRowsKind::DownloadSeeds,
                crate::local::LocalRowId::DownloadSeed(download_index),
            ) => {
                if let Some(song) = self.library_ui.downloaded.get(*download_index) {
                    self.push_local_song_details(&mut lines, song);
                }
            }
            (LocalRowsKind::Albums(albums), crate::local::LocalRowId::Album(_)) => {
                if let Some(album) = albums.get(index).and_then(Option::as_ref) {
                    push_detail_line(&mut lines, t!("Album", "앨범", "アルバム"), &album.title);
                    push_detail_line(
                        &mut lines,
                        t!("Artist", "아티스트", "アーティスト"),
                        &album.album_artist,
                    );
                    if let Some(year) = album.year {
                        push_detail_line(&mut lines, t!("Year", "연도", "年"), year.to_string());
                    }
                    push_detail_line(
                        &mut lines,
                        t!("Tracks", "곡", "曲"),
                        format!("{} {}", album.track_count, t!("tracks", "곡", "曲")),
                    );
                    if let Some(duration) = album.duration_ms {
                        push_detail_line(
                            &mut lines,
                            t!("Duration", "길이", "再生時間"),
                            format_local_duration_ms(duration),
                        );
                    }
                    push_detail_line(
                        &mut lines,
                        t!("Cover", "커버", "カバー"),
                        format_embedded_cover_count(album.embedded_cover_count),
                    );
                }
            }
            (LocalRowsKind::Artists(artists), crate::local::LocalRowId::Artist(_)) => {
                if let Some(artist) = artists.get(index).and_then(Option::as_ref) {
                    push_detail_line(
                        &mut lines,
                        t!("Artist", "아티스트", "アーティスト"),
                        &artist.name,
                    );
                    push_detail_line(
                        &mut lines,
                        t!("Albums", "앨범", "アルバム"),
                        format!(
                            "{} {}",
                            artist.album_count,
                            t!("albums", "앨범", "アルバム")
                        ),
                    );
                    push_detail_line(
                        &mut lines,
                        t!("Tracks", "곡", "曲"),
                        format!("{} {}", artist.track_count, t!("tracks", "곡", "曲")),
                    );
                }
            }
            (LocalRowsKind::Counted(counts), crate::local::LocalRowId::Genre(genre)) => {
                push_detail_line(&mut lines, t!("Genre", "장르", "ジャンル"), genre);
                push_detail_line(
                    &mut lines,
                    t!("Tracks", "곡", "曲"),
                    format!(
                        "{} {}",
                        counts.get(index).copied().unwrap_or_default(),
                        t!("tracks", "곡", "曲")
                    ),
                );
            }
            (LocalRowsKind::Counted(counts), crate::local::LocalRowId::Folder(folder)) => {
                push_detail_line(
                    &mut lines,
                    t!("Folder", "폴더", "フォルダー"),
                    folder.display().to_string(),
                );
                push_detail_line(
                    &mut lines,
                    t!("Tracks", "곡", "曲"),
                    format!(
                        "{} {}",
                        counts.get(index).copied().unwrap_or_default(),
                        t!("tracks", "곡", "曲")
                    ),
                );
            }
            (LocalRowsKind::Counted(counts), crate::local::LocalRowId::Smart(smart)) => {
                push_detail_line(
                    &mut lines,
                    t!("Smart list", "스마트 목록", "スマートリスト"),
                    smart.label(),
                );
                push_detail_line(
                    &mut lines,
                    t!("Tracks", "곡", "曲"),
                    format!(
                        "{} {}",
                        counts.get(index).copied().unwrap_or_default(),
                        t!("tracks", "곡", "曲")
                    ),
                );
            }
            _ => self.push_local_row_details(&mut lines, row),
        }
        let lines: Arc<[String]> = Arc::from(lines);
        insert_bounded_local_value(
            &snapshot.data.row_details,
            index,
            Arc::clone(&lines),
            LOCAL_SELECTED_VALUE_CACHE_CAP,
        );
        lines
    }

    pub(crate) fn local_import_record_deletable_at(
        &self,
        snapshot: &LocalRowsSnapshot,
        index: usize,
    ) -> bool {
        if let Some(deletable) = snapshot.data.import_deletable.borrow().get(&index) {
            return *deletable;
        }
        let deletable = snapshot
            .rows()
            .get(index)
            .is_some_and(|row| self.local_import_record_deletable(row));
        insert_bounded_local_value(
            &snapshot.data.import_deletable,
            index,
            deletable,
            LOCAL_VISIBLE_VALUE_CACHE_CAP,
        );
        deletable
    }

    pub(crate) fn local_import_action_hint_for_snapshot(
        &self,
        snapshot: &LocalRowsSnapshot,
    ) -> Option<String> {
        let index = self.local_mode.ui.selected;
        snapshot.rows().get(index)?;
        if let Some(hint) = snapshot.data.import_action_hint.borrow().get(&index) {
            return hint.as_ref().map(|hint| hint.to_string());
        }
        let hint = self.local_import_action_hint();
        insert_bounded_local_value(
            &snapshot.data.import_action_hint,
            index,
            hint.as_deref().map(Arc::<str>::from),
            LOCAL_SELECTED_VALUE_CACHE_CAP,
        );
        hint
    }
}

fn local_rows_read_import_files(section: LocalSection, drill: Option<&LocalDrill>) -> bool {
    match drill {
        Some(LocalDrill::ImportSession(_)) => true,
        Some(_) => false,
        None => matches!(section, LocalSection::ImportSessions | LocalSection::Inbox),
    }
}

fn insert_bounded_local_value<T>(
    cache: &RefCell<HashMap<usize, T>>,
    index: usize,
    value: T,
    capacity: usize,
) {
    let mut cache = cache.borrow_mut();
    if cache.len() >= capacity && !cache.contains_key(&index) {
        cache.clear();
    }
    cache.insert(index, value);
}

fn local_index_rows_fingerprint(index: &crate::local::LocalIndex) -> LocalIndexRowsFingerprint {
    let tracks = index.tracks();
    let mut hasher = DefaultHasher::new();
    for sample in representative_indices(tracks.len()).into_iter().flatten() {
        sample.hash(&mut hasher);
        hash_local_track(&tracks[sample], &mut hasher);
    }
    LocalIndexRowsFingerprint {
        updated_at: index.updated_at,
        len: tracks.len(),
        representative: hasher.finish(),
    }
}

fn local_download_rows_fingerprint(revision: u64, songs: &[Song]) -> LocalDownloadRowsFingerprint {
    let mut hasher = DefaultHasher::new();
    for sample in representative_indices(songs.len()).into_iter().flatten() {
        sample.hash(&mut hasher);
        let song = &songs[sample];
        song.video_id.hash(&mut hasher);
        song.title.hash(&mut hasher);
        song.artist.hash(&mut hasher);
        song.artists.hash(&mut hasher);
        song.duration.hash(&mut hasher);
        song.album.hash(&mut hasher);
        song.album_artist.hash(&mut hasher);
        song.local_path.hash(&mut hasher);
        song.import_session_id.hash(&mut hasher);
        song.import_source_order.hash(&mut hasher);
    }
    LocalDownloadRowsFingerprint {
        revision,
        len: songs.len(),
        representative: hasher.finish(),
    }
}

fn local_error_rows_fingerprint(
    load_errors: &[crate::local::ScanError],
    scan_errors: &[crate::local::ScanError],
) -> LocalErrorRowsFingerprint {
    let mut hasher = DefaultHasher::new();
    for (tag, errors) in [(0_u8, load_errors), (1_u8, scan_errors)] {
        tag.hash(&mut hasher);
        for sample in representative_indices(errors.len()).into_iter().flatten() {
            sample.hash(&mut hasher);
            errors[sample].path.hash(&mut hasher);
            errors[sample].message.hash(&mut hasher);
        }
    }
    LocalErrorRowsFingerprint {
        load_len: load_errors.len(),
        scan_len: scan_errors.len(),
        representative: hasher.finish(),
    }
}

fn local_config_rows_fingerprint(config: &crate::config::Config) -> u64 {
    let mut hasher = DefaultHasher::new();
    config.download_dir.hash(&mut hasher);
    config.romanized_titles.hash(&mut hasher);
    config.local.include_download_dir.hash(&mut hasher);
    config.local.roots.len().hash(&mut hasher);
    for root in &config.local.roots {
        root.path.hash(&mut hasher);
        root.enabled.hash(&mut hasher);
        root.recursive.hash(&mut hasher);
    }
    hasher.finish()
}

fn representative_indices(len: usize) -> [Option<usize>; 3] {
    match len {
        0 => [None, None, None],
        1 => [Some(0), None, None],
        2 => [Some(0), Some(1), None],
        _ => [Some(0), Some(len / 2), Some(len - 1)],
    }
}

fn hash_local_track(track: &crate::local::LocalTrack, hasher: &mut impl Hasher) {
    track.id.hash(hasher);
    track.path.hash(hasher);
    track.title.hash(hasher);
    track.artist.hash(hasher);
    track.album.hash(hasher);
    track.album_artist.hash(hasher);
    track.genre.hash(hasher);
    track.year.hash(hasher);
    track.disc_no.hash(hasher);
    track.track_no.hash(hasher);
    track.isrc.hash(hasher);
    track.duration_ms.hash(hasher);
    match &track.format {
        None => 0_u8.hash(hasher),
        Some(crate::local::AudioFormat::Aac) => 1_u8.hash(hasher),
        Some(crate::local::AudioFormat::Flac) => 2_u8.hash(hasher),
        Some(crate::local::AudioFormat::M4a) => 3_u8.hash(hasher),
        Some(crate::local::AudioFormat::Mp3) => 4_u8.hash(hasher),
        Some(crate::local::AudioFormat::Ogg) => 5_u8.hash(hasher),
        Some(crate::local::AudioFormat::Opus) => 6_u8.hash(hasher),
        Some(crate::local::AudioFormat::Wav) => 7_u8.hash(hasher),
        Some(crate::local::AudioFormat::Wma) => 8_u8.hash(hasher),
        Some(crate::local::AudioFormat::Other(value)) => {
            9_u8.hash(hasher);
            value.hash(hasher);
        }
    }
    track.bitrate.hash(hasher);
    track.sample_rate.hash(hasher);
    track.file_size.hash(hasher);
    track.modified_at.hash(hasher);
    track.fingerprint.stable_hash().hash(hasher);
    track.embedded_art_key.hash(hasher);
    track.linked_video_id.hash(hasher);
    track.origin_key.hash(hasher);
    track.origin_url.hash(hasher);
    track.import_session_id.hash(hasher);
    track.import_source_order.hash(hasher);
}
