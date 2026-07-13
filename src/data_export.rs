//! Portable, credential-free personal-data export.
//!
//! The export schema is deliberately independent from the persistence schemas. Every value is
//! copied through an explicit allowlist so adding a new field to `Config`, `Song`, or another
//! store cannot accidentally make it public. Export files contain listening history and remain
//! private user data even though credentials, machine paths, playable URLs, and transient state
//! are excluded.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
#[cfg(not(windows))]
use std::fs::OpenOptions;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::{Value, json};

use crate::api::{Song, is_youtube_video_id};
use crate::config::Config;
use crate::library::Library;
use crate::playlists::Playlists;
use crate::search_source::{SearchConfig, SearchSource};
use crate::signals::Signals;
use crate::station::StationStore;
use crate::streaming::StreamingConfig;

pub(crate) mod live;
mod offline;
mod publish;
#[cfg(unix)]
mod unix_private;
pub(crate) use offline::load_playlists_read_only;
#[cfg(target_os = "macos")]
mod macos_private;
#[cfg(windows)]
pub(crate) mod windows_private;

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
#[cfg(windows)]
use std::os::windows::fs::MetadataExt;

/// Stable wire schema version for [`ExportSnapshot`].
pub const EXPORT_SCHEMA_VERSION: u32 = 1;
/// Hard cap for a single export, including pretty-print whitespace and the trailing newline.
pub const EXPORT_MAX_BYTES: u64 = 192 * 1024 * 1024;

const EXPORT_KIND: &str = "yututui_personal_data_export";
const EXPORT_PROFILE: &str = "portable";
const FILE_PREFIX: &str = "yututui-personal-data-v1";

const OMITTED_CATEGORIES: &[&str] = &[
    "authentication cookies, API keys, OAuth tokens, and account identifiers",
    "filesystem paths and machine-specific audio device settings",
    "playable, origin, artwork, and radio stream URLs",
    "actual downloads and recordings, download manifests, and media sidecars",
    "pending scrobbles, transfer jobs and reports, and session queues",
    "AI usage logs, generated caches, artwork caches, and application logs",
    "managed tool binaries and paths, desktop geometry, and recovery backups",
];

/// An owned, already-sanitized snapshot that is safe to move to a blocking worker.
///
/// This type intentionally does not retain `Config`, `Song`, or any secret-bearing source type.
#[derive(Debug, Clone, Serialize)]
pub struct ExportSnapshot {
    kind: String,
    schema_version: u32,
    created_at_unix: u64,
    source_app_version: String,
    profile: String,
    privacy: PrivacyMetadata,
    summary: ExportSummary,
    settings: PortableSettingsV1,
    library: PortableLibraryV1,
    playlists: Vec<PortablePlaylistV1>,
    preferences: PortablePreferencesV1,
}

impl ExportSnapshot {
    /// Project the current in-memory state into the portable v1 allowlist.
    pub fn new(
        config: &Config,
        library: &Library,
        playlists: &Playlists,
        signals: &Signals,
        station: &StationStore,
    ) -> Self {
        Self::new_at(config, library, playlists, signals, station, unix_now())
    }

