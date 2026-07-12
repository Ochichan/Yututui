//! Persisted manifest of downloaded tracks' YouTube identity and rich metadata.
//!
//! [`crate::library::scan_downloads`] rebuilds the on-disk download list from bare files on
//! every launch, which loses the YouTube video id (and the real artist/duration). This
//! manifest — persisted to `<data dir>/downloads.json`, mirroring [`crate::library::Library`]'s
//! atomic write — remembers each download's enriched [`Song`] so a downloaded-and-online track
//! can still produce its YouTube share URL after a restart. Records are keyed by the
//! `local:{canonical path}` `video_id` that both [`Song::local_file`] and
//! [`Song::with_local_path`] derive identically, so a scanned file and its remembered record
//! match exactly. The id-in-filename scheme ([`crate::download`]) covers files this manifest
//! doesn't know (e.g. copied from another machine); this manifest adds the richer metadata.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, BufReader, Read, Seek as _, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::api::{Song, is_youtube_video_id};
use crate::search_source::SearchSource;
use crate::util::safe_fs;

/// Cap on remembered downloads (bounded memory; matches the scan cap).
const STORE_MAX: usize = 999;
/// Cap the on-disk read so a bloated/corrupt/synced `downloads.json` can't be slurped whole
/// at startup; an oversize file is moved to `*.too-large.bak` (never destroyed) and the store
/// falls back to empty. `STORE_MAX` records of paths/titles stay far below this.
const STORE_MAX_BYTES: u64 = 50 * 1024 * 1024;
const SIDECAR_SCHEMA_VERSION: u32 = 2;
const SIDECAR_MAX_BYTES: u64 = 64 * 1024;
const MIN_TAGGED_AUDIO_BYTES: u64 = 32;

/// Per-file metadata handoff written next to downloaded audio.
///
/// The global download manifest is still the main app-level memory. This sidecar travels with
/// the audio file, so a Local Deck rescan can recover the YouTube origin and catalog metadata
/// even if the file is moved within a scanned root or the central manifest is missing.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DownloadSidecar {
    pub schema_version: u32,
    pub video_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub youtube_id: Option<String>,
    pub title: String,
    pub artist: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artists: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album_artist: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub album_artists: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album_release_date: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album_release_date_precision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album_total_tracks: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album_art_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disc_number: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track_number: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explicit: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isrc: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub import_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub import_source_order: Option<u32>,
    pub source: SearchSource,
}

impl Default for DownloadSidecar {
    fn default() -> Self {
        Self {
            schema_version: SIDECAR_SCHEMA_VERSION,
            video_id: String::new(),
            youtube_id: None,
            title: String::new(),
            artist: String::new(),
            artists: Vec::new(),
            album: None,
            album_artist: None,
            album_artists: Vec::new(),
            album_release_date: None,
            album_release_date_precision: None,
            album_total_tracks: None,
            album_type: None,
            album_art_url: None,
            disc_number: None,
            track_number: None,
            explicit: None,
            duration_secs: None,
            isrc: None,
            origin_key: None,
            origin_url: None,
            import_session_id: None,
            import_source_order: None,
            source: SearchSource::Youtube,
        }
    }
}

impl DownloadSidecar {
    pub fn from_song(song: &Song) -> Self {
        let artists = if song.artists.is_empty() {
            (!song.artist.trim().is_empty()).then(|| vec![song.artist.clone()])
        } else {
            Some(song.artists.clone())
        }
        .unwrap_or_default();
        let album_artists = if song.album_artists.is_empty() {
            song.album_artist.iter().cloned().collect()
        } else {
            song.album_artists.clone()
        };
        Self {
            schema_version: SIDECAR_SCHEMA_VERSION,
            video_id: song.video_id.clone(),
            youtube_id: song.youtube_id().map(str::to_owned),
            title: song.title.clone(),
            artist: song.artist.clone(),
            artists,
            album: song.album.clone(),
            album_artist: song.album_artist.clone(),
            album_artists,
            album_release_date: song.album_release_date.clone(),
            album_release_date_precision: song.album_release_date_precision.clone(),
            album_total_tracks: song.album_total_tracks,
            album_type: song.album_type.clone(),
            album_art_url: song.album_art_url.clone(),
            disc_number: song.disc_number,
            track_number: song.track_number,
            explicit: song.explicit,
            duration_secs: song.duration_secs,
            isrc: song.isrc.clone(),
            origin_key: song.origin_key.clone(),
            origin_url: song.origin_url.clone(),
            import_session_id: song.import_session_id.clone(),
            import_source_order: song.import_source_order,
            source: song.source,
        }
    }

    pub fn linked_youtube_id(&self) -> Option<&str> {
        self.youtube_id
            .as_deref()
            .or_else(|| is_youtube_video_id(&self.video_id).then_some(self.video_id.as_str()))
    }
}

/// The download manifest, persisted to `<data dir>/downloads.json`.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DownloadStore {
    /// Enriched download records, most-recent first, de-duplicated by `video_id`.
    tracks: Vec<Song>,
}

impl DownloadStore {
    pub(crate) fn preflight_persistence_recovery()
    -> Result<(), crate::persist::StartupRecoveryError> {
        let Some(path) = store_path() else {
            return Ok(());
        };
        crate::persist::preflight_journal_recovery::<Self>(
            crate::persist::StoreKind::Downloads,
            &path,
            STORE_MAX_BYTES,
        )
    }

    /// Load from disk, falling back to an empty store if absent or unreadable.
    pub fn load() -> Self {
        let Some(path) = store_path() else {
            return DownloadStore::default();
        };
        // Schema-drift tolerant: one changed field no longer discards download history.
        let mut store = crate::persist::load_with_journal_recovery(
            crate::persist::StoreKind::Downloads,
            &path,
            STORE_MAX_BYTES,
            || safe_fs::load_json_or_default_limited::<DownloadStore>(&path, STORE_MAX_BYTES),
        );
        store.tracks.truncate(STORE_MAX);
        store
    }

