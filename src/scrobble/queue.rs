//! The durable offline scrobble queue: `<data dir>/scrobble-queue.jsonl`.
//!
//! Crash-safety contract: a [`QueueEntry`] is **appended the moment the monitor decides a
//! listen counts** (threshold crossing), before any network attempt. The flusher then
//! drains per service and compacts — a full atomic rewrite that strips acknowledged
//! services from `pending` and drops delivered entries. The duplicate-on-crash window is
//! therefore just the gap between a successful submit and the rename, the same
//! best-effort standard every desktop scrobbler accepts.
//!
//! Single-writer story: the app's single-instance guard means one playback-owning process
//! at a time, so appends and compactions normally have one owner. The `--new-instance`
//! escape hatch can create more; appends stay safe (O_APPEND single lines) and compaction
//! races are prevented by a sibling `.lock` file (O_EXCL, stale after 10 minutes) — a
//! flusher that can't take the lock just skips its round.

use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::service::{ScrobbleTrack, ServiceKind};
use crate::util::safe_fs;

/// Compaction keeps at most this many entries (newest first) — a two-week offline stretch
/// of heavy listening fits comfortably; beyond that the oldest listens are the right loss.
pub const QUEUE_CAP: usize = 2000;
/// Last.fm silently ignores scrobbles older than two weeks; stop owing it those. Other
/// services (ListenBrainz imports) accept arbitrary ages and keep their markers.
const LASTFM_MAX_AGE: Duration = Duration::from_secs(14 * 24 * 3600);
/// A `.lock` older than this belongs to a dead process and is reclaimed.
const LOCK_STALE: Duration = Duration::from_secs(600);
/// Queue reads cap at this size; the CAP-compaction keeps real files far below it.
const QUEUE_READ_MAX: u64 = 4 * 1024 * 1024;
static ENTRY_SEQ: AtomicU64 = AtomicU64::new(1);
static BOOT_NONCE: OnceLock<String> = OnceLock::new();

/// One queued listen. The field names are a stable on-disk format (JSONL, one per line).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueueEntry {
    /// Unique per listen, stable across reloads (dedupe key). New entries use
    /// `"{ts}-{boot nonce}-{monotonic seq}"`; old JSONL used `"{ts}-{track key}"`.
    pub id: String,
    /// Stable track identity. Added after the original id format; old entries derive this from id.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub track_key: String,
    /// Listen start, unix seconds (the scrobble timestamp).
    pub ts: i64,
    pub artist: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_url: Option<String>,
    /// Services that still owe this listen a delivery.
    pub pending: Vec<ServiceKind>,
}

impl QueueEntry {
    pub fn from_track(track: &ScrobbleTrack, pending: Vec<ServiceKind>) -> Self {
        Self {
            id: next_entry_id(track.started_unix),
            track_key: track.key.clone(),
            ts: track.started_unix,
            artist: track.artist.clone(),
            title: track.title.clone(),
            album: track.album.clone(),
            duration: track.duration_secs,
            origin_url: track.origin_url.clone(),
            pending,
        }
    }

    pub fn to_track(&self) -> ScrobbleTrack {
        ScrobbleTrack {
            key: if self.track_key.is_empty() {
                legacy_track_key_from_id(&self.id)
            } else {
                self.track_key.clone()
            },
            artist: self.artist.clone(),
            title: self.title.clone(),
            album: self.album.clone(),
            duration_secs: self.duration,
            origin_url: self.origin_url.clone(),
            started_unix: self.ts,
        }
    }
}

fn next_entry_id(started_unix: i64) -> String {
    let seq = ENTRY_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{started_unix}-{}-{seq}", boot_nonce())
}

fn boot_nonce() -> &'static str {
    BOOT_NONCE.get_or_init(|| {
        let mut bytes = [0u8; 8];
        if getrandom::fill(&mut bytes).is_err() {
            let fallback = format!(
                "{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or_default()
            );
            return fallback;
        }
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    })
}

fn legacy_track_key_from_id(id: &str) -> String {
    id.split_once('-')
        .map(|(_, k)| k.to_owned())
        .unwrap_or_else(|| id.to_owned())
}