    fn new_at(
        config: &Config,
        library: &Library,
        playlists: &Playlists,
        signals: &Signals,
        station: &StationStore,
        created_at_unix: u64,
    ) -> Self {
        let catalog = build_catalog_map(library, playlists);
        let mut tracks_without_stable_id = 0usize;

        let portable_library = PortableLibraryV1 {
            favorites: project_tracks(&library.favorites, &mut tracks_without_stable_id),
            history: project_tracks(library.history.iter(), &mut tracks_without_stable_id),
            radio_favorites: project_tracks(
                &library.radio_favorites,
                &mut tracks_without_stable_id,
            ),
            radio_history: project_tracks(library.radios.iter(), &mut tracks_without_stable_id),
        };

        let mut portable_playlists = Vec::with_capacity(playlists.list().len());
        for playlist in playlists.list() {
            portable_playlists.push(PortablePlaylistV1 {
                id: safe_public_identifier(&playlist.id, 128),
                name: safe_text(&playlist.name, 300).unwrap_or_else(|| "Playlist".to_owned()),
                tracks: project_tracks(&playlist.songs, &mut tracks_without_stable_id),
            });
        }

        let (portable_signals, omitted_signal_tracks, omitted_signal_events) =
            project_signals(signals, &catalog);
        let (portable_station, omitted_station_values) = project_station(station);

        let summary = ExportSummary {
            favorites: portable_library.favorites.len(),
            history: portable_library.history.len(),
            radio_favorites: portable_library.radio_favorites.len(),
            radio_history: portable_library.radio_history.len(),
            playlists: portable_playlists.len(),
            playlist_tracks: portable_playlists
                .iter()
                .map(|playlist| playlist.tracks.len())
                .sum(),
            preference_track_signals: portable_signals.track_signals.len(),
            preference_play_events: portable_signals.play_log.len(),
            artist_affinities: portable_signals.artist_weights.len(),
            tracks_without_stable_id,
            omitted_signal_tracks,
            omitted_signal_events,
            omitted_station_values,
        };

        Self {
            kind: EXPORT_KIND.to_owned(),
            schema_version: EXPORT_SCHEMA_VERSION,
            created_at_unix,
            source_app_version: env!("CARGO_PKG_VERSION").to_owned(),
            profile: EXPORT_PROFILE.to_owned(),
            privacy: PrivacyMetadata {
                credentials_included: false,
                filesystem_paths_included: false,
                playable_urls_included: false,
                media_files_included: false,
                contains_listening_history: true,
                omitted: OMITTED_CATEGORIES
                    .iter()
                    .map(|category| (*category).to_owned())
                    .collect(),
            },
            summary,
            settings: project_settings(config),
            library: portable_library,
            playlists: portable_playlists,
            preferences: PortablePreferencesV1 {
                signals: portable_signals,
                station: portable_station,
            },
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct PrivacyMetadata {
    credentials_included: bool,
    filesystem_paths_included: bool,
    playable_urls_included: bool,
    media_files_included: bool,
    contains_listening_history: bool,
    omitted: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct ExportSummary {
    favorites: usize,
    history: usize,
    radio_favorites: usize,
    radio_history: usize,
    playlists: usize,
    playlist_tracks: usize,
    preference_track_signals: usize,
    preference_play_events: usize,
    artist_affinities: usize,
    tracks_without_stable_id: usize,
    omitted_signal_tracks: usize,
    omitted_signal_events: usize,
    omitted_station_values: usize,
}

/// Explicit groups of portable settings. Each `Value` is constructed field-by-field below;
/// none is a serialization of `Config` or a secret-bearing nested config object.
#[derive(Debug, Clone, Serialize)]
struct PortableSettingsV1 {
    general: Value,
    playback: Value,
    search: Value,
    streaming: Value,
    animations: Value,
    assistant: Value,
    appearance: Value,
    bindings: Value,
    integrations: Value,
    tools: Value,
    audio: Value,
    recording: Value,
}

#[derive(Debug, Clone, Serialize)]
struct PortableLibraryV1 {
    favorites: Vec<PortableTrackV1>,
    history: Vec<PortableTrackV1>,
    radio_favorites: Vec<PortableTrackV1>,
    radio_history: Vec<PortableTrackV1>,
}

#[derive(Debug, Clone, Serialize)]
struct PortablePlaylistV1 {
    id: Option<String>,
    name: String,
    tracks: Vec<PortableTrackV1>,
}

#[derive(Debug, Clone, Serialize)]
struct PortablePreferencesV1 {
    signals: PortableSignalsV1,
    station: PortableStationStoreV1,
}

#[derive(Debug, Clone, Serialize)]
struct PortableTrackV1 {
    catalog: Option<PortableCatalogId>,
    local_origin: bool,
    source: SearchSource,
    title: String,
    artist: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    artists: Vec<String>,
    duration: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_secs: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    album: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    album_artist: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    album_artists: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    album_release_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    album_release_date_precision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    album_total_tracks: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    album_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    disc_number: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    track_number: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    explicit: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    isrc: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
struct PortableCatalogId {
    source: SearchSource,
    id: String,
}

#[derive(Debug, Clone, Serialize)]
struct PortableSignalsV1 {
    track_signals: Vec<PortableTrackSignalV1>,
    artist_weights: BTreeMap<String, f32>,
    play_log: Vec<PortablePlayEventV1>,
}

#[derive(Debug, Clone, Serialize)]
struct PortableTrackSignalV1 {
    catalog: PortableCatalogId,
    play_count: u32,
    completed_count: u32,
    skip_count: u32,
    last_played_at: i64,
    last_completion: f32,
    disliked: bool,
}

#[derive(Debug, Clone, Serialize)]
struct PortablePlayEventV1 {
    catalog: PortableCatalogId,
    played_at: i64,
}

#[derive(Debug, Clone, Serialize)]
struct PortableStationStoreV1 {
    active: Option<PortableStationProfileV1>,
}

#[derive(Debug, Clone, Serialize)]
struct PortableStationProfileV1 {
    #[serde(skip_serializing_if = "Option::is_none")]
    query: Option<String>,
    explore: crate::station::Explore,
    avoid_artist_keys: Vec<String>,
}

/// Failures are intentionally small and do not retain or print source data.
#[derive(Debug)]
pub enum ExportError {
    NoDownloadsDirectory,
    InvalidDestination(String),
    SourceStore { store: &'static str, detail: String },
    TooLarge { max_bytes: u64 },
    Io(io::Error),
    Serialization(serde_json::Error),
}

impl fmt::Display for ExportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoDownloadsDirectory => write!(
                f,
                "the OS Downloads directory is unavailable; choose an existing directory with --to"
            ),
            Self::InvalidDestination(reason) => write!(f, "invalid export directory: {reason}"),
            Self::SourceStore { store, detail } => {
                write!(f, "cannot safely export the {store} store: {detail}")
            }
            Self::TooLarge { max_bytes } => write!(
                f,
                "personal-data export exceeds the {} MiB safety limit",
                max_bytes / (1024 * 1024)
            ),
            Self::Io(error) => write!(f, "could not write personal-data export: {error}"),
            Self::Serialization(error) => {
                write!(f, "could not serialize personal-data export: {error}")
            }
        }
    }
}

impl std::error::Error for ExportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Serialization(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for ExportError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for ExportError {
    fn from(value: serde_json::Error) -> Self {
        Self::Serialization(value)
    }
}

/// Resolve the platform Downloads directory. There is deliberately no current-directory fallback.
pub fn default_export_directory() -> Result<PathBuf, ExportError> {
    directories::UserDirs::new()
        .and_then(|dirs| dirs.download_dir().map(Path::to_path_buf))
        .ok_or(ExportError::NoDownloadsDirectory)
}

/// Return whether `name` has the exact shape generated for a completed v1 export.
///
/// Remote clients use this as part of validating an owner's completion response before showing a
/// filesystem path as a successful backup.
pub fn is_personal_export_file_name(name: &str) -> bool {
    let Some(stem) = name
        .strip_prefix(FILE_PREFIX)
        .and_then(|rest| rest.strip_prefix('-'))
        .and_then(|rest| rest.strip_suffix(".json"))
    else {
        return false;
    };
    let Some((created_at, suffix)) = stem.rsplit_once('-') else {
        return false;
    };
    !created_at.is_empty()
        && created_at.bytes().all(|byte| byte.is_ascii_digit())
        && suffix.len() == 16
        && suffix
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Load all typed stores (including persistence journals), sanitize them, and export to `directory`.
pub fn export_from_disk(directory: &Path) -> Result<PathBuf, ExportError> {
    let sources = offline::load_sources()?;
    let snapshot = ExportSnapshot::new(
        &sources.config,
        &sources.library,
        &sources.playlists,
        &sources.signals,
        &sources.station,
    );
    // The projection owns every exported value. Release the secret-bearing source stores before
    // serialization so a large offline export does not retain both full representations.
    drop(sources);
    export_snapshot(directory, &snapshot)
}

/// Pretty-print and atomically publish a private, uniquely named JSON export.
pub fn export_snapshot(
    directory: &Path,
    snapshot: &ExportSnapshot,
) -> Result<PathBuf, ExportError> {
    let destination = validate_destination(directory)?;
    let suffix = random_suffix()?;
    let final_path = destination.join(format!(
        "{FILE_PREFIX}-{}-{suffix}.json",
        snapshot.created_at_unix
    ));
    let (temp_path, file) = create_private_temp(&destination)?;

    let write_result = (|| {
        let opened_temp_metadata = file.metadata()?;
        revalidate_destination_security(&destination, &opened_temp_metadata)?;
        revalidate_temp_file(&temp_path, &opened_temp_metadata)?;
        let mut limited = LimitedWriter::new(file, EXPORT_MAX_BYTES);
        if let Err(error) = serde_json::to_writer_pretty(&mut limited, snapshot) {
            return if limited.exceeded {
                Err(ExportError::TooLarge {
                    max_bytes: EXPORT_MAX_BYTES,
                })
            } else {
                Err(ExportError::Serialization(error))
            };
        }
        if let Err(error) = limited.write_all(b"\n") {
            return if limited.exceeded {
                Err(ExportError::TooLarge {
                    max_bytes: EXPORT_MAX_BYTES,
                })
            } else {
                Err(ExportError::Io(error))
            };
        }
        limited.flush()?;
        let file = limited.into_inner();
        file.sync_all()?;
        // Establish the complete private temp name durably before publishing another link/name.
        // If the later post-publish directory sync fails, that durable temp entry is the recovery
        // anchor and must not be removed.
        publish::sync_directory(&destination)?;
        let temp_metadata = file.metadata()?;
        #[cfg(windows)]
        let temp_identity = windows_private::file_identity(&file)?;
        drop(file);

        revalidate_destination(&destination)?;
        revalidate_destination_security(&destination, &temp_metadata)?;
        revalidate_temp_file(&temp_path, &temp_metadata)?;
        #[cfg(windows)]
        windows_private::revalidate_path_identity(&temp_path, temp_identity)?;
        #[cfg(windows)]
        let _destination_chain_guard =
            windows_private::verify_private_destination_chain(&destination)
                .map_err(windows_destination_error)?;
        let outcome = publish::no_replace(&temp_path, &final_path)?;
        Ok(publish::finish(
            &destination,
            &temp_path,
            &final_path,
            outcome,
        ))
    })();

    if write_result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    write_result
}

fn project_settings(config: &Config) -> PortableSettingsV1 {
    let streaming_defaults = StreamingConfig::default();
    let theme = config.theme.normalized();
    let radio_theme = config.radio_theme.as_ref().map(|theme| theme.normalized());
    let audio = config.audio.runtime();

    PortableSettingsV1 {
        general: json!({
            "volume": config.volume,
            "download_concurrency": config.download_concurrency,
            "mouse": config.mouse,
            "album_art": config.album_art,
            "local_include_download_dir": config.local.include_download_dir,
            "retro_mode": config.retro_mode,
            "language": config.language,
            "update_check_enabled": config.update_check_enabled,
        }),
        playback: json!({
            "eq_preset": config.eq_preset,
            "eq_bands": finite_bands(config.eq_bands),
            "normalize": config.normalize,
            "speed": finite_f64_option(config.speed),
            "seek_seconds": finite_f64_option(config.seek_seconds),
            "mouse_wheel_volume": config.mouse_wheel_volume,
            "text_zoom": config.text_zoom,
            "zoom_wheel_lock": config.zoom_wheel_lock,
            "gapless": config.gapless,
            "shuffle": config.shuffle,
            "repeat": config.repeat,
            "enqueue_next": config.enqueue_next,
            "autoplay_streaming": config.autoplay_streaming,
            "autoplay_on_start": config.autoplay_on_start,
            "auto_continue_videos": config.auto_continue_videos,
            "video_layout": config.video_layout,
            "media_controls": config.media_controls,
        }),
        search: project_search(&config.search),
        streaming: project_streaming(&config.streaming, &streaming_defaults),
        animations: project_animations(config),
        assistant: json!({
            "gemini_model": config.gemini_model,
            "enabled": config.ai_enabled,
            "romanized_titles": config.romanized_titles,
            "language": config.dj_gem_language,
        }),
        appearance: json!({
            "theme": {
                "preset": theme.preset,
                "overrides": theme.overrides,
            },
            "radio_theme": radio_theme.map(|theme| json!({
                "preset": theme.preset,
                "overrides": theme.overrides,
            })),
        }),
        bindings: json!({
            "keybindings": safe_keybindings(&config.keybindings),
            "mouse_bindings": safe_identifier_bindings(&config.mouse_bindings),
        }),
        integrations: json!({
            "scrobble": {
                "local_files": config.scrobble.local_files,
                "lastfm": {
                    "enabled": config.scrobble.lastfm.enabled,
                    "love_sync": config.scrobble.lastfm.love_sync,
                },
                "listenbrainz": {
                    "enabled": config.scrobble.listenbrainz.enabled,
                },
            },
            "spotify": {
                "redirect_port": config.spotify.redirect_port,
                "market": config.spotify.market.as_deref().and_then(|market| safe_public_identifier(market, 16)),
                "import_mode": config.spotify.import_mode,
            },
        }),
        tools: json!({
            "ytdlp_managed": config.tools.ytdlp_managed,
            "ytdlp_channel": config.tools.ytdlp_channel,
        }),
        audio: json!({
            "backend": audio.backend,
            "mpv": {
                "cache_forward": audio.mpv.cache_forward,
                "cache_back": audio.mpv.cache_back,
                "long_form_seek_optimization": audio.mpv.long_form_seek_optimization,
            },
        }),
        recording: json!({
            "mode": config.recording.mode,
            "min_duration_secs": config.recording.min_duration_secs,
            "max_duration_secs": config.recording.max_duration_secs,
            "past_tracks_count": config.recording.past_tracks_count,
            "notify": config.recording.notify,
        }),
    }
}

fn project_search(search: &SearchConfig) -> Value {
    json!({
        "source": search.source,
        "streaming_source": search.streaming_source,
        "youtube": search.youtube,
        "soundcloud": search.soundcloud,
        "audius": search.audius,
        "jamendo": search.jamendo,
        "internet_archive": search.internet_archive,
        "radio_browser": search.radio_browser,
    })
}

fn project_streaming(streaming: &StreamingConfig, defaults: &StreamingConfig) -> Value {
    json!({
        "mode": streaming.mode,
        "weights": {
            "cooccurrence": finite_f32_or(streaming.weights.cooccurrence, defaults.weights.cooccurrence),
            "seed_affinity": finite_f32_or(streaming.weights.seed_affinity, defaults.weights.seed_affinity),
            "novelty": finite_f32_or(streaming.weights.novelty, defaults.weights.novelty),
            "ytm_continuation": finite_f32_or(streaming.weights.ytm_continuation, defaults.weights.ytm_continuation),
            "completion": finite_f32_or(streaming.weights.completion, defaults.weights.completion),
            "music_tier": finite_f32_or(streaming.weights.music_tier, defaults.weights.music_tier),
            "dislike_penalty": finite_f32_or(streaming.weights.dislike_penalty, defaults.weights.dislike_penalty),
        },
        "similarity_weights": {
            "cooccurrence": finite_f32_or(streaming.sim_weights.cooc, defaults.sim_weights.cooc),
            "artist": finite_f32_or(streaming.sim_weights.artist, defaults.sim_weights.artist),
            "album": finite_f32_or(streaming.sim_weights.album, defaults.sim_weights.album),
        },
        "album_gap": streaming.album_gap,
        "sample_top_k": streaming.sample_top_k,
        "recency_half_life_days": finite_f32_or(streaming.recency_half_life_days, defaults.recency_half_life_days),
        "min_duration_secs": streaming.min_duration_secs,
        "max_duration_secs": streaming.max_duration_secs,
        "cooccurrence": {
            "window": streaming.cooc.window,
            "sppmi_k": finite_f32_or(streaming.cooc.sppmi_k, defaults.cooc.sppmi_k),
            "reverse": finite_f32_or(streaming.cooc.reverse, defaults.cooc.reverse),
            "session_gap_min": streaming.cooc.session_gap_min,
            "session_max": streaming.cooc.session_max,
        },
        "ai_rerank": {
            "enabled": streaming.ai.enabled,
            "shortlist": streaming.ai.shortlist,
            "picks": streaming.ai.picks,
            "smart_gate": streaming.ai.smart_gate,
            "ambiguity_gap": finite_f32_or(streaming.ai.ambiguity_gap, defaults.ai.ambiguity_gap),
        },
        "music_gate": {
            "enabled": streaming.gate.enabled,
            "gate_watch_playlist": streaming.gate.gate_watch_playlist,
            "block_altered_versions": streaming.gate.block_altered_versions,
        },
    })
}

fn project_animations(config: &Config) -> Value {
    // `AnimationsConfig` is a flat serde struct whose field names ARE the portable keys, so
    // serializing it directly keeps this projection in lock-step with every new effect flag
    // (a hand-written `json!` here also blew the macro recursion limit at ~33 keys).
    serde_json::to_value(config.animations).unwrap_or(Value::Null)
}

fn project_tracks<'a>(
    songs: impl IntoIterator<Item = &'a Song>,
    tracks_without_stable_id: &mut usize,
) -> Vec<PortableTrackV1> {
    songs
        .into_iter()
        .map(|song| {
            let catalog = catalog_for_song(song);
            if catalog.is_none() {
                *tracks_without_stable_id = tracks_without_stable_id.saturating_add(1);
            }
            PortableTrackV1 {
                catalog,
                local_origin: song.is_local() || song.video_id.starts_with("local:"),
                source: song.source,
                title: safe_text(&song.title, 300).unwrap_or_else(|| {
                    if song.is_local() {
                        "Local track".to_owned()
                    } else {
                        "Unknown".to_owned()
                    }
                }),
                artist: safe_text(&song.artist, 200).unwrap_or_default(),
                artists: song
                    .artists
                    .iter()
                    .filter_map(|artist| safe_text(artist, 200))
                    .collect(),
                duration: safe_text(&song.duration, 32).unwrap_or_default(),
                duration_secs: song.duration_secs,
                album: song
                    .album
                    .as_deref()
                    .and_then(|value| safe_text(value, 200)),
                album_artist: song
                    .album_artist
                    .as_deref()
                    .and_then(|value| safe_text(value, 200)),
                album_artists: song
                    .album_artists
                    .iter()
                    .filter_map(|artist| safe_text(artist, 200))
                    .collect(),
                album_release_date: song
                    .album_release_date
                    .as_deref()
                    .and_then(|value| safe_public_identifier(value, 32)),
                album_release_date_precision: song
                    .album_release_date_precision
                    .as_deref()
                    .and_then(|value| safe_public_identifier(value, 32)),
                album_total_tracks: song.album_total_tracks,
                album_type: song
                    .album_type
                    .as_deref()
                    .and_then(|value| safe_text(value, 64)),
                disc_number: song.disc_number,
                track_number: song.track_number,
                explicit: song.explicit,
                isrc: song
                    .isrc
                    .as_deref()
                    .and_then(|value| safe_public_identifier(value, 32)),
            }
        })
        .collect()
}

fn build_catalog_map(
    library: &Library,
    playlists: &Playlists,
) -> HashMap<String, PortableCatalogId> {
    let mut map = HashMap::new();
    let library_songs = library
        .favorites
        .iter()
        .chain(library.history.iter())
        .chain(library.radio_favorites.iter())
        .chain(library.radios.iter());
    let playlist_songs = playlists
        .list()
        .iter()
        .flat_map(|playlist| playlist.songs.iter());
    for song in library_songs.chain(playlist_songs) {
        if let Some(catalog) = catalog_for_song(song) {
            map.insert(song.video_id.clone(), catalog.clone());
            if let Some(yt_id) = song
                .yt_video_id
                .as_deref()
                .filter(|id| is_youtube_video_id(id))
            {
                map.insert(yt_id.to_owned(), catalog);
            }
        }
    }
    map
}

fn catalog_for_song(song: &Song) -> Option<PortableCatalogId> {
    if let Some(id) = song
        .yt_video_id
        .as_deref()
        .filter(|id| is_youtube_video_id(id))
    {
        return Some(PortableCatalogId {
            source: SearchSource::Youtube,
            id: id.to_owned(),
        });
    }
    if song.video_id.starts_with("local:") || song.is_local() {
        return None;
    }
    if song.source == SearchSource::Youtube {
        return is_youtube_video_id(&song.video_id).then(|| PortableCatalogId {
            source: SearchSource::Youtube,
            id: song.video_id.clone(),
        });
    }
    if song.source == SearchSource::All {
        return None;
    }
    let prefix = format!("{}:", song.source.id_prefix());
    let raw_id = song.video_id.strip_prefix(&prefix)?;
    safe_public_identifier(raw_id, 128).map(|id| PortableCatalogId {
        source: song.source,
        id,
    })
}

fn project_signals(
    signals: &Signals,
    catalog: &HashMap<String, PortableCatalogId>,
) -> (PortableSignalsV1, usize, usize) {
    // `Signals` keeps its maps private. Serializing to a value here is still fail-closed: only
    // the explicitly named keys and scalar fields below are copied into the public projection.
    let raw = match serde_json::to_value(signals) {
        Ok(raw) => raw,
        Err(error) => {
            // A hand-edited/non-finite signal must not block the rest of a user's backup.
            // The public play log is enough to provide conservative omission accounting while
            // the private signal maps remain fail-closed and are dropped as one damaged group.
            tracing::warn!(%error, "omitting malformed preference signals from personal-data export");
            let omitted_events = signals.play_log().len();
            let omitted_tracks = signals
                .play_log()
                .iter()
                .map(|(id, _)| id)
                .collect::<HashSet<_>>()
                .len();
            return (
                PortableSignalsV1 {
                    track_signals: Vec::new(),
                    artist_weights: BTreeMap::new(),
                    play_log: Vec::new(),
                },
                omitted_tracks,
                omitted_events,
            );
        }
    };
    let mut track_signals = Vec::new();
    let mut omitted_tracks = 0usize;
    if let Some(tracks) = raw.get("tracks").and_then(Value::as_object) {
        for (raw_id, values) in tracks {
            let Some(catalog_id) = catalog_for_signal(raw_id, catalog) else {
                omitted_tracks = omitted_tracks.saturating_add(1);
                continue;
            };
            let Some(values) = values.as_object() else {
                omitted_tracks = omitted_tracks.saturating_add(1);
                continue;
            };
            track_signals.push(PortableTrackSignalV1 {
                catalog: catalog_id,
                play_count: json_u32(values.get("play_count")),
                completed_count: json_u32(values.get("completed_count")),
                skip_count: json_u32(values.get("skip_count")),
                last_played_at: values
                    .get("last_played_at")
                    .and_then(Value::as_i64)
                    .unwrap_or_default(),
                last_completion: finite_f64_or(
                    values
                        .get("last_completion")
                        .and_then(Value::as_f64)
                        .unwrap_or_default(),
                    0.0,
                ) as f32,
                disliked: values
                    .get("disliked")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            });
        }
    }
    track_signals.sort_by(|left, right| {
        source_sort_key(left.catalog.source)
            .cmp(source_sort_key(right.catalog.source))
            .then(left.catalog.id.cmp(&right.catalog.id))
    });

    let mut artist_weights = BTreeMap::new();
    if let Some(weights) = raw.get("artist_weight").and_then(Value::as_object) {
        for (artist, weight) in weights {
            let Some(artist) = safe_text(artist, 200) else {
                continue;
            };
            let Some(weight) = weight.as_f64().filter(|weight| weight.is_finite()) else {
                continue;
            };
            artist_weights.insert(artist, weight.clamp(-2.0, 2.0) as f32);
        }
    }

    let mut play_log = Vec::new();
    let mut omitted_events = 0usize;
    if let Some(events) = raw.get("play_log").and_then(Value::as_array) {
        for event in events {
            let Some(pair) = event.as_array().filter(|pair| pair.len() == 2) else {
                omitted_events = omitted_events.saturating_add(1);
                continue;
            };
            let Some(raw_id) = pair[0].as_str() else {
                omitted_events = omitted_events.saturating_add(1);
                continue;
            };
            let Some(catalog_id) = catalog_for_signal(raw_id, catalog) else {
                omitted_events = omitted_events.saturating_add(1);
                continue;
            };
            let Some(played_at) = pair[1].as_i64() else {
                omitted_events = omitted_events.saturating_add(1);
                continue;
            };
            play_log.push(PortablePlayEventV1 {
                catalog: catalog_id,
                played_at,
            });
        }
    }

    (
        PortableSignalsV1 {
            track_signals,
            artist_weights,
            play_log,
        },
        omitted_tracks,
        omitted_events,
    )
}

fn project_station(station: &StationStore) -> (PortableStationStoreV1, usize) {
    let mut omitted = 0usize;
    let active = station.active.as_ref().map(|profile| {
        let query = safe_text(&profile.query, 500);
        if query.is_none() && !profile.query.trim().is_empty() {
            omitted = omitted.saturating_add(1);
        }
        let avoid_artist_keys = profile
            .avoid_artist_keys
            .iter()
            .filter_map(|artist| {
                let safe = safe_text(artist, 200);
                if safe.is_none() && !artist.trim().is_empty() {
                    omitted = omitted.saturating_add(1);
                }
                safe
            })
            .collect();
        PortableStationProfileV1 {
            query,
            explore: profile.explore,
            avoid_artist_keys,
        }
    });
    (PortableStationStoreV1 { active }, omitted)
}

fn catalog_for_signal(
    raw_id: &str,
    known: &HashMap<String, PortableCatalogId>,
) -> Option<PortableCatalogId> {
    known.get(raw_id).cloned().or_else(|| {
        is_youtube_video_id(raw_id).then(|| PortableCatalogId {
            source: SearchSource::Youtube,
            id: raw_id.to_owned(),
        })
    })
}

fn source_sort_key(source: SearchSource) -> &'static str {
    match source {
        SearchSource::Youtube => "youtube",
        SearchSource::SoundCloud => "soundcloud",
        SearchSource::Audius => "audius",
        SearchSource::Jamendo => "jamendo",
        SearchSource::InternetArchive => "internet_archive",
        SearchSource::RadioBrowser => "radio_browser",
        SearchSource::All => "all",
    }
}

fn json_u32(value: Option<&Value>) -> u32 {
    value
        .and_then(Value::as_u64)
        .unwrap_or_default()
        .min(u64::from(u32::MAX)) as u32
}

fn safe_keybindings(bindings: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    bindings
        .iter()
        .filter_map(|(key, value)| {
            let key = safe_public_identifier(key, 128)?;
            let value = safe_binding_text(value, 128)?;
            let chord = crate::keymap::parse_chord(&value)?;
            Some((key, crate::keymap::chord_to_config(chord)))
        })
        .collect()
}

fn safe_identifier_bindings(bindings: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    bindings
        .iter()
        .filter_map(|(key, value)| {
            Some((
                safe_public_identifier(key, 128)?,
                safe_public_identifier(value, 128)?,
            ))
        })
        .collect()
}

fn safe_binding_text(value: &str, max_chars: usize) -> Option<String> {
    let value = value.trim();
    if value.is_empty()
        || value.chars().count() > max_chars
        || value
            .chars()
            .any(|ch| ch.is_control() || is_bidi_control(ch))
    {
        return None;
    }
    Some(value.to_owned())
}

fn safe_public_identifier(value: &str, max_chars: usize) -> Option<String> {
    let value = value.trim();
    if value.is_empty()
        || value.chars().count() > max_chars
        || value.starts_with("local:")
        || !value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        return None;
    }
    Some(value.to_owned())
}

fn safe_text(value: &str, max_chars: usize) -> Option<String> {
    let value = value.trim();
    if value.is_empty() || looks_like_location_or_url(value) {
        return None;
    }
    let clean: String = value
        .chars()
        .filter(|ch| !ch.is_control() && !is_bidi_control(*ch))
        .take(max_chars)
        .collect();
    (!clean.trim().is_empty()).then(|| clean.trim().to_owned())
}

fn looks_like_location_or_url(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    let bytes = value.as_bytes();
    value.starts_with('/')
        || value.starts_with('\\')
        || value.starts_with("~/")
        || value.starts_with("~\\")
        || lower.starts_with("file:")
        || lower.starts_with("local:")
        || lower.contains("://")
        || lower.contains("/users/")
        || lower.contains("/home/")
        || lower.contains("\\users\\")
        || (bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && matches!(bytes[2], b'/' | b'\\'))
}

fn is_bidi_control(ch: char) -> bool {
    matches!(
        ch,
        '\u{200b}'
            | '\u{200c}'
            | '\u{200d}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
            | '\u{feff}'
    )
}

fn finite_bands(bands: Option<[f64; crate::eq::BANDS]>) -> Option<Vec<f64>> {
    let bands = bands?;
    bands
        .iter()
        .all(|value| value.is_finite())
        .then(|| bands.to_vec())
}

fn finite_f64_option(value: Option<f64>) -> Option<f64> {
    value.filter(|value| value.is_finite())
}

fn finite_f32_or(value: f32, fallback: f32) -> f32 {
    if value.is_finite() { value } else { fallback }
}

fn finite_f64_or(value: f64, fallback: f64) -> f64 {
    if value.is_finite() { value } else { fallback }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn random_suffix() -> io::Result<String> {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).map_err(io::Error::other)?;
    let mut suffix = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use fmt::Write as _;
        let _ = write!(&mut suffix, "{byte:02x}");
    }
    Ok(suffix)
}

fn validate_destination(directory: &Path) -> Result<PathBuf, ExportError> {
    let metadata = fs::symlink_metadata(directory).map_err(|error| {
        ExportError::InvalidDestination(format!("{} ({error})", directory.display()))
    })?;
    if destination_is_link(&metadata) {
        return Err(ExportError::InvalidDestination(format!(
            "refusing symlink or reparse-point {}",
            directory.display()
        )));
    }
    if !metadata.is_dir() {
        return Err(ExportError::InvalidDestination(format!(
            "not a directory: {}",
            directory.display()
        )));
    }
    let canonical = fs::canonicalize(directory).map_err(ExportError::Io)?;
    revalidate_destination(&canonical)?;
    Ok(canonical)
}

fn revalidate_destination(directory: &Path) -> Result<(), ExportError> {
    let metadata = fs::symlink_metadata(directory)?;
    if destination_is_link(&metadata) || !metadata.is_dir() {
        return Err(ExportError::InvalidDestination(format!(
            "directory changed while exporting: {}",
            directory.display()
        )));
    }
    #[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
    return Err(ExportError::InvalidDestination(
        "private ACL semantics are not verified on this Unix platform; use macOS, Linux, or Windows"
            .to_owned(),
    ));
    #[cfg(windows)]
    let _guard = windows_private::verify_private_destination_chain(directory)
        .map_err(windows_destination_error)?;
    Ok(())
}

#[cfg(unix)]
fn revalidate_destination_security(
    directory: &Path,
    current_process_file: &fs::Metadata,
) -> Result<(), ExportError> {
    unix_private::verify_destination_chain(directory, current_process_file.uid())
        .map_err(ExportError::InvalidDestination)
}

#[cfg(not(unix))]
fn revalidate_destination_security(
    _directory: &Path,
    _current_process_file: &fs::Metadata,
) -> Result<(), ExportError> {
    Ok(())
}

#[cfg(windows)]
fn windows_destination_error(error: io::Error) -> ExportError {
    ExportError::InvalidDestination(format!(
        "directory chain permissions cannot be proven private; restrict create, delete, and permission-change access to your account or choose a directory under your user profile ({error})"
    ))
}

fn revalidate_temp_file(path: &Path, _opened_metadata: &fs::Metadata) -> Result<(), ExportError> {
    let path_metadata = fs::symlink_metadata(path)?;
    if destination_is_link(&path_metadata) || !path_metadata.is_file() {
        return Err(ExportError::Io(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "export temp file changed before publish",
        )));
    }
    #[cfg(unix)]
    if path_metadata.uid() != _opened_metadata.uid()
        || path_metadata.dev() != _opened_metadata.dev()
        || path_metadata.ino() != _opened_metadata.ino()
    {
        return Err(ExportError::Io(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "export temp file identity changed before publish",
        )));
    }
    Ok(())
}