    /// Persist atomically (temp file + rename). A missing data dir is a no-op.
    pub fn save(&self) -> std::io::Result<()> {
        crate::persist::ensure_persistence_writes_allowed()?;
        let Some(path) = store_path() else {
            return Ok(());
        };
        crate::persist::write_store_json(&path, self)
    }

    /// Whether an existing remembered download shares this YouTube id — the bulk-download dedup
    /// uses it to skip tracks already fetched in a previous session. Missing artifacts are kept
    /// in the manifest for scan-time metadata enrichment, but never suppress a replacement
    /// download after an unlink raced manifest persistence.
    pub fn contains_youtube_id(&self, yt: &str) -> bool {
        self.tracks_with_existing_files()
            .any(|song| song.youtube_id() == Some(yt))
    }

    pub fn tracks(&self) -> &[Song] {
        &self.tracks
    }

    pub(crate) fn tracks_with_existing_files(&self) -> impl Iterator<Item = &Song> {
        self.tracks.iter().filter(|song| {
            song.local_path
                .as_deref()
                .is_some_and(is_existing_manifest_artifact)
        })
    }

    /// Remember an enriched downloaded track (dedup by `video_id`, newest first).
    pub fn record(&mut self, song: &Song) {
        self.tracks.retain(|s| s.video_id != song.video_id);
        self.tracks.insert(0, song.clone());
        self.tracks.truncate(STORE_MAX);
    }

    /// Drop records whose on-disk file is among `paths` (after a confirmed delete).
    pub fn remove_paths(&mut self, paths: &[PathBuf]) {
        let doomed: HashSet<&PathBuf> = paths.iter().collect();
        self.tracks
            .retain(|s| s.local_path.as_ref().is_none_or(|p| !doomed.contains(p)));
    }

    /// Fold the remembered enriched metadata (real artist/duration + `yt_video_id`, and the
    /// clean catalog title) into each bare scanned song when the `video_id` matches; otherwise
    /// keep the scanned song unchanged. The scan's own `video_id`/`local_path` are kept — the
    /// on-disk scan is the source of truth for *where each file actually is* (the stored record's
    /// raw path can be stale, e.g. moved/symlinked dirs), while the manifest only lends the
    /// richer fields a bare scan can't know. A remembered record for a deleted file never matches.
    pub fn enrich(&self, scanned: Vec<Song>) -> Vec<Song> {
        let by_id: HashMap<&str, &Song> = self
            .tracks
            .iter()
            .map(|s| (s.video_id.as_str(), s))
            .collect();
        scanned
            .into_iter()
            .map(|song| match by_id.get(song.video_id.as_str()) {
                Some(rec) => Song {
                    title: rec.title.clone(),
                    artist: rec.artist.clone(),
                    artists: rec.artists.clone(),
                    duration: rec.duration.clone(),
                    album: rec.album.clone(),
                    album_artist: rec.album_artist.clone(),
                    album_artists: rec.album_artists.clone(),
                    album_release_date: rec.album_release_date.clone(),
                    album_release_date_precision: rec.album_release_date_precision.clone(),
                    album_total_tracks: rec.album_total_tracks,
                    album_type: rec.album_type.clone(),
                    album_art_url: rec.album_art_url.clone(),
                    disc_number: rec.disc_number,
                    track_number: rec.track_number,
                    explicit: rec.explicit,
                    isrc: rec.isrc.clone(),
                    origin_key: rec.origin_key.clone(),
                    origin_url: rec.origin_url.clone(),
                    import_session_id: rec.import_session_id.clone(),
                    import_source_order: rec.import_source_order,
                    duration_secs: rec.duration_secs,
                    yt_video_id: rec.yt_video_id.clone(),
                    ..song
                },
                None => song,
            })
            .collect()
    }
}

pub(crate) fn is_existing_manifest_artifact(path: &Path) -> bool {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        let Ok(current_dir) = std::env::current_dir() else {
            return false;
        };
        current_dir.join(path)
    };
    let Some(parent) = absolute.parent() else {
        return false;
    };
    // The manifest predates a persisted trusted-root identity, so it cannot safely walk and pin
    // the whole chain here. Still fail open for download replacement when either the artifact or
    // its immediate containing directory is a symlink/reparse point.
    if fs::symlink_metadata(parent)
        .map(|metadata| !metadata.is_dir() || is_link_or_reparse_point(&metadata))
        .unwrap_or(true)
    {
        return false;
    }
    safe_fs::metadata_no_symlink(&absolute).is_ok()
}

#[cfg(windows)]
fn is_link_or_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt as _;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_link_or_reparse_point(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

pub fn sidecar_path(audio_path: &Path) -> PathBuf {
    let file_name = audio_path
        .file_name()
        .map(OsString::from)
        .unwrap_or_else(|| OsString::from("track"));
    let mut sidecar_name = file_name;
    sidecar_name.push(".ytt.json");
    audio_path.with_file_name(sidecar_name)
}

pub fn write_sidecar(song: &Song, audio_path: &Path) -> io::Result<()> {
    write_sidecar_noreplace(song, audio_path)
}

/// Publish an import sidecar durably without replacing any existing foreign path. An exact
/// already-published payload is idempotent success; every other collision is preserved.
pub(crate) fn write_sidecar_noreplace(song: &Song, audio_path: &Path) -> io::Result<()> {
    let path = sidecar_path(audio_path);
    let json = sidecar_bytes(song)?;
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("sidecar path has no parent: {}", path.display()),
        )
    })?;
    let canonical_parent = fs::canonicalize(parent)?;
    let pinned = safe_fs::PinnedDir::open_existing(&canonical_parent, Path::new(""))?;
    let name = path.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "sidecar path has no basename")
    })?;
    match pinned.open_child_readonly(name) {
        Ok(existing) => return exact_sidecar_or_collision(&existing, &json, &path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let mut generation = match pinned.create_new(name) {
        Ok(generation) => generation,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let existing = pinned.open_child_readonly(name)?;
            return exact_sidecar_or_collision(&existing, &json, &path);
        }
        Err(error) => return Err(error),
    };
    generation.file_mut()?.write_all(&json)?;
    generation.sync_durable()
}

