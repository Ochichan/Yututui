//! Media-session artwork: async fetch → square-crop → disk cache.
//!
//! Windows (SMTC) and macOS (Now Playing) consume artwork as local data, so remote
//! thumbnails are cached on disk once per track and re-used across sessions; Linux
//! (MPRIS) prefers the cached `file://` URI and falls back to the remote URL while
//! the fetch is in flight. Independent of the TUI's opt-in album-art feature — the
//! OS widget gets art even when in-terminal art is off (and in the headless daemon).
//!
//! YouTube thumbnails are 4:3/16:9 frames with the square release art letterboxed in
//! the middle, so the center square crop recovers the actual cover for catalog
//! tracks; local files use their embedded tag art as-is (cropped the same way).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use tokio::sync::mpsc;

use crate::util::http;

/// Longest side of the cached image (spec AW-2: request ~512² for OS surfaces).
const MAX_DIM: u32 = 512;
/// Remote fetch guard-rails (spec AW-4): per-attempt timeout and one retry.
const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const FETCH_ATTEMPTS: usize = 2;
const REMOTE_ART_MAX_BYTES: usize = 5 * 1024 * 1024;
/// Cache bounds (spec AW-5), pruned oldest-first by mtime.
const CACHE_MAX_FILES: usize = 500;
const CACHE_MAX_BYTES: u64 = 100 * 1024 * 1024;

/// Where a track's media-session art comes from.
#[derive(Debug, Clone, PartialEq)]
pub enum ArtQuery {
    /// Catalog track: fetch `i.ytimg.com/vi/<id>/…` (maxres first, hq fallback).
    Youtube { id: String },
    /// Local file: read the embedded cover art out of its tags.
    LocalFile(PathBuf),
}

/// A finished cache entry, delivered back to the core as a message so the next
/// snapshot carries `art_file` (single-direction data flow, spec A-4).
#[derive(Debug, Clone, PartialEq)]
pub struct MediaArtworkReady {
    /// The track key (`video_id`) the art belongs to.
    pub key: String,
    pub path: PathBuf,
}

/// The remote thumbnail URL MPRIS clients can use before the disk cache lands.
pub fn remote_thumbnail_url(youtube_id: &str) -> String {
    format!("https://i.ytimg.com/vi/{youtube_id}/hqdefault.jpg")
}

/// Handle to the artwork cache actor. Requests are deduplicated per track key and
/// results (cache hits included) come back through the `emit` sink given at spawn.
pub struct ArtworkCache {
    tx: mpsc::UnboundedSender<(String, ArtQuery)>,
}

impl ArtworkCache {
    pub fn spawn<F>(emit: F) -> Self
    where
        F: Fn(MediaArtworkReady) + Send + Sync + 'static,
    {
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(run_actor(rx, emit));
        Self { tx }
    }

    pub fn request(&self, key: String, query: ArtQuery) {
        let _ = self.tx.send((key, query));
    }
}

async fn run_actor<F>(mut rx: mpsc::UnboundedReceiver<(String, ArtQuery)>, emit: F)
where
    F: Fn(MediaArtworkReady) + Send + Sync + 'static,
{
    let Some(dir) = cache_dir() else {
        tracing::warn!("media artwork cache disabled: no cache directory");
        return;
    };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!(error = %e, "media artwork cache disabled: could not create cache dir");
        return;
    }
    let client = reqwest::Client::builder()
        .user_agent("ytm-tui/1 (https://github.com/Ochichan/ytm-tui)")
        .timeout(FETCH_TIMEOUT)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    // Tracks that failed this session, so a stuck thumbnail isn't re-fetched on
    // every publish of the same track (spec AW-4: fail → show without artwork).
    let mut failed: HashSet<String> = HashSet::new();

    // Sequential, like the in-TUI artwork actor: rapid skips queue up, the facade
    // drops stale results by key, and only one decode is in flight at a time.
    while let Some((key, query)) = rx.recv().await {
        let path = dir.join(cache_file_name(&key));
        if path.is_file() {
            emit(MediaArtworkReady { key, path });
            continue;
        }
        if failed.contains(&key) {
            continue;
        }
        let bytes = match &query {
            ArtQuery::Youtube { id } => fetch_remote(&client, id).await,
            ArtQuery::LocalFile(file) => fetch_local(file.clone()).await,
        };
        let stored = match bytes {
            Some(bytes) => store_processed(bytes, &path).await,
            None => false,
        };
        if stored {
            prune_cache(&dir);
            tracing::debug!(key = %key, path = %path.display(), "media artwork cached");
            emit(MediaArtworkReady { key, path });
        } else {
            tracing::debug!(key = %key, "media artwork unavailable");
            failed.insert(key);
        }
    }
}

fn cache_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "ytm-tui").map(|dirs| dirs.cache_dir().join("media-art"))
}