/// What [`QueueFile::load`] found: parsed entries (id-deduped, keep-first) plus how many
/// lines were corrupt (skipped, never fatal — one mangled line must not strand the rest).
#[derive(Debug, Default)]
pub struct LoadedQueue {
    pub entries: Vec<QueueEntry>,
    pub corrupt: usize,
    /// The file was present but could not be read (oversize, permission, bad bytes). The
    /// flusher must treat this as "unknown", NOT as an empty queue — compacting an unknown
    /// queue to nothing would delete a queue we simply failed to read.
    pub read_failed: bool,
}

pub struct QueueFile {
    path: PathBuf,
}

impl QueueFile {
    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    /// The production location, following the other data-dir stores.
    pub fn default_path() -> Option<PathBuf> {
        directories::ProjectDirs::from("", "", "ytm-tui")
            .map(|d| d.data_dir().join("scrobble-queue.jsonl"))
    }

    #[cfg(test)]
    pub(crate) fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Durably append one entry (0600, O_APPEND, file + parent dir synced before return).
    pub fn append(&self, entry: &QueueEntry) -> std::io::Result<()> {
        let line = serde_json::to_string(entry)?;
        safe_fs::append_private_jsonl_durable(&self.path, &line)
    }

    /// Read and parse the whole queue. A missing file is an empty queue.
    pub fn load(&self) -> LoadedQueue {
        let bytes = match safe_fs::read_no_symlink_limited(&self.path, QUEUE_READ_MAX) {
            Ok(b) => b,
            // A missing file is genuinely an empty queue. Any other failure (oversize,
            // permission, not a regular file) is *unknown*, not empty — flag it so the
            // flusher leaves the file intact instead of compacting it to nothing.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return LoadedQueue::default(),
            Err(e) => {
                tracing::warn!(error = %e, "scrobble queue unreadable; leaving it intact");
                return LoadedQueue {
                    read_failed: true,
                    ..LoadedQueue::default()
                };
            }
        };
        let text = String::from_utf8_lossy(&bytes);
        let mut out = LoadedQueue::default();
        let mut seen = std::collections::HashSet::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<QueueEntry>(line) {
                Ok(e) if seen.insert(e.id.clone()) => out.entries.push(e),
                Ok(_) => {} // duplicate id (crash between submit and rewrite): keep-first
                Err(_) => out.corrupt += 1,
            }
        }
        out
    }

    /// Atomically replace the file with `entries` (compaction). An empty queue removes
    /// the file entirely so an idle setup leaves no residue.
    pub fn rewrite(&self, entries: &[QueueEntry]) -> std::io::Result<()> {
        if entries.is_empty() {
            match std::fs::remove_file(&self.path) {
                Ok(()) => return Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(e) => return Err(e),
            }
        }
        let mut buf = String::new();
        for e in entries {
            buf.push_str(&serde_json::to_string(e)?);
            buf.push('\n');
        }
        safe_fs::write_private_atomic(&self.path, buf.as_bytes())
    }

    /// Take the flush lock (sibling `.lock`, O_EXCL). `None` = another live process is
    /// flushing; skip this round. A lock whose mtime is older than [`LOCK_STALE`] belongs
    /// to a dead process and is reclaimed once.
    pub fn try_lock(&self) -> Option<QueueFlushLock> {
        let lock_path = self.path.with_extension("jsonl.lock");
        for attempt in 0..2 {
            if let Some(dir) = lock_path.parent() {
                let _ = safe_fs::ensure_private_dir(dir);
            }
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&lock_path)
            {
                Ok(mut f) => {
                    use std::io::Write;
                    let _ = write!(f, "{}", std::process::id());
                    return Some(QueueFlushLock { path: lock_path });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && attempt == 0 => {
                    let stale = std::fs::metadata(&lock_path)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|m| m.elapsed().ok())
                        .is_some_and(|age| age > LOCK_STALE);
                    if stale {
                        let _ = std::fs::remove_file(&lock_path);
                        continue; // one retry with the stale lock cleared
                    }
                    return None;
                }
                Err(_) => return None,
            }
        }
        None
    }
}

/// Held while flushing; releasing (drop) removes the lock file.
pub struct QueueFlushLock {
    path: PathBuf,
}