pub(crate) fn sidecar_bytes(song: &Song) -> io::Result<Vec<u8>> {
    serde_json::to_vec_pretty(&DownloadSidecar::from_song(song)).map_err(io::Error::other)
}

fn exact_sidecar_or_collision(
    existing: &safe_fs::OwnedGeneration,
    expected: &[u8],
    path: &Path,
) -> io::Result<()> {
    let metadata = existing.file()?.metadata()?;
    if metadata.len() > SIDECAR_MAX_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("refusing oversized existing sidecar {}", path.display()),
        ));
    }
    let file = existing.file()?.try_clone()?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(SIDECAR_MAX_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes == expected {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("refusing to replace foreign sidecar {}", path.display()),
        ))
    }
}

pub fn write_audio_tags(song: &Song, audio_path: &Path) -> lofty::error::Result<()> {
    write_audio_tags_with_art(song, audio_path, None)
}

pub fn write_audio_tags_with_art(
    song: &Song,
    audio_path: &Path,
    cover_art: Option<&[u8]>,
) -> lofty::error::Result<()> {
    use lofty::config::WriteOptions;
    use lofty::file::{AudioFile, TaggedFileExt};
    use lofty::picture::{Picture, PictureType};
    use lofty::probe::Probe;
    use lofty::tag::Tag;

    let mut tagged = Probe::open(audio_path)?.guess_file_type()?.read()?;
    let tag_type = tagged.primary_tag_type();
    if tagged.primary_tag_mut().is_none() {
        tagged.insert_tag(Tag::new(tag_type));
    }
    let Some(tag) = tagged.primary_tag_mut() else {
        return Ok(());
    };
    apply_song_tags(tag, song);
    if let Some(bytes) = cover_art.filter(|bytes| !bytes.is_empty()) {
        let mut reader = std::io::Cursor::new(bytes);
        let mut picture = Picture::from_reader(&mut reader)?;
        picture.set_pic_type(PictureType::CoverFront);
        tag.remove_picture_type(PictureType::CoverFront);
        tag.push_picture(picture);
    }
    tagged.save_to_path(audio_path, WriteOptions::default())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StagedAudioRewriteFaultPoint {
    BeforePublish,
    AfterPublish,
}

type StagedAudioFaultHook<'a> = &'a dyn Fn(StagedAudioRewriteFaultPoint, &Path) -> io::Result<()>;

/// Rewrite tags on a private same-directory copy, validate the rewritten container, flush it,
/// and only then atomically replace the downloaded path. A deterministic backup published from
/// the exact original handle keeps the prior bytes recoverable across the publish boundary.
pub(crate) fn write_audio_tags_staged(
    song: &Song,
    audio_path: &Path,
    cover_art: Option<&[u8]>,
) -> io::Result<()> {
    let original = safe_fs::open_regular_no_symlink(audio_path)?;
    let original_object = safe_fs::file_object_id(&original)?;
    drop(original);
    staged_audio_rewrite_with(
        audio_path,
        Some(original_object),
        |stage, _| rewrite_audio_file(song, stage, cover_art),
        |published| validate_tagged_audio_path(song, published),
        None,
    )
}

/// Copy one downloaded import object into a deterministic claim-owned generation, then rewrite
/// and validate metadata exclusively through that generation's already-open handle.
pub(crate) struct ImportAudioGeneration {
    pub(crate) path: PathBuf,
    pub(crate) source_object_id: safe_fs::FileObjectId,
}

pub(crate) fn write_import_audio_generation(
    song: &Song,
    audio_path: &Path,
    claim_id: &str,
    cover_art: Option<&[u8]>,
) -> io::Result<ImportAudioGeneration> {
    if claim_id.is_empty()
        || !claim_id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "import claim id is not safe for an owned generation name",
        ));
    }
    let (parent, source_name) = pinned_private_import_parent_and_name(audio_path)?;
    let source = parent.open_child_readonly(&source_name)?;
    let source_metadata = source.file()?.metadata()?;
    if source_metadata.len() == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "downloaded import audio is empty",
        ));
    }
    let extension = audio_path
        .extension()
        .and_then(|extension| extension.to_str())
        .filter(|extension| {
            !extension.is_empty() && extension.bytes().all(|byte| byte.is_ascii_alphanumeric())
        })
        .unwrap_or("audio");
    let generation_name = OsString::from(format!(".ytt-owned-{claim_id}.{extension}"));
    let mut generation = match parent.create_new(&generation_name) {
        Ok(generation) => generation,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            parent.open_child(&generation_name)?
        }
        Err(error) => return Err(error),
    };
    if generation.identity() == source.identity() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "claim-owned import generation aliases its raw source",
        ));
    }
    {
        let destination = generation.file_mut()?;
        destination.set_len(0)?;
        destination.seek(SeekFrom::Start(0))?;
    }
    let mut source_file = source.file()?.try_clone()?;
    source_file.seek(SeekFrom::Start(0))?;
    let copied = {
        let destination = generation.file_mut()?;
        let limit = source_metadata
            .len()
            .checked_add(1)
            .ok_or_else(|| io::Error::other("downloaded audio size cannot be bounded"))?;
        io::copy(&mut source_file.take(limit), destination)?
    };
    if copied != source_metadata.len() {
        return Err(io::Error::other(
            "downloaded audio changed while creating its owned generation",
        ));
    }
    generation.sync_durable()?;
    rewrite_open_audio_generation(song, &mut generation, cover_art)?;
    let canonical_parent = fs::canonicalize(
        audio_path
            .parent()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "audio has no parent"))?,
    )?;
    Ok(ImportAudioGeneration {
        path: canonical_parent.join(generation.basename()),
        source_object_id: source.identity(),
    })
}