/// A filesystem-safe cache file name for a track key. YouTube ids are already safe
/// (`[A-Za-z0-9_-]`); anything else (e.g. `local:<path>` keys) is hashed.
pub(crate) fn cache_file_name(key: &str) -> String {
    let safe = !key.is_empty()
        && key.len() <= 64
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if safe {
        format!("{key}.jpg")
    } else {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        key.hash(&mut hasher);
        format!("h{:016x}.jpg", hasher.finish())
    }
}

/// Fetch the YouTube thumbnail: `maxresdefault` (720² art region) first, then the
/// always-present `hqdefault`. One retry per quality (spec AW-4).
async fn fetch_remote(client: &reqwest::Client, video_id: &str) -> Option<Vec<u8>> {
    for quality in ["maxresdefault", "hqdefault"] {
        let url = format!("https://i.ytimg.com/vi/{video_id}/{quality}.jpg");
        for _ in 0..FETCH_ATTEMPTS {
            if let Ok(resp) = client.get(&url).send().await
                && let Ok(resp) = resp.error_for_status()
                && let Ok(bytes) = http::read_response_limited(resp, REMOTE_ART_MAX_BYTES).await
                && !bytes.is_empty()
            {
                return Some(bytes);
            }
        }
    }
    None
}

/// Read embedded cover art from a local audio file (off-thread; lofty parses tags).
async fn fetch_local(path: PathBuf) -> Option<Vec<u8>> {
    tokio::task::spawn_blocking(move || {
        use lofty::file::TaggedFileExt;
        let tagged = lofty::read_from_path(&path).ok()?;
        let tag = tagged.primary_tag().or_else(|| tagged.first_tag())?;
        let pic = tag.pictures().first()?;
        Some(pic.data().to_vec())
    })
    .await
    .ok()
    .flatten()
}

/// Decode, center-crop to a square, downscale to [`MAX_DIM`], and write as JPEG
/// (temp file + rename so a crash never leaves a truncated cache entry).
async fn store_processed(bytes: Vec<u8>, path: &Path) -> bool {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let img = image::load_from_memory(&bytes).ok()?;
        let side = img.width().min(img.height());
        if side == 0 {
            return None;
        }
        let x = (img.width() - side) / 2;
        let y = (img.height() - side) / 2;
        let mut img = img.crop_imm(x, y, side, side);
        if side > MAX_DIM {
            img = img.resize(MAX_DIM, MAX_DIM, image::imageops::FilterType::Triangle);
        }
        // JPEG can't encode alpha; thumbnails are photos, so RGB8 is lossless enough.
        let rgb = img.to_rgb8();
        let mut out = Vec::new();
        let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 85);
        rgb.write_with_encoder(encoder).ok()?;
        let tmp = path.with_extension("jpg.tmp");
        std::fs::write(&tmp, &out).ok()?;
        std::fs::rename(&tmp, &path).ok()?;
        Some(())
    })
    .await
    .ok()
    .flatten()
    .is_some()
}

/// Bound the cache (spec AW-5): keep at most [`CACHE_MAX_FILES`] /
/// [`CACHE_MAX_BYTES`], evicting oldest-by-mtime first.
fn prune_cache(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(PathBuf, std::time::SystemTime, u64)> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let meta = entry.metadata().ok()?;
            if !meta.is_file() {
                return None;
            }
            let mtime = meta.modified().ok()?;
            Some((path, mtime, meta.len()))
        })
        .collect();
    let total: u64 = files.iter().map(|(_, _, len)| len).sum();
    if files.len() <= CACHE_MAX_FILES && total <= CACHE_MAX_BYTES {
        return;
    }
    files.sort_by_key(|(_, mtime, _)| *mtime);
    let mut count = files.len();
    let mut bytes = total;
    for (path, _, len) in files {
        if count <= CACHE_MAX_FILES && bytes <= CACHE_MAX_BYTES {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            count -= 1;
            bytes = bytes.saturating_sub(len);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn youtube_ids_keep_their_name() {
        assert_eq!(cache_file_name("dQw4w9WgXcQ"), "dQw4w9WgXcQ.jpg");
        assert_eq!(cache_file_name("a-b_c"), "a-b_c.jpg");
    }

    #[test]
    fn unsafe_keys_are_hashed() {
        let name = cache_file_name("local:/home/user/음악/track.m4a");
        assert!(name.starts_with('h') && name.ends_with(".jpg"), "{name}");
        assert!(!name.contains('/'));
        // Stable: the same key always maps to the same file.
        assert_eq!(name, cache_file_name("local:/home/user/음악/track.m4a"));
    }

    #[test]
    fn remote_url_uses_hqdefault() {
        assert_eq!(
            remote_thumbnail_url("abc"),
            "https://i.ytimg.com/vi/abc/hqdefault.jpg"
        );
    }
}
