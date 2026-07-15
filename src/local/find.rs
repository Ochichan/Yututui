//! Pure, in-memory search domain for Local Deck Find.
//!
//! The corpus owns normalized metadata and stable local identities only. Building and
//! querying it never performs filesystem or network I/O, so callers may safely move both
//! operations to a background worker and reject stale snapshots by revision/generation.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use super::model::{AudioFormat, LocalAlbumId, LocalArtistId, LocalTrack, LocalTrackId};

/// Searchable result families, in their fixed display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum LocalFindScope {
    #[default]
    All,
    Tracks,
    Albums,
    Artists,
    Genres,
    Folders,
    Playlists,
}

impl LocalFindScope {
    pub const RESULT_GROUPS: [Self; 6] = [
        Self::Tracks,
        Self::Albums,
        Self::Artists,
        Self::Genres,
        Self::Folders,
        Self::Playlists,
    ];
}

/// Deterministic ordering applied to both rows and whole-result mixes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum LocalFindSort {
    #[default]
    Relevance,
    Title,
    Artist,
    Album,
    Year,
    Recent,
}

impl LocalFindSort {
    pub const ALL: [Self; 6] = [
        Self::Relevance,
        Self::Title,
        Self::Artist,
        Self::Album,
        Self::Year,
        Self::Recent,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Relevance => "relevance",
            Self::Title => "title",
            Self::Artist => "artist",
            Self::Album => "album",
            Self::Year => "year",
            Self::Recent => "recent",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|sort| sort.as_str() == value)
    }
}

mod query;
#[path = "find/search.rs"]
mod search;
use query::normalize_text;
pub use query::{
    LocalFindCommand, LocalFindCommandQuery, LocalFindField, LocalFindIs, LocalFindMissing,
    LocalFindParseError, LocalFindParseErrorKind, LocalFindQuery, LocalFindTerm,
    LocalFindYearRange,
};
/// Corpus generations supplied by the app. They are copied into every snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LocalFindCorpusRevision {
    pub index: u64,
    pub playlists: u64,
    /// Downloaded-song snapshot used only while the scanner index is empty.
    pub downloads: u64,
    /// Stable fingerprint of path-classification options such as the effective download root.
    pub options: u64,
}

/// Pure build options. Paths are compared only; they are never opened or probed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LocalFindCorpusOptions {
    pub downloaded_roots: Vec<PathBuf>,
}

/// Compact playlist input independent of the persisted playlist/app types.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LocalFindPlaylistInput {
    pub id: String,
    pub name: String,
    pub entries: Vec<LocalFindPlaylistEntryInput>,
}