fn rewrite_open_audio_generation(
    song: &Song,
    generation: &mut safe_fs::OwnedGeneration,
    cover_art: Option<&[u8]>,
) -> io::Result<()> {
    rewrite_audio_file(song, generation.file_mut()?, cover_art)?;
    generation.sync_durable()
}

fn rewrite_audio_file(song: &Song, file: &mut File, cover_art: Option<&[u8]>) -> io::Result<()> {
    use lofty::config::WriteOptions;
    use lofty::file::{AudioFile, TaggedFileExt};
    use lofty::picture::{Picture, PictureType};
    use lofty::probe::Probe;
    use lofty::tag::Tag;

    file.seek(SeekFrom::Start(0))?;
    let original_file = file.try_clone()?;
    let original = Probe::new(BufReader::new(original_file))
        .guess_file_type()?
        .read()
        .map_err(io::Error::other)?;
    let expected_type = original.file_type();
    let expected_duration = original.properties().duration();
    let mut tagged = original;
    let tag_type = tagged.primary_tag_type();
    if tagged.primary_tag_mut().is_none() {
        tagged.insert_tag(Tag::new(tag_type));
    }
    if let Some(tag) = tagged.primary_tag_mut() {
        apply_song_tags(tag, song);
        if let Some(bytes) = cover_art.filter(|bytes| !bytes.is_empty()) {
            let mut reader = std::io::Cursor::new(bytes);
            let mut picture = Picture::from_reader(&mut reader).map_err(io::Error::other)?;
            picture.set_pic_type(PictureType::CoverFront);
            tag.remove_picture_type(PictureType::CoverFront);
            tag.push_picture(picture);
        }
    }
    tagged
        .save_to(&mut *file, WriteOptions::default())
        .map_err(io::Error::other)?;
    file.sync_all()?;

    file.seek(SeekFrom::Start(0))?;
    let rewritten_file = file.try_clone()?;
    let rewritten = Probe::new(BufReader::new(rewritten_file))
        .guess_file_type()?
        .read()
        .map_err(io::Error::other)?;
    let metadata = file.metadata()?;
    if metadata.len() < MIN_TAGGED_AUDIO_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "rewritten audio is implausibly small",
        ));
    }
    if rewritten.file_type() != expected_type {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "rewritten audio format changed",
        ));
    }
    if !expected_duration.is_zero() && rewritten.properties().duration() != expected_duration {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "rewritten audio duration changed",
        ));
    }
    let tag = rewritten.primary_tag().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "rewritten audio has no primary metadata tag",
        )
    })?;
    if !tag_matches_song(tag, song) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "rewritten audio tags do not match the requested metadata",
        ));
    }
    Ok(())
}

fn validate_tagged_audio_path(song: &Song, path: &Path) -> io::Result<bool> {
    use lofty::file::TaggedFileExt as _;
    use lofty::probe::Probe;

    let file = safe_fs::open_regular_no_symlink(path)?;
    if file.metadata()?.len() < MIN_TAGGED_AUDIO_BYTES {
        return Ok(false);
    }
    let tagged = Probe::new(BufReader::new(file))
        .guess_file_type()?
        .read()
        .map_err(io::Error::other)?;
    Ok(tagged
        .primary_tag()
        .is_some_and(|tag| tag_matches_song(tag, song)))
}

fn tag_matches_song(tag: &lofty::tag::Tag, song: &Song) -> bool {
    use lofty::tag::Accessor as _;

    (song.title.trim().is_empty() || tag.title().as_deref() == Some(song.title.as_str()))
        && (song.artist.trim().is_empty() || tag.artist().as_deref() == Some(song.artist.as_str()))
        && song
            .album
            .as_deref()
            .is_none_or(|album| album.trim().is_empty() || tag.album().as_deref() == Some(album))
}

fn pinned_parent_and_name(path: &Path) -> io::Result<(safe_fs::PinnedDir, OsString)> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("artifact has no parent: {}", path.display()),
        )
    })?;
    let canonical_parent = fs::canonicalize(parent)?;
    let name = path
        .file_name()
        .map(OsString::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "artifact has no basename"))?;
    let pinned = safe_fs::PinnedDir::open_existing(&canonical_parent, Path::new(""))?;
    Ok((pinned, name))
}

fn pinned_private_import_parent_and_name(
    path: &Path,
) -> io::Result<(safe_fs::PinnedDir, OsString)> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("import artifact has no parent: {}", path.display()),
        )
    })?;
    let canonical_parent = fs::canonicalize(parent)?;
    let private_root = canonical_parent
        .ancestors()
        .find(|ancestor| {
            ancestor
                .file_name()
                .is_some_and(|name| name == ".yututui-inbox")
        })
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "import artifact is outside the private inbox namespace",
            )
        })?;
    let relative = canonical_parent
        .strip_prefix(private_root)
        .expect("selected ancestor prefixes canonical import parent");
    let pinned = safe_fs::PinnedDir::open_private_existing(private_root, relative)?;
    let name = path
        .file_name()
        .map(OsString::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "artifact has no basename"))?;
    Ok((pinned, name))
}
fn staged_audio_backup_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("download.audio");
    path.with_file_name(format!(".{name}.ytt-tag-backup"))
}