#[cfg(unix)]
fn destination_is_link(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(windows)]
fn destination_is_link(metadata: &fs::Metadata) -> bool {
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(any(unix, windows)))]
fn destination_is_link(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

fn create_private_temp(directory: &Path) -> Result<(PathBuf, File), ExportError> {
    for _ in 0..16 {
        let path = directory.join(format!(
            ".{FILE_PREFIX}.tmp.{}.{}",
            std::process::id(),
            random_suffix()?
        ));
        match create_private_file(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(ExportError::Io(error)),
        }
    }
    Err(ExportError::Io(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique private export temp file",
    )))
}

#[cfg(unix)]
fn create_private_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW);
    let file = options.open(path)?;
    if let Err(error) = file.set_permissions(fs::Permissions::from_mode(0o600)) {
        drop(file);
        let _ = fs::remove_file(path);
        return Err(error);
    }
    #[cfg(target_os = "macos")]
    if let Err(error) = macos_private::clear_and_verify_acl(&file) {
        drop(file);
        let _ = fs::remove_file(path);
        return Err(error);
    }
    let opened_metadata = match file.metadata() {
        Ok(metadata) => metadata,
        Err(error) => {
            drop(file);
            let _ = fs::remove_file(path);
            return Err(error);
        }
    };
    let path_metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) => {
            drop(file);
            let _ = fs::remove_file(path);
            return Err(error);
        }
    };
    if !opened_metadata.is_file()
        || path_metadata.file_type().is_symlink()
        || !path_metadata.is_file()
        || path_metadata.uid() != opened_metadata.uid()
        || path_metadata.dev() != opened_metadata.dev()
        || path_metadata.ino() != opened_metadata.ino()
        || opened_metadata.permissions().mode() & 0o777 != 0o600
    {
        drop(file);
        let _ = fs::remove_file(path);
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "export temp path identity, ownership, or permissions changed after creation",
        ));
    }
    Ok(file)
}

#[cfg(windows)]
fn create_private_file(path: &Path) -> io::Result<File> {
    windows_private::create_private_file(path)
}

#[cfg(not(any(unix, windows)))]
fn create_private_file(path: &Path) -> io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

struct LimitedWriter<W> {
    inner: W,
    written: u64,
    max: u64,
    exceeded: bool,
}

impl<W> LimitedWriter<W> {
    fn new(inner: W, max: u64) -> Self {
        Self {
            inner,
            written: 0,
            max,
            exceeded: false,
        }
    }

    fn into_inner(self) -> W {
        self.inner
    }
}

impl<W: Write> Write for LimitedWriter<W> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if buffer.len() as u64 > self.max.saturating_sub(self.written) {
            self.exceeded = true;
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "personal-data export size limit exceeded",
            ));
        }
        let count = self.inner.write(buffer)?;
        self.written = self.written.saturating_add(count as u64);
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests;