/// A caller-prevalidated playlist entry.
///
/// `readable_local_path` must only be populated after the caller has established that the path
/// is an allowed readable local file. The pure resolver itself performs no filesystem I/O.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LocalFindPlaylistEntryInput {
    pub local_track_id: Option<LocalTrackId>,
    pub readable_local_path: Option<PathBuf>,
    pub stable_keys: Vec<String>,
    pub isrc: Option<String>,
    pub title: String,
    pub artists: Vec<String>,
    pub album: Option<String>,
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalFindPlaylistMatch {
    LocalPath,
    StableIdentity,
    Isrc,
    Metadata,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalFindPlaylistEntryResolution {
    Resolved {
        track_id: LocalTrackId,
        matched_by: LocalFindPlaylistMatch,
    },
    Unresolved,
    Ambiguous,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalFindPlaylistProjection {
    pub id: String,
    pub name: String,
    pub total_track_count: usize,
    pub track_ids: Vec<LocalTrackId>,
    pub resolutions: Vec<LocalFindPlaylistEntryResolution>,
}

impl LocalFindPlaylistProjection {
    pub fn locally_playable_count(&self) -> usize {
        self.track_ids.len()
    }

    pub fn unresolved_count(&self) -> usize {
        self.resolutions
            .iter()
            .filter(|resolution| matches!(resolution, LocalFindPlaylistEntryResolution::Unresolved))
            .count()
    }

    pub fn ambiguous_count(&self) -> usize {
        self.resolutions
            .iter()
            .filter(|resolution| matches!(resolution, LocalFindPlaylistEntryResolution::Ambiguous))
            .count()
    }
}

/// Resolve a single playlist without touching disk or app state.
pub fn resolve_playlist_projection(
    tracks: &[LocalTrack],
    playlist: &LocalFindPlaylistInput,
) -> LocalFindPlaylistProjection {
    PlaylistResolver::new(tracks).resolve(playlist)
}

/// Stable typed identity carried by result rows and mouse targets.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LocalFindHitId {
    Track(LocalTrackId),
    Album(LocalAlbumId),
    Artist(LocalArtistId),
    Genre(String),
    Folder(PathBuf),
    Playlist(String),
    Command(LocalFindCommand),
}

/// Why a playlist row matched the current query. Kept typed so the UI can localize it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalFindMatchReason {
    PlaylistName,
    ResolvedLocalTrack,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalFindHit {
    pub id: LocalFindHitId,
    pub label: String,
    pub secondary: String,
    pub year: Option<i32>,
    pub locally_playable_count: usize,
    pub total_track_count: usize,
    pub match_reason: Option<LocalFindMatchReason>,
}

impl LocalFindHit {
    pub fn is_playable(&self) -> bool {
        self.locally_playable_count > 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalFindGroup {
    pub scope: LocalFindScope,
    pub hits: Vec<LocalFindHit>,
}

/// Immutable result snapshot. Generation and both corpus revisions make stale actions rejectable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalFindSnapshot {
    pub generation: u64,
    pub corpus_revision: LocalFindCorpusRevision,
    pub query: LocalFindQuery,
    pub scope: LocalFindScope,
    pub sort: LocalFindSort,
    pub groups: Vec<LocalFindGroup>,
    pub total_hits: usize,
}

impl LocalFindSnapshot {
    pub fn hits(&self) -> impl Iterator<Item = &LocalFindHit> {
        self.groups.iter().flat_map(|group| group.hits.iter())
    }

    /// Return a flattened result by index without walking through preceding hits one by one.
    ///
    /// There are at most six result groups, so viewport and selection lookups stay bounded by
    /// the number of groups rather than the size of a large result set.
    pub fn hit_at(&self, mut index: usize) -> Option<&LocalFindHit> {
        for group in &self.groups {
            if index < group.hits.len() {
                return group.hits.get(index);
            }
            index -= group.hits.len();
        }
        None
    }
}

#[derive(Debug, Clone, Default)]
struct SearchFields {
    titles: Vec<String>,
    artists: Vec<String>,
    albums: Vec<String>,
    album_artists: Vec<String>,
    genres: Vec<String>,
    paths: Vec<String>,
    formats: Vec<String>,
}

impl SearchFields {
    fn values(&self, field: LocalFindField) -> impl Iterator<Item = &str> {
        let fields: &[String] = match field {
            LocalFindField::Title => &self.titles,
            LocalFindField::TrackArtist => &self.artists,
            LocalFindField::Album => &self.albums,
            LocalFindField::AlbumArtist => &self.album_artists,
            LocalFindField::Genre => &self.genres,
            LocalFindField::Path => &self.paths,
            LocalFindField::Format => &self.formats,
            LocalFindField::Any => &[],
        };
        fields.iter().map(String::as_str)
    }

    fn major_values(&self) -> impl Iterator<Item = &str> {
        self.titles
            .iter()
            .chain(self.artists.iter())
            .chain(self.albums.iter())
            .chain(self.album_artists.iter())
            .chain(self.genres.iter())
            .map(String::as_str)
    }

    fn all_values(&self) -> impl Iterator<Item = &str> {
        self.major_values()
            .chain(self.paths.iter().map(String::as_str))
            .chain(self.formats.iter().map(String::as_str))
    }
}

#[derive(Debug, Clone)]
struct TrackSearchData {
    fields: SearchFields,
    year: Option<i32>,
    modified_at: i64,
    predicates: BTreeSet<LocalFindIs>,
    missing: BTreeSet<LocalFindMissing>,
    album_sort: String,
    artist_sort: String,
    disc_no: Option<u32>,
    track_no: Option<u32>,
    title_sort: String,
}

#[derive(Debug, Clone)]
struct SearchDocument {
    id: LocalFindHitId,
    label: String,
    label_sort: String,
    secondary: String,
    secondary_sort: String,
    fields: SearchFields,
    track_ids: Vec<LocalTrackId>,
    year: Option<i32>,
    recent: i64,
    artist_sort: String,
    album_sort: String,
    locally_playable_count: usize,
    total_track_count: usize,
}

impl SearchDocument {
    fn hit(&self) -> LocalFindHit {
        LocalFindHit {
            id: self.id.clone(),
            label: self.label.clone(),
            secondary: self.secondary.clone(),
            year: self.year,
            locally_playable_count: self.locally_playable_count,
            total_track_count: self.total_track_count,
            match_reason: None,
        }
    }
}

/// Immutable normalized local catalog. Build it off the UI thread, then share it by `Arc`.
#[derive(Debug, Clone)]
pub struct LocalFindCorpus {
    revision: LocalFindCorpusRevision,
    /// Canonical local payloads retained for late action resolution. The reducer keeps only
    /// stable IDs in result snapshots; playback/details convert those IDs back to local files
    /// here, including the in-memory downloaded-song fallback used before the scanner index is
    /// available.
    local_tracks: BTreeMap<LocalTrackId, LocalTrack>,
    tracks: Vec<SearchDocument>,
    albums: Vec<SearchDocument>,
    artists: Vec<SearchDocument>,
    genres: Vec<SearchDocument>,
    folders: Vec<SearchDocument>,
    playlists: Vec<SearchDocument>,
    track_data: BTreeMap<LocalTrackId, TrackSearchData>,
    entity_tracks: BTreeMap<LocalFindHitId, Vec<LocalTrackId>>,
}

impl LocalFindCorpus {
    pub fn from_tracks(tracks: &[LocalTrack]) -> Self {
        Self::build(
            tracks,
            &[],
            LocalFindCorpusRevision::default(),
            &LocalFindCorpusOptions::default(),
        )
    }

    pub fn build(
        tracks: &[LocalTrack],
        playlists: &[LocalFindPlaylistInput],
        revision: LocalFindCorpusRevision,
        options: &LocalFindCorpusOptions,
    ) -> Self {
        let track_data: BTreeMap<_, _> = tracks
            .iter()
            .map(|track| (track.id.clone(), track_search_data(track, options)))
            .collect();
        let mut ordered_track_ids: Vec<_> = track_data.keys().cloned().collect();
        ordered_track_ids.sort_by(|left, right| compare_canonical_tracks(left, right, &track_data));
        let order: BTreeMap<_, _> = ordered_track_ids
            .iter()
            .cloned()
            .enumerate()
            .map(|(rank, id)| (id, rank))
            .collect();

        let tracks_by_id: BTreeMap<_, _> = tracks
            .iter()
            .map(|track| (track.id.clone(), track))
            .collect();
        let mut track_docs = Vec::with_capacity(ordered_track_ids.len());
        for id in &ordered_track_ids {
            let Some(track) = tracks_by_id.get(id) else {
                continue;
            };
            let data = &track_data[id];
            track_docs.push(SearchDocument {
                id: LocalFindHitId::Track(id.clone()),
                label: track.display_title(),
                label_sort: data.title_sort.clone(),
                secondary: track.display_artist(),
                secondary_sort: data.artist_sort.clone(),
                fields: SearchFields::default(),
                track_ids: vec![id.clone()],
                year: track.year,
                recent: track.modified_at,
                artist_sort: data.artist_sort.clone(),
                album_sort: data.album_sort.clone(),
                locally_playable_count: 1,
                total_track_count: 1,
            });
        }

        let albums = build_albums(tracks, &track_data, &order);
        let artists = build_artists(tracks, &track_data, &order);
        let genres = build_genres(tracks, &track_data, &order);
        let folders = build_folders(tracks, &track_data, &order);
        let resolver = PlaylistResolver::new(tracks);
        let playlist_docs: Vec<SearchDocument> = playlists
            .iter()
            .map(|playlist| playlist_document(resolver.resolve(playlist), &track_data))
            .collect();

        let mut entity_tracks = BTreeMap::new();
        for document in track_docs
            .iter()
            .chain(albums.iter())
            .chain(artists.iter())
            .chain(genres.iter())
            .chain(folders.iter())
            .chain(playlist_docs.iter())
        {
            entity_tracks.insert(document.id.clone(), document.track_ids.clone());
        }

        Self {
            revision,
            local_tracks: tracks
                .iter()
                .map(|track| (track.id.clone(), track.clone()))
                .collect(),
            tracks: track_docs,
            albums,
            artists,
            genres,
            folders,
            playlists: playlist_docs,
            track_data,
            entity_tracks,
        }
    }

    pub fn revision(&self) -> LocalFindCorpusRevision {
        self.revision
    }

    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty()
    }

    pub fn track_count(&self) -> usize {
        self.tracks.len()
    }

    /// Resolve a stable result identity to the exact local payload captured by this corpus.
    pub fn local_track(&self, id: &LocalTrackId) -> Option<&LocalTrack> {
        self.local_tracks.get(id)
    }

    /// Expand one selected row to its canonical, duplicate-free local track mix.
    pub fn mix_for_hit(&self, hit: &LocalFindHitId) -> Option<Vec<LocalTrackId>> {
        let ids = self.entity_tracks.get(hit)?;
        Some(dedupe_track_ids(ids.iter().cloned()))
    }

    /// Expand the whole visible result according to the scope contract.
    ///
    /// All/Tracks uses direct track hits only; collection scopes expand their matched rows.
    /// A revision mismatch returns `None`, allowing the reducer to reject a stale action.
    pub fn mix_for_snapshot(&self, snapshot: &LocalFindSnapshot) -> Option<Vec<LocalTrackId>> {
        if snapshot.corpus_revision != self.revision {
            return None;
        }
        let mut expanded = Vec::new();
        let direct_tracks_only =
            matches!(snapshot.scope, LocalFindScope::All | LocalFindScope::Tracks);
        for group in snapshot
            .groups
            .iter()
            .filter(|group| !direct_tracks_only || group.scope == LocalFindScope::Tracks)
        {
            for hit in &group.hits {
                let ids = self.entity_tracks.get(&hit.id)?;
                expanded.extend(ids.iter().cloned());
            }
        }
        Some(dedupe_track_ids(expanded))
    }
}

/// Keep the first occurrence of every stable local track identity.
pub fn dedupe_track_ids(track_ids: impl IntoIterator<Item = LocalTrackId>) -> Vec<LocalTrackId> {
    let mut seen = BTreeSet::new();
    track_ids
        .into_iter()
        .filter(|id| seen.insert(id.clone()))
        .collect()
}

fn track_search_data(track: &LocalTrack, options: &LocalFindCorpusOptions) -> TrackSearchData {
    let title = normalize_text(&track.display_title());
    let artists = normalized_nonempty(track.artist.iter().map(String::as_str));
    let album = normalize_optional(track.album.as_deref());
    let album_artist = normalize_optional(track.album_artist.as_deref());
    let genres = normalized_nonempty(track.genre.iter().map(String::as_str));
    let path = normalize_text(&track.path.to_string_lossy());
    let filename = track
        .path
        .file_name()
        .and_then(|name| name.to_str())
        .map(normalize_text)
        .unwrap_or_default();
    let format = track.format.as_ref().map(audio_format_key);

    let downloaded = track.linked_video_id.is_some()
        || track.origin_key.is_some()
        || track.origin_url.is_some()
        || track.import_session_id.is_some()
        || options
            .downloaded_roots
            .iter()
            .any(|root| track.path.starts_with(root));
    let mut predicates = BTreeSet::new();
    if matches!(track.format, Some(AudioFormat::Flac | AudioFormat::Wav)) {
        predicates.insert(LocalFindIs::Lossless);
    }
    predicates.insert(if downloaded {
        LocalFindIs::Downloaded
    } else {
        LocalFindIs::LocalOnly
    });

    let mut missing = BTreeSet::new();
    if track.artist.iter().all(|artist| artist.trim().is_empty()) {
        missing.insert(LocalFindMissing::Artist);
    }
    if track
        .album
        .as_deref()
        .is_none_or(|album| album.trim().is_empty())
    {
        missing.insert(LocalFindMissing::Album);
    }
    if track
        .embedded_art_key
        .as_deref()
        .is_none_or(|cover| cover.trim().is_empty())
    {
        missing.insert(LocalFindMissing::Cover);
    }

    TrackSearchData {
        fields: SearchFields {
            titles: vec![title.clone()],
            artists: artists.clone(),
            albums: album.iter().cloned().collect(),
            album_artists: album_artist.iter().cloned().collect(),
            genres,
            paths: vec![filename, path],
            formats: format.into_iter().collect(),
        },
        year: track.year,
        modified_at: track.modified_at,
        predicates,
        missing,
        album_sort: album.unwrap_or_default(),
        artist_sort: album_artist
            .or_else(|| artists.first().cloned())
            .unwrap_or_default(),
        disc_no: track.disc_no,
        track_no: track.track_no,
        title_sort: title,
    }
}

fn audio_format_key(format: &AudioFormat) -> String {
    match format {
        AudioFormat::Aac => "aac".to_owned(),
        AudioFormat::Flac => "flac".to_owned(),
        AudioFormat::M4a => "m4a".to_owned(),
        AudioFormat::Mp3 => "mp3".to_owned(),
        AudioFormat::Ogg => "ogg".to_owned(),
        AudioFormat::Opus => "opus".to_owned(),
        AudioFormat::Wav => "wav".to_owned(),
        AudioFormat::Wma => "wma".to_owned(),
        AudioFormat::Other(other) => normalize_text(other.trim()),
    }
}

fn normalize_optional(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(normalize_text)
}

fn normalized_nonempty<'a>(values: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut values = values
        .into_iter()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(normalize_text)
        .collect::<Vec<_>>();
    values.sort();
    values.dedup();
    values
}