fn staged_audio_rewrite_with<R, V>(
    audio_path: &Path,
    expected_object: Option<safe_fs::FileObjectId>,
    rewrite_and_validate: R,
    validate_published: V,
    fault: Option<StagedAudioFaultHook<'_>>,
) -> io::Result<()>
where
    R: Fn(&mut File, &Path) -> io::Result<()>,
    V: Fn(&Path) -> io::Result<bool>,
{
    let backup = staged_audio_backup_path(audio_path);
    reconcile_staged_audio_backup(audio_path, &backup, &validate_published)?;

    let (parent, original_name) = pinned_parent_and_name(audio_path)?;
    let original = parent.open_child_readonly(&original_name)?;
    let original_object = original.identity();
    if expected_object.is_some_and(|expected| expected != original_object) {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "downloaded audio changed before staging: {}",
                audio_path.display()
            ),
        ));
    }
    let initial = original.file()?.metadata()?;
    if initial.len() == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("downloaded audio is empty: {}", audio_path.display()),
        ));
    }
    let (stage, mut stage_file) = safe_fs::create_private_temp_for(audio_path)?;
    let result = (|| {
        let mut original_file = original.file()?.try_clone()?;
        let copied = copy_exact_snapshot(&mut original_file, &mut stage_file, initial.len())?;
        if copied != initial.len() {
            return Err(io::Error::other("downloaded audio changed while staged"));
        }
        stage_file.flush()?;
        stage_file.sync_all()?;
        let final_source = original.file()?.metadata()?;
        let modified_changed = match (initial.modified(), final_source.modified()) {
            (Ok(initial), Ok(final_source)) => initial != final_source,
            _ => false,
        };
        if final_source.len() != initial.len() || modified_changed {
            return Err(io::Error::other("downloaded audio changed while staged"));
        }

        rewrite_and_validate(&mut stage_file, &stage)?;
        stage_file.flush()?;
        stage_file.sync_all()?;

        let backup_name = backup
            .file_name()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "backup has no basename"))?;
        #[cfg(not(windows))]
        let backup_generation = original.publish_noreplace(&parent, backup_name)?;
        #[cfg(windows)]
        let backup_generation = {
            // A second hard link to the live destination prevents MoveFileExW from replacing its
            // original name on Windows. Preserve the exact validated bytes in an independent,
            // exclusive generation instead.
            let mut backup = parent.create_new(backup_name)?;
            let mut source = original.file()?.try_clone()?;
            source.seek(SeekFrom::Start(0))?;
            let copied = copy_exact_snapshot(&mut source, backup.file_mut()?, initial.len())?;
            if copied != initial.len() {
                return Err(io::Error::other(
                    "downloaded audio changed while creating its recovery backup",
                ));
            }
            backup.sync_durable()?;
            backup
        };
        if let Some(fault) = fault {
            fault(StagedAudioRewriteFaultPoint::BeforePublish, &stage)?;
        }
        let current = safe_fs::open_regular_no_symlink(audio_path)?;
        if safe_fs::file_object_id(&current)? != original_object {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "downloaded audio was replaced before tagged publish: {}",
                    audio_path.display()
                ),
            ));
        }
        let current_stage = safe_fs::open_regular_no_symlink(&stage)?;
        if safe_fs::file_object_id(&current_stage)? != safe_fs::file_object_id(&stage_file)? {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "audio staging name no longer binds the rewritten generation",
            ));
        }
        // Windows cannot replace a destination while these validation handles still reference
        // its current generation, even though every open permits delete sharing. The durable
        // backup name already retains that exact object for recovery.
        drop(current_stage);
        drop(current);
        drop(original);
        drop(backup_generation);
        drop(stage_file);

        safe_fs::atomic_replace(&stage, audio_path)?;
        safe_fs::sync_parent_dir(audio_path)?;
        if let Some(fault) = fault {
            fault(StagedAudioRewriteFaultPoint::AfterPublish, audio_path)?;
        }
        if !validate_published(audio_path)? {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "published audio failed validation: {}",
                    audio_path.display()
                ),
            ));
        }
        safe_fs::remove_private_file_durable(&backup)
    })();

    if result.is_err() {
        let _ = fs::remove_file(&stage);
        let _ = safe_fs::sync_parent_dir(&stage);
    }
    result
}

fn copy_exact_snapshot(source: &mut File, destination: &mut File, len: u64) -> io::Result<u64> {
    let limit = len
        .checked_add(1)
        .ok_or_else(|| io::Error::other("downloaded audio size cannot be bounded"))?;
    let mut bounded = source.take(limit);
    io::copy(&mut bounded, destination)
}

fn reconcile_staged_audio_backup<V>(
    audio_path: &Path,
    backup: &Path,
    validate_published: &V,
) -> io::Result<()>
where
    V: Fn(&Path) -> io::Result<bool>,
{
    let backup_file = match safe_fs::open_regular_no_symlink(backup) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let current = safe_fs::open_regular_no_symlink(audio_path)?;
    if safe_fs::file_object_id(&backup_file)? == safe_fs::file_object_id(&current)?
        || validate_published(audio_path)?
    {
        return safe_fs::remove_private_file_durable(backup);
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!(
            "preserving prior audio backup after an unverified path replacement: {}",
            backup.display()
        ),
    ))
}

pub fn read_sidecar(audio_path: &Path) -> io::Result<Option<DownloadSidecar>> {
    let path = sidecar_path(audio_path);
    let bytes = match safe_fs::read_no_symlink_limited(&path, SIDECAR_MAX_BYTES) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let sidecar: DownloadSidecar = serde_json::from_slice(&bytes)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if !(1..=SIDECAR_SCHEMA_VERSION).contains(&sidecar.schema_version) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "download sidecar schema version {} is not supported",
                sidecar.schema_version
            ),
        ));
    }
    Ok(Some(sidecar))
}