impl Drop for QueueFlushLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Compaction policy, pure so it's testable: age out Last.fm markers past two weeks,
/// drop fully-delivered entries, and cap to the newest [`QUEUE_CAP`] by timestamp.
/// Returns the surviving entries plus how many were dropped by the cap.
pub fn compact(mut entries: Vec<QueueEntry>, now_unix: i64) -> (Vec<QueueEntry>, usize) {
    let lastfm_cutoff = now_unix - LASTFM_MAX_AGE.as_secs() as i64;
    for e in &mut entries {
        if e.ts < lastfm_cutoff {
            e.pending.retain(|s| *s != ServiceKind::Lastfm);
        }
    }
    entries.retain(|e| !e.pending.is_empty());
    let mut dropped = 0;
    if entries.len() > QUEUE_CAP {
        entries.sort_by_key(|e| e.ts);
        dropped = entries.len() - QUEUE_CAP;
        entries.drain(..dropped);
    }
    (entries, dropped)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_queue(name: &str) -> (PathBuf, QueueFile) {
        let mut bytes = [0u8; 8];
        getrandom::fill(&mut bytes).unwrap();
        let suffix = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
        let dir = std::env::temp_dir().join(format!(
            "ytm-tui-squeue-{name}-{}-{suffix}",
            std::process::id()
        ));
        let file = QueueFile::at(dir.join("scrobble-queue.jsonl"));
        (dir, file)
    }

    fn entry(id_key: &str, ts: i64, pending: Vec<ServiceKind>) -> QueueEntry {
        QueueEntry {
            id: format!("{ts}-{id_key}"),
            track_key: id_key.to_owned(),
            ts,
            artist: "artist".to_owned(),
            title: "title".to_owned(),
            album: None,
            duration: Some(200),
            origin_url: None,
            pending,
        }
    }

    #[test]
    fn append_load_rewrite_round_trip() {
        let (dir, q) = temp_queue("rt");
        let a = entry(
            "a",
            100,
            vec![ServiceKind::Lastfm, ServiceKind::ListenBrainz],
        );
        let b = entry("b", 200, vec![ServiceKind::Lastfm]);
        q.append(&a).unwrap();
        q.append(&b).unwrap();
        let loaded = q.load();
        assert_eq!(loaded.entries, vec![a.clone(), b.clone()]);
        assert_eq!(loaded.corrupt, 0);
        // Rewrite with one entry delivered; reload sees only the survivor.
        q.rewrite(std::slice::from_ref(&b)).unwrap();
        assert_eq!(q.load().entries, vec![b]);
        // Empty rewrite removes the file.
        q.rewrite(&[]).unwrap();
        assert!(!q.path().exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn corrupt_lines_are_skipped_not_fatal() {
        let (dir, q) = temp_queue("corrupt");
        let a = entry("a", 100, vec![ServiceKind::Lastfm]);
        q.append(&a).unwrap();
        crate::util::safe_fs::append_private_jsonl(q.path(), "{not json").unwrap();
        let b = entry("b", 200, vec![ServiceKind::Lastfm]);
        q.append(&b).unwrap();
        let loaded = q.load();
        assert_eq!(loaded.entries, vec![a, b]);
        assert_eq!(loaded.corrupt, 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn oversize_queue_is_flagged_read_failed_not_emptied() {
        let (dir, q) = temp_queue("oversize");
        std::fs::create_dir_all(q.path().parent().unwrap()).unwrap();
        // A file just over the read cap must not read as an empty queue.
        std::fs::write(q.path(), vec![b'x'; (QUEUE_READ_MAX as usize) + 1]).unwrap();
        let loaded = q.load();
        assert!(
            loaded.read_failed,
            "oversize file is flagged, not read as empty"
        );
        assert!(loaded.entries.is_empty());
        assert!(
            q.path().exists(),
            "the queue file is left intact, not deleted"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_queue_is_empty_not_read_failed() {
        let (dir, q) = temp_queue("missing");
        let loaded = q.load();
        assert!(
            !loaded.read_failed,
            "a missing file is a genuinely empty queue"
        );
        assert!(loaded.entries.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn duplicate_ids_keep_first() {
        let (dir, q) = temp_queue("dupe");
        let a = entry(
            "a",
            100,
            vec![ServiceKind::Lastfm, ServiceKind::ListenBrainz],
        );
        let mut a_later = a.clone();
        a_later.pending = vec![ServiceKind::Lastfm];
        q.append(&a).unwrap();
        q.append(&a_later).unwrap();
        assert_eq!(q.load().entries, vec![a]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn compaction_ages_out_lastfm_and_drops_empty() {
        let now = 10_000_000;
        let old_ts = now - 15 * 24 * 3600; // 15 days: past the Last.fm window
        let entries = vec![
            entry(
                "old-both",
                old_ts,
                vec![ServiceKind::Lastfm, ServiceKind::ListenBrainz],
            ),
            entry("old-lastfm", old_ts, vec![ServiceKind::Lastfm]),
            entry("fresh", now - 60, vec![ServiceKind::Lastfm]),
        ];
        let (kept, dropped) = compact(entries, now);
        assert_eq!(dropped, 0);
        // old-lastfm lost its only marker → gone; old-both keeps LB only.
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[0].pending, vec![ServiceKind::ListenBrainz]);
        assert_eq!(kept[1].pending, vec![ServiceKind::Lastfm]);
    }

    #[test]
    fn cap_drops_oldest_first() {
        let now = 10_000_000;
        let entries: Vec<QueueEntry> = (0..QUEUE_CAP + 10)
            .map(|i| {
                entry(
                    &format!("k{i}"),
                    now - 100_000 + i as i64,
                    vec![ServiceKind::Lastfm],
                )
            })
            .collect();
        let (kept, dropped) = compact(entries, now);
        assert_eq!(dropped, 10);
        assert_eq!(kept.len(), QUEUE_CAP);
        assert!(kept.iter().all(|e| e.ts >= now - 100_000 + 10));
    }

    #[test]
    fn lock_excludes_and_stale_lock_reclaims() {
        let (dir, q) = temp_queue("lock");
        q.append(&entry("a", 1, vec![ServiceKind::Lastfm])).unwrap();
        let lock = q.try_lock().expect("first lock succeeds");
        assert!(q.try_lock().is_none(), "second concurrent lock is refused");
        drop(lock);
        let lock2 = q.try_lock().expect("released lock can be retaken");
        // Simulate a dead process: age the lock file past the stale window.
        let lock_path = q.path().with_extension("jsonl.lock");
        drop(lock2);
        std::fs::write(&lock_path, b"999999").unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(&lock_path)
            .unwrap();
        f.set_modified(old).unwrap();
        drop(f);
        assert!(q.try_lock().is_some(), "stale lock is reclaimed");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn queue_entry_round_trips_scrobble_track() {
        let track = ScrobbleTrack {
            key: "dQw4w9WgXcQ".to_owned(),
            artist: "아이유".to_owned(),
            title: "Love wins all".to_owned(),
            album: Some("The Winning".to_owned()),
            duration_secs: Some(245),
            origin_url: Some("https://music.youtube.com/watch?v=dQw4w9WgXcQ".to_owned()),
            started_unix: 1_751_400_000,
        };
        let e = QueueEntry::from_track(&track, vec![ServiceKind::Lastfm]);
        assert!(e.id.starts_with("1751400000-"));
        assert_eq!(e.track_key, "dQw4w9WgXcQ");
        assert_eq!(e.to_track(), track);
    }

    #[test]
    fn same_second_same_track_entries_get_distinct_ids() {
        let track = ScrobbleTrack {
            key: "same".to_owned(),
            artist: "artist".to_owned(),
            title: "title".to_owned(),
            album: None,
            duration_secs: Some(120),
            origin_url: None,
            started_unix: 1_751_400_000,
        };

        let a = QueueEntry::from_track(&track, vec![ServiceKind::Lastfm]);
        let b = QueueEntry::from_track(&track, vec![ServiceKind::Lastfm]);

        assert_ne!(a.id, b.id);
        assert_eq!(a.to_track(), track);
        assert_eq!(b.to_track(), track);
    }

    #[test]
    fn old_entries_without_track_key_still_recover_key_from_id() {
        let entry = QueueEntry {
            id: "100-old-key".to_owned(),
            track_key: String::new(),
            ts: 100,
            artist: "artist".to_owned(),
            title: "title".to_owned(),
            album: None,
            duration: None,
            origin_url: None,
            pending: vec![ServiceKind::Lastfm],
        };

        assert_eq!(entry.to_track().key, "old-key");
    }
}