fn compare_canonical_tracks(
    left: &LocalTrackId,
    right: &LocalTrackId,
    tracks: &BTreeMap<LocalTrackId, TrackSearchData>,
) -> Ordering {
    let left_track = &tracks[left];
    let right_track = &tracks[right];
    left_track
        .artist_sort
        .cmp(&right_track.artist_sort)
        .then_with(|| left_track.album_sort.cmp(&right_track.album_sort))
        .then_with(|| option_asc(left_track.disc_no, right_track.disc_no))
        .then_with(|| option_asc(left_track.track_no, right_track.track_no))
        .then_with(|| left_track.title_sort.cmp(&right_track.title_sort))
        .then_with(|| left.cmp(right))
}

fn option_asc<T: Ord>(left: Option<T>, right: Option<T>) -> Ordering {
    match (left, right) {
        (Some(left), Some(right)) => left.cmp(&right),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn ordered_ids(
    ids: impl IntoIterator<Item = LocalTrackId>,
    order: &BTreeMap<LocalTrackId, usize>,
) -> Vec<LocalTrackId> {
    let mut ids = dedupe_track_ids(ids);
    ids.sort_by_key(|id| order.get(id).copied().unwrap_or(usize::MAX));
    ids
}

#[derive(Default)]
struct CollectionBuilder {
    display: String,
    secondary: String,
    track_ids: Vec<LocalTrackId>,
}

fn choose_display(current: &mut String, candidate: &str) {
    let candidate = candidate.trim();
    if candidate.is_empty() {
        return;
    }
    if current.is_empty()
        || normalize_text(candidate)
            .cmp(&normalize_text(current))
            .then_with(|| candidate.cmp(current))
            == Ordering::Less
    {
        *current = candidate.to_owned();
    }
}

fn document_stats(
    track_ids: &[LocalTrackId],
    track_data: &BTreeMap<LocalTrackId, TrackSearchData>,
) -> (Option<i32>, i64) {
    let year = track_ids
        .iter()
        .filter_map(|id| track_data.get(id).and_then(|track| track.year))
        .min();
    let recent = track_ids
        .iter()
        .filter_map(|id| track_data.get(id).map(|track| track.modified_at))
        .max()
        .unwrap_or_default();
    (year, recent)
}

fn build_albums(
    tracks: &[LocalTrack],
    track_data: &BTreeMap<LocalTrackId, TrackSearchData>,
    order: &BTreeMap<LocalTrackId, usize>,
) -> Vec<SearchDocument> {
    let mut groups = BTreeMap::<(String, String), CollectionBuilder>::new();
    for track in tracks {
        let Some(album) = track
            .album
            .as_deref()
            .map(str::trim)
            .filter(|album| !album.is_empty())
        else {
            continue;
        };
        let album_artist = track.grouping_album_artist();
        let key = (normalize_text(album), normalize_text(&album_artist));
        let group = groups.entry(key).or_default();
        choose_display(&mut group.display, album);
        choose_display(&mut group.secondary, &album_artist);
        group.track_ids.push(track.id.clone());
    }
    groups
        .into_iter()
        .map(|((album_key, artist_key), group)| {
            let ids = ordered_ids(group.track_ids, order);
            let (year, recent) = document_stats(&ids, track_data);
            let count = ids.len();
            SearchDocument {
                id: LocalFindHitId::Album(LocalAlbumId::from_parts(
                    &group.display,
                    &group.secondary,
                )),
                label: group.display,
                label_sort: album_key.clone(),
                secondary: group.secondary,
                secondary_sort: artist_key.clone(),
                fields: SearchFields {
                    albums: vec![album_key.clone()],
                    album_artists: vec![artist_key.clone()],
                    ..SearchFields::default()
                },
                track_ids: ids,
                year,
                recent,
                artist_sort: artist_key,
                album_sort: album_key,
                locally_playable_count: count,
                total_track_count: count,
            }
        })
        .collect()
}

fn build_artists(
    tracks: &[LocalTrack],
    track_data: &BTreeMap<LocalTrackId, TrackSearchData>,
    order: &BTreeMap<LocalTrackId, usize>,
) -> Vec<SearchDocument> {
    let mut groups = BTreeMap::<String, CollectionBuilder>::new();
    for track in tracks {
        let mut names = track
            .artist
            .iter()
            .map(String::as_str)
            .chain(track.album_artist.iter().map(String::as_str))
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .collect::<Vec<_>>();
        if names.is_empty() {
            names.push("Unknown Artist");
        }
        let mut seen = BTreeSet::new();
        for name in names {
            let key = normalize_text(name);
            if !seen.insert(key.clone()) {
                continue;
            }
            let group = groups.entry(key).or_default();
            choose_display(&mut group.display, name);
            group.track_ids.push(track.id.clone());
        }
    }
    groups
        .into_iter()
        .map(|(artist_key, group)| {
            let ids = ordered_ids(group.track_ids, order);
            let (year, recent) = document_stats(&ids, track_data);
            let count = ids.len();
            SearchDocument {
                id: LocalFindHitId::Artist(LocalArtistId::from_name(&group.display)),
                label: group.display,
                label_sort: artist_key.clone(),
                secondary: String::new(),
                secondary_sort: String::new(),
                fields: SearchFields {
                    artists: vec![artist_key.clone()],
                    album_artists: vec![artist_key.clone()],
                    ..SearchFields::default()
                },
                track_ids: ids,
                year,
                recent,
                artist_sort: artist_key,
                album_sort: String::new(),
                locally_playable_count: count,
                total_track_count: count,
            }
        })
        .collect()
}

fn build_genres(
    tracks: &[LocalTrack],
    track_data: &BTreeMap<LocalTrackId, TrackSearchData>,
    order: &BTreeMap<LocalTrackId, usize>,
) -> Vec<SearchDocument> {
    let mut groups = BTreeMap::<String, CollectionBuilder>::new();
    for track in tracks {
        for genre in &track.genre {
            let genre = genre.trim();
            if genre.is_empty() {
                continue;
            }
            let key = normalize_text(genre);
            let group = groups.entry(key).or_default();
            choose_display(&mut group.display, genre);
            group.track_ids.push(track.id.clone());
        }
    }
    groups
        .into_iter()
        .map(|(genre_key, group)| {
            let ids = ordered_ids(group.track_ids, order);
            let (year, recent) = document_stats(&ids, track_data);
            let count = ids.len();
            SearchDocument {
                id: LocalFindHitId::Genre(genre_key.clone()),
                label: group.display,
                label_sort: genre_key.clone(),
                secondary: String::new(),
                secondary_sort: String::new(),
                fields: SearchFields {
                    genres: vec![genre_key],
                    ..SearchFields::default()
                },
                track_ids: ids,
                year,
                recent,
                artist_sort: String::new(),
                album_sort: String::new(),
                locally_playable_count: count,
                total_track_count: count,
            }
        })
        .collect()
}

fn build_folders(
    tracks: &[LocalTrack],
    track_data: &BTreeMap<LocalTrackId, TrackSearchData>,
    order: &BTreeMap<LocalTrackId, usize>,
) -> Vec<SearchDocument> {
    let mut groups = BTreeMap::<PathBuf, Vec<LocalTrackId>>::new();
    for track in tracks {
        if let Some(parent) = track.path.parent() {
            groups
                .entry(parent.to_path_buf())
                .or_default()
                .push(track.id.clone());
        }
    }
    groups
        .into_iter()
        .map(|(path, ids)| {
            let ids = ordered_ids(ids, order);
            let (year, recent) = document_stats(&ids, track_data);
            let count = ids.len();
            let label = path
                .file_name()
                .and_then(|name| name.to_str())
                .filter(|name| !name.trim().is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| path.display().to_string());
            let path_display = path.to_string_lossy().into_owned();
            let path_key = normalize_text(&path_display);
            SearchDocument {
                id: LocalFindHitId::Folder(path),
                label_sort: normalize_text(&label),
                label,
                secondary: path_display,
                secondary_sort: path_key.clone(),
                fields: SearchFields {
                    paths: vec![path_key],
                    ..SearchFields::default()
                },
                track_ids: ids,
                year,
                recent,
                artist_sort: String::new(),
                album_sort: String::new(),
                locally_playable_count: count,
                total_track_count: count,
            }
        })
        .collect()
}

fn playlist_document(
    projection: LocalFindPlaylistProjection,
    track_data: &BTreeMap<LocalTrackId, TrackSearchData>,
) -> SearchDocument {
    let (year, recent) = document_stats(&projection.track_ids, track_data);
    let local_count = projection.locally_playable_count();
    SearchDocument {
        id: LocalFindHitId::Playlist(projection.id),
        label_sort: normalize_text(&projection.name),
        label: projection.name,
        secondary: String::new(),
        secondary_sort: String::new(),
        fields: SearchFields::default(),
        track_ids: projection.track_ids,
        year,
        recent,
        artist_sort: String::new(),
        album_sort: String::new(),
        locally_playable_count: local_count,
        total_track_count: projection.total_track_count,
    }
}

#[derive(Default)]
struct PlaylistResolver<'a> {
    tracks: &'a [LocalTrack],
    ids: BTreeSet<LocalTrackId>,
    by_path: BTreeMap<PathBuf, BTreeSet<LocalTrackId>>,
    by_stable_key: BTreeMap<String, BTreeSet<LocalTrackId>>,
    by_isrc: BTreeMap<String, BTreeSet<LocalTrackId>>,
}

impl<'a> PlaylistResolver<'a> {
    fn new(tracks: &'a [LocalTrack]) -> Self {
        let mut resolver = Self {
            tracks,
            ..Self::default()
        };
        for track in tracks {
            resolver.ids.insert(track.id.clone());
            resolver
                .by_path
                .entry(track.path.clone())
                .or_default()
                .insert(track.id.clone());
            for key in [
                track.linked_video_id.as_deref(),
                track.origin_key.as_deref(),
                track.origin_url.as_deref(),
            ]
            .into_iter()
            .flatten()
            .map(str::trim)
            .filter(|key| !key.is_empty())
            {
                resolver
                    .by_stable_key
                    .entry(key.to_owned())
                    .or_default()
                    .insert(track.id.clone());
            }
            if let Some(isrc) = track
                .isrc
                .as_deref()
                .map(str::trim)
                .filter(|isrc| !isrc.is_empty())
            {
                resolver
                    .by_isrc
                    .entry(normalize_text(isrc))
                    .or_default()
                    .insert(track.id.clone());
            }
        }
        resolver
    }

    fn resolve(&self, playlist: &LocalFindPlaylistInput) -> LocalFindPlaylistProjection {
        let resolutions = playlist
            .entries
            .iter()
            .map(|entry| self.resolve_entry(entry))
            .collect::<Vec<_>>();
        let track_ids = resolutions
            .iter()
            .filter_map(|resolution| match resolution {
                LocalFindPlaylistEntryResolution::Resolved { track_id, .. } => {
                    Some(track_id.clone())
                }
                LocalFindPlaylistEntryResolution::Unresolved
                | LocalFindPlaylistEntryResolution::Ambiguous => None,
            })
            .collect();
        LocalFindPlaylistProjection {
            id: playlist.id.clone(),
            name: playlist.name.clone(),
            total_track_count: playlist.entries.len(),
            track_ids,
            resolutions,
        }
    }

    fn resolve_entry(
        &self,
        entry: &LocalFindPlaylistEntryInput,
    ) -> LocalFindPlaylistEntryResolution {
        if let Some(path) = &entry.readable_local_path
            && let Some(resolution) =
                resolve_unique(self.by_path.get(path), LocalFindPlaylistMatch::LocalPath)
        {
            return resolution;
        }

        let mut stable_candidates = BTreeSet::new();
        if let Some(id) = &entry.local_track_id
            && self.ids.contains(id)
        {
            stable_candidates.insert(id.clone());
        }
        for key in entry
            .stable_keys
            .iter()
            .map(|key| key.trim())
            .filter(|key| !key.is_empty())
        {
            if let Some(ids) = self.by_stable_key.get(key) {
                stable_candidates.extend(ids.iter().cloned());
            }
        }
        if let Some(resolution) = resolve_unique(
            (!stable_candidates.is_empty()).then_some(&stable_candidates),
            LocalFindPlaylistMatch::StableIdentity,
        ) {
            return resolution;
        }

        if let Some(isrc) = entry
            .isrc
            .as_deref()
            .map(str::trim)
            .filter(|isrc| !isrc.is_empty())
            && let Some(resolution) = resolve_unique(
                self.by_isrc.get(&normalize_text(isrc)),
                LocalFindPlaylistMatch::Isrc,
            )
        {
            return resolution;
        }

        let metadata_candidates = self.metadata_candidates(entry);
        resolve_unique(
            (!metadata_candidates.is_empty()).then_some(&metadata_candidates),
            LocalFindPlaylistMatch::Metadata,
        )
        .unwrap_or(LocalFindPlaylistEntryResolution::Unresolved)
    }

    fn metadata_candidates(&self, entry: &LocalFindPlaylistEntryInput) -> BTreeSet<LocalTrackId> {
        let title = normalize_text(entry.title.trim());
        let artists = normalized_nonempty(entry.artists.iter().map(String::as_str));
        if title.is_empty() || artists.is_empty() {
            return BTreeSet::new();
        }
        let album = normalize_optional(entry.album.as_deref());
        self.tracks
            .iter()
            .filter(|track| normalize_text(&track.display_title()) == title)
            .filter(|track| {
                let track_artists = normalized_nonempty(track.artist.iter().map(String::as_str));
                artists.iter().any(|artist| track_artists.contains(artist))
            })
            .filter(|track| {
                album.as_ref().is_none_or(|album| {
                    normalize_optional(track.album.as_deref()).as_ref() == Some(album)
                })
            })
            .filter(|track| {
                entry.duration_ms.is_none_or(|duration| {
                    track
                        .duration_ms
                        .is_some_and(|candidate| candidate.abs_diff(duration) <= 2_000)
                })
            })
            .map(|track| track.id.clone())
            .collect()
    }
}

fn resolve_unique(
    candidates: Option<&BTreeSet<LocalTrackId>>,
    matched_by: LocalFindPlaylistMatch,
) -> Option<LocalFindPlaylistEntryResolution> {
    let candidates = candidates?;
    match candidates.len() {
        0 => None,
        1 => Some(LocalFindPlaylistEntryResolution::Resolved {
            track_id: candidates.iter().next().expect("one candidate").clone(),
            matched_by,
        }),
        _ => Some(LocalFindPlaylistEntryResolution::Ambiguous),
    }
}

#[cfg(test)]
#[path = "find/tests.rs"]
mod tests;