fn apply_song_tags(tag: &mut lofty::tag::Tag, song: &Song) {
    use lofty::tag::{Accessor, ItemKey};

    if !song.title.trim().is_empty() {
        tag.set_title(song.title.clone());
    }
    if !song.artist.trim().is_empty() {
        tag.set_artist(song.artist.clone());
        let artists = if song.artists.is_empty() {
            song.artist.clone()
        } else {
            song.artists.join("; ")
        };
        tag.insert_text(ItemKey::TrackArtists, artists);
    }
    if let Some(album) = song
        .album
        .as_deref()
        .filter(|album| !album.trim().is_empty())
    {
        tag.set_album(album.to_owned());
    }
    if let Some(album_artist) = song
        .album_artist
        .as_deref()
        .filter(|album_artist| !album_artist.trim().is_empty())
    {
        tag.insert_text(ItemKey::AlbumArtist, album_artist.to_owned());
        let album_artists = if song.album_artists.is_empty() {
            album_artist.to_owned()
        } else {
            song.album_artists.join("; ")
        };
        tag.insert_text(ItemKey::AlbumArtists, album_artists);
    }
    if let Some(track_number) = song.track_number {
        tag.set_track(track_number);
    }
    if let Some(track_total) = song.album_total_tracks {
        tag.insert_text(ItemKey::TrackTotal, track_total.to_string());
    }
    if let Some(disc_number) = song.disc_number {
        tag.set_disk(disc_number);
    }
    if let Some(release_date) = song
        .album_release_date
        .as_deref()
        .filter(|date| !date.trim().is_empty())
    {
        tag.insert_text(ItemKey::RecordingDate, release_date.to_owned());
        tag.insert_text(ItemKey::ReleaseDate, release_date.to_owned());
        if let Some(year) = release_date
            .get(..4)
            .filter(|year| year.bytes().all(|b| b.is_ascii_digit()))
        {
            tag.insert_text(ItemKey::Year, year.to_owned());
        }
    }
    if let Some(isrc) = song.isrc.as_deref().filter(|isrc| !isrc.trim().is_empty()) {
        tag.insert_text(ItemKey::Isrc, isrc.to_owned());
    }
    if let Some(explicit) = song.explicit {
        tag.insert_text(
            ItemKey::ParentalAdvisory,
            if explicit { "1" } else { "0" }.to_owned(),
        );
    }
    let mut comment = Vec::new();
    if let Some(url) = song.share_url() {
        comment.push(format!("YouTube: {url}"));
    }
    if let Some(url) = song
        .origin_url
        .as_deref()
        .filter(|url| !url.trim().is_empty())
    {
        comment.push(format!("Origin: {url}"));
    }
    if let Some(album_art_url) = song
        .album_art_url
        .as_deref()
        .filter(|url| !url.trim().is_empty())
    {
        comment.push(format!("Album art: {album_art_url}"));
    }
    if !comment.is_empty() {
        tag.insert_text(ItemKey::Comment, comment.join("\n"));
    }
}

pub(crate) fn store_path() -> Option<PathBuf> {
    crate::paths::data_dir().map(|d| d.join("downloads.json"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn downloaded(path: &str, yt_id: &str) -> Song {
        Song::remote(yt_id, "Title", "Real Artist", "3:00").with_local_path(PathBuf::from(path))
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let mut bytes = [0u8; 8];
        getrandom::fill(&mut bytes).unwrap();
        let suffix = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
        std::env::temp_dir().join(format!(
            "yututui-downloads-{tag}-{}-{suffix}",
            std::process::id()
        ))
    }

    fn overwrite_open(file: &mut File, bytes: &[u8]) -> io::Result<()> {
        file.set_len(0)?;
        std::io::Seek::seek(file, SeekFrom::Start(0))?;
        std::io::Write::write_all(file, bytes)
    }

    fn wav_bytes() -> Vec<u8> {
        let samples = vec![128_u8; 800];
        let mut bytes = Vec::with_capacity(44 + samples.len());
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36_u32 + samples.len() as u32).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16_u32.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&8_000_u32.to_le_bytes());
        bytes.extend_from_slice(&8_000_u32.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&8_u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&(samples.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&samples);
        bytes
    }

    #[test]
    fn enrich_restores_id_and_artist_by_video_id() {
        let mut store = DownloadStore::default();
        let rich = downloaded("/tmp/Title [dQw4w9WgXcQ].m4a", "dQw4w9WgXcQ");
        store.record(&rich);

        // A bare scanned song with the SAME local video_id but no metadata.
        let scanned = Song::local_file(PathBuf::from("/tmp/plain.m4a"));
        let scanned = Song {
            video_id: rich.video_id.clone(),
            ..scanned
        };
        let out = store.enrich(vec![scanned]);
        assert_eq!(out[0].artist, "Real Artist");
        assert_eq!(out[0].youtube_id(), Some("dQw4w9WgXcQ"));
        assert!(out[0].share_url().is_some());
    }

    #[test]
    fn enrich_keeps_scanned_local_path_over_stored() {
        // The manifest's stored raw path can go stale (file moved, dir re-symlinked) while the
        // canonical `video_id` still agrees. Enrich must take metadata from the manifest but keep
        // the scan's on-disk `local_path` so playback points at where the file actually is.
        let mut store = DownloadStore::default();
        let rich = downloaded("/tmp/stale/Title [dQw4w9WgXcQ].m4a", "dQw4w9WgXcQ");
        store.record(&rich);

        let scanned = Song::local_file(PathBuf::from("/tmp/fresh/plain.m4a"));
        let scanned = Song {
            video_id: rich.video_id.clone(),
            ..scanned
        };
        let out = store.enrich(vec![scanned]);
        assert_eq!(out[0].artist, "Real Artist");
        assert_eq!(out[0].youtube_id(), Some("dQw4w9WgXcQ"));
        assert_eq!(
            out[0].local_path,
            Some(PathBuf::from("/tmp/fresh/plain.m4a"))
        );
    }

    #[test]
    fn enrich_keeps_unknown_scanned_songs() {
        let store = DownloadStore::default();
        let scanned = Song::local_file(PathBuf::from("/tmp/unknown.m4a"));
        let out = store.enrich(vec![scanned.clone()]);
        assert_eq!(out[0].video_id, scanned.video_id);
        assert_eq!(out[0].artist, "Local file");
    }

    #[test]
    fn record_dedups_by_video_id_newest_first() {
        let mut store = DownloadStore::default();
        let a = downloaded("/tmp/a.m4a", "aaaaaaaaaaa");
        let b = downloaded("/tmp/b.m4a", "bbbbbbbbbbb");
        store.record(&a);
        store.record(&b);
        store.record(&a); // re-record a -> back to front, no dupe
        assert_eq!(store.tracks.len(), 2);
        assert_eq!(store.tracks[0].video_id, a.video_id);
    }

    #[test]
    fn remove_paths_drops_matching_records() {
        let mut store = DownloadStore::default();
        let a = downloaded("/tmp/a.m4a", "aaaaaaaaaaa");
        store.record(&a);
        store.remove_paths(&[PathBuf::from("/tmp/a.m4a")]);
        assert!(store.tracks.is_empty());
    }

    #[test]
    fn missing_manifest_artifact_does_not_suppress_a_replacement_download() {
        let dir = temp_dir("stale-dedupe");
        fs::create_dir_all(&dir).unwrap();
        let audio = dir.join("Track [aaaaaaaaaaa].m4a");
        fs::write(&audio, b"audio").unwrap();
        let mut store = DownloadStore::default();
        store.record(&downloaded(audio.to_str().unwrap(), "aaaaaaaaaaa"));

        assert!(store.contains_youtube_id("aaaaaaaaaaa"));
        fs::remove_file(&audio).unwrap();
        assert!(!store.contains_youtube_id("aaaaaaaaaaa"));
        assert_eq!(
            store.tracks().len(),
            1,
            "metadata stays available to enrich"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn replaced_symlink_ancestor_does_not_suppress_a_replacement_download() {
        use std::os::unix::fs::symlink;

        let dir = temp_dir("stale-dedupe-ancestor");
        let original_parent = dir.join("library");
        let moved_parent = dir.join("moved-library");
        fs::create_dir_all(&original_parent).unwrap();
        let audio = original_parent.join("Track [aaaaaaaaaaa].m4a");
        fs::write(&audio, b"audio").unwrap();
        let mut store = DownloadStore::default();
        store.record(&downloaded(audio.to_str().unwrap(), "aaaaaaaaaaa"));
        assert!(store.contains_youtube_id("aaaaaaaaaaa"));

        fs::rename(&original_parent, &moved_parent).unwrap();
        symlink(&moved_parent, &original_parent).unwrap();
        assert!(
            audio.is_file(),
            "the stale path still resolves through the link"
        );
        assert!(!store.contains_youtube_id("aaaaaaaaaaa"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn json_round_trips_and_missing_defaults_empty() {
        let mut store = DownloadStore::default();
        store.record(&downloaded("/tmp/a.m4a", "aaaaaaaaaaa"));
        let s = serde_json::to_string(&store).unwrap();
        let back: DownloadStore = serde_json::from_str(&s).unwrap();
        assert_eq!(back.tracks.len(), 1);
        let empty: DownloadStore = serde_json::from_str("{}").unwrap();
        assert!(empty.tracks.is_empty());
    }

    #[test]
    fn sidecar_path_appends_to_audio_file_name() {
        assert_eq!(
            sidecar_path(&PathBuf::from("/tmp/Track.m4a")),
            PathBuf::from("/tmp/Track.m4a.ytt.json")
        );
    }

    #[test]
    fn sidecar_round_trips_download_metadata() {
        let dir = temp_dir("sidecar");
        fs::create_dir_all(&dir).unwrap();
        let audio = dir.join("Track [abc123def45].m4a");
        let mut song = Song::from_search(
            "abc123def45",
            "Track",
            "Artist A, Artist B",
            "3:03",
            Some("Album".to_owned()),
        );
        song.duration_secs = Some(183);
        song = song.with_catalog_metadata(
            Some("Album Artist".to_owned()),
            Some(1),
            Some(4),
            Some("ISRC123".to_owned()),
            Some("spotify:track:abc".to_owned()),
            Some("https://open.spotify.com/track/abc".to_owned()),
        );
        song = song.with_import_session(Some("sp2yt-20260708-abcd".to_owned()), Some(7));

        write_sidecar(&song, &audio).unwrap();
        let sidecar = read_sidecar(&audio).unwrap().expect("sidecar exists");

        assert_eq!(sidecar.video_id, "abc123def45");
        assert_eq!(sidecar.linked_youtube_id(), Some("abc123def45"));
        assert_eq!(sidecar.title, "Track");
        assert_eq!(sidecar.artist, "Artist A, Artist B");
        assert_eq!(sidecar.album.as_deref(), Some("Album"));
        assert_eq!(sidecar.album_artist.as_deref(), Some("Album Artist"));
        assert_eq!(sidecar.disc_number, Some(1));
        assert_eq!(sidecar.track_number, Some(4));
        assert_eq!(sidecar.duration_secs, Some(183));
        assert_eq!(sidecar.isrc.as_deref(), Some("ISRC123"));
        assert_eq!(sidecar.origin_key.as_deref(), Some("spotify:track:abc"));
        assert_eq!(
            sidecar.origin_url.as_deref(),
            Some("https://open.spotify.com/track/abc")
        );
        assert_eq!(
            sidecar.import_session_id.as_deref(),
            Some("sp2yt-20260708-abcd")
        );
        assert_eq!(sidecar.import_source_order, Some(7));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn tag_writer_populates_common_catalog_fields() {
        use lofty::tag::{Accessor, ItemKey, Tag, TagType};

        let mut song = Song::from_search(
            "abc123def45",
            "Track",
            "Artist A, Artist B",
            "3:03",
            Some("Album".to_owned()),
        );
        song.duration_secs = Some(183);
        song = song.with_catalog_metadata(
            Some("Album Artist".to_owned()),
            Some(1),
            Some(4),
            Some("ISRC123".to_owned()),
            Some("spotify:track:abc".to_owned()),
            Some("https://open.spotify.com/track/abc".to_owned()),
        );
        let mut tag = Tag::new(TagType::Mp4Ilst);

        apply_song_tags(&mut tag, &song);

        assert_eq!(tag.title().as_deref(), Some("Track"));
        assert_eq!(tag.artist().as_deref(), Some("Artist A, Artist B"));
        assert_eq!(tag.album().as_deref(), Some("Album"));
        assert_eq!(tag.get_string(ItemKey::AlbumArtist), Some("Album Artist"));
        assert_eq!(tag.get_string(ItemKey::TrackNumber), Some("4"));
        assert_eq!(tag.get_string(ItemKey::DiscNumber), Some("1"));
        assert_eq!(tag.get_string(ItemKey::Isrc), Some("ISRC123"));
        assert_eq!(
            tag.get_string(ItemKey::Comment),
            Some(
                "YouTube: https://www.youtube.com/watch?v=abc123def45\nOrigin: https://open.spotify.com/track/abc"
            )
        );
    }

    #[test]
    fn production_staged_tag_writer_replaces_only_after_valid_rewrite() {
        let dir = temp_dir("production-staged-tags");
        fs::create_dir_all(&dir).unwrap();
        let audio = dir.join("Track.wav");
        fs::write(&audio, wav_bytes()).unwrap();
        let original = safe_fs::open_regular_no_symlink(&audio).unwrap();
        let original_object = safe_fs::file_object_id(&original).unwrap();
        drop(original);
        let song = Song::remote("abc123def45", "Staged Track", "Staged Artist", "0:00");

        write_audio_tags_staged(&song, &audio, None).unwrap();

        let published = safe_fs::open_regular_no_symlink(&audio).unwrap();
        assert_ne!(
            safe_fs::file_object_id(&published).unwrap(),
            original_object,
            "tagging must publish a distinct generation"
        );
        assert!(validate_tagged_audio_path(&song, &audio).unwrap());
        assert!(!staged_audio_backup_path(&audio).exists());
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn staged_rewrite_enospc_preserves_original_and_cleans_stage() {
        let dir = temp_dir("staged-enospc");
        fs::create_dir_all(&dir).unwrap();
        let audio = dir.join("Track.audio");
        let original = b"original downloaded bytes remain intact";
        fs::write(&audio, original).unwrap();

        let error = staged_audio_rewrite_with(
            &audio,
            None,
            |stage, _| {
                overwrite_open(stage, b"partial")?;
                Err(io::Error::from_raw_os_error(libc::ENOSPC))
            },
            |_| Ok(false),
            None,
        )
        .expect_err("injected disk exhaustion must abort publication");

        assert_eq!(error.raw_os_error(), Some(libc::ENOSPC));
        assert_eq!(fs::read(&audio).unwrap(), original);
        assert!(!staged_audio_backup_path(&audio).exists());
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn staged_rewrite_never_overwrites_a_foreign_publish_race() {
        let dir = temp_dir("staged-race");
        fs::create_dir_all(&dir).unwrap();
        let audio = dir.join("Track.audio");
        let original = b"original downloaded bytes remain intact";
        fs::write(&audio, original).unwrap();
        let hook_audio = audio.clone();
        let hook = move |point, _stage: &Path| {
            if point == StagedAudioRewriteFaultPoint::BeforePublish {
                fs::remove_file(&hook_audio)?;
                fs::write(&hook_audio, b"foreign sentinel")?;
            }
            Ok(())
        };

        let error = staged_audio_rewrite_with(
            &audio,
            None,
            |stage, _| overwrite_open(stage, b"complete rewritten audio bytes"),
            |published| Ok(fs::read(published)? == b"complete rewritten audio bytes"),
            Some(&hook),
        )
        .expect_err("foreign path replacement must stop publication");

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&audio).unwrap(), b"foreign sentinel");
        assert_eq!(
            fs::read(staged_audio_backup_path(&audio)).unwrap(),
            original
        );
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 2);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn staged_rewrite_after_publish_fault_recovers_without_another_download() {
        let dir = temp_dir("staged-retry");
        fs::create_dir_all(&dir).unwrap();
        let audio = dir.join("Track.audio");
        fs::write(&audio, b"old valid audio bytes").unwrap();
        let hook = |point, _path: &Path| {
            if point == StagedAudioRewriteFaultPoint::AfterPublish {
                return Err(io::Error::other("crash after publish"));
            }
            Ok(())
        };
        let rewrite = |stage: &mut File, _path: &Path| {
            overwrite_open(stage, b"new valid audio bytes after tagging")
        };
        let validate =
            |published: &Path| Ok(fs::read(published)? == b"new valid audio bytes after tagging");

        staged_audio_rewrite_with(&audio, None, rewrite, validate, Some(&hook))
            .expect_err("post-publish crash must retain recovery state");
        assert_eq!(
            fs::read(&audio).unwrap(),
            b"new valid audio bytes after tagging"
        );
        assert!(staged_audio_backup_path(&audio).is_file());

        staged_audio_rewrite_with(&audio, None, rewrite, validate, None)
            .expect("retry should recognize published bytes and converge");
        assert_eq!(
            fs::read(&audio).unwrap(),
            b"new valid audio bytes after tagging"
        );
        assert!(!staged_audio_backup_path(&audio).exists());
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        let _ = fs::remove_dir_all(dir);
    }
}
