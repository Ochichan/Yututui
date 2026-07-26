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
//! escape hatch can create more; both append and compaction take the same sibling advisory
//! lock. Ownership belongs to the open file handle and no process ever unlinks a "stale"
//! lock inode, so a slow but live owner cannot be bypassed.

use std::path::PathBuf;
use std::sync::OnceLock;
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(test)]
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::service::{ScrobbleTrack, ServiceKind};
use crate::util::safe_fs;

mod audit;

/// External scrobble services keep at most this many pending listens. Exact music-server
/// submissions have an independent, larger retention budget below.
pub const QUEUE_CAP: usize = 2000;
/// Exact OpenSubsonic submissions are local-first engagement events, not best-effort third-party
/// scrobbles. Keep a full personal-state event window even when external services hit their cap.
pub const OPEN_SUBSONIC_QUEUE_CAP: usize = 20_000;
/// Last.fm silently ignores scrobbles older than two weeks; stop owing it those. Other
/// services (ListenBrainz imports) accept arbitrary ages and keep their markers.
const LASTFM_MAX_AGE: Duration = Duration::from_secs(14 * 24 * 3600);
/// Personal-state payloads share the repository-wide 192 MiB interpretation ceiling. The normal
/// 22,000-entry post-compaction queue stays much smaller, while this permits recovery from a
/// pre-compaction exact-event backlog.
const QUEUE_READ_MAX: u64 = 192 * 1024 * 1024;
/// Refuse adversarial line-count growth before allocating an unbounded parsed queue.
const QUEUE_RESOURCE_MAX: usize = 40_000;
const MAX_EXACT_EVENT_ID_BYTES: usize = 1_024;
const MAX_EXACT_TRACK_KEY_BYTES: usize = 2_048;
static ENTRY_SEQ: AtomicU64 = AtomicU64::new(1);
static BOOT_NONCE: OnceLock<String> = OnceLock::new();

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct QueueDropKinds {
    generic: bool,
    open_subsonic: bool,
    scope_retired: bool,
}

/// Exact loss description produced before compaction mutates the durable queue.
///
/// Keeping marker categories separate prevents the generic 2,000-listen policy from silently
/// consuming the larger exact OpenSubsonic budget. IDs make the audit write idempotent across a
/// crash between its fsync and the queue replacement.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueueDropSummary {
    records: std::collections::BTreeMap<String, QueueDropKinds>,
}

impl QueueDropSummary {
    pub(crate) fn note_generic(&mut self, entry_id: &str) {
        self.records.entry(entry_id.to_owned()).or_default().generic = true;
    }

    pub(crate) fn note_open_subsonic(&mut self, entry_id: &str) {
        self.records
            .entry(entry_id.to_owned())
            .or_default()
            .open_subsonic = true;
    }

    pub(crate) fn note_scope_retired(&mut self, entry_id: &str) {
        self.records
            .entry(entry_id.to_owned())
            .or_default()
            .scope_retired = true;
    }

    pub fn generic_dropped(&self) -> usize {
        self.records.values().filter(|kinds| kinds.generic).count()
    }

    pub fn open_subsonic_dropped(&self) -> usize {
        self.records
            .values()
            .filter(|kinds| kinds.open_subsonic)
            .count()
    }

    pub fn scope_retired(&self) -> usize {
        self.records
            .values()
            .filter(|kinds| kinds.scope_retired)
            .count()
    }

    pub fn affected_entries(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Whether an atomic queue replacement installed every marker removal described here.
    ///
    /// A rewrite error may be reported after the replacement reached disk. The actor retains the
    /// bounded summary and checks the next strict load before publishing a permanent-loss event.
    pub(crate) fn is_installed_in(&self, entries: &[QueueEntry]) -> bool {
        self.records.iter().all(|(entry_id, kinds)| {
            entries
                .iter()
                .find(|entry| entry.id == entry_id.as_str())
                .is_none_or(|entry| {
                    (!kinds.generic || entry.pending.is_empty())
                        && (!kinds.open_subsonic || !entry.open_subsonic_pending)
                        && (!kinds.scope_retired || !entry.open_subsonic_pending)
                })
        })
    }
}

/// One queued listen. The field names are a stable on-disk format (JSONL, one per line).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueueEntry {
    /// Unique per listen, stable across reloads (dedupe key). New entries use
    /// `"{ts}-{boot nonce}-{monotonic seq}"`; old JSONL used `"{ts}-{track key}"`.
    pub id: String,
    /// Stable track identity. Added after the original id format; old entries derive this from id.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub track_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_subsonic_item: Option<crate::open_subsonic::OpenSubsonicItemRef>,
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
    /// An exact OpenSubsonic submission still needs confirmation that the credential owner's
    /// bridge store crossed its durability boundary. This marker is independent from external
    /// scrobble-service delivery and survives restarts with the same `id`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub open_subsonic_pending: bool,
    /// This exact marker crossed the source journal's pre-handoff durability boundary. Once set,
    /// the credential owner may have accepted the event even if its acknowledgement has not
    /// returned, so retention compaction must never evict it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub open_subsonic_handoff_started: bool,
    /// The bridge has durably queued this exact event, so restart must replay only the source
    /// acknowledgement and never the server submission itself.
    #[serde(default, skip_serializing_if = "is_false")]
    pub open_subsonic_bridge_durable: bool,
    /// Services that still owe this listen a delivery.
    pub pending: Vec<ServiceKind>,
}

impl QueueEntry {
    pub fn from_track(track: &ScrobbleTrack, pending: Vec<ServiceKind>) -> Self {
        Self {
            id: next_event_id(track.started_unix),
            track_key: track.key.clone(),
            open_subsonic_item: track.open_subsonic_item.clone(),
            ts: track.started_unix,
            artist: track.artist.clone(),
            title: track.title.clone(),
            album: track.album.clone(),
            duration: track.duration_secs,
            origin_url: track.origin_url.clone(),
            open_subsonic_pending: track.open_subsonic_item.is_some(),
            open_subsonic_handoff_started: false,
            open_subsonic_bridge_durable: false,
            pending,
        }
    }

    pub fn has_pending_delivery(&self) -> bool {
        self.open_subsonic_pending || !self.pending.is_empty()
    }

    pub fn to_track(&self) -> ScrobbleTrack {
        ScrobbleTrack {
            key: if self.track_key.is_empty() {
                legacy_track_key_from_id(&self.id)
            } else {
                self.track_key.clone()
            },
            open_subsonic_item: self.open_subsonic_item.clone(),
            artist: self.artist.clone(),
            title: self.title.clone(),
            album: self.album.clone(),
            duration_secs: self.duration,
            origin_url: self.origin_url.clone(),
            started_unix: self.ts,
        }
    }

    fn validate_on_disk(&self) -> bool {
        if self.open_subsonic_bridge_durable
            && (!self.open_subsonic_pending || !self.open_subsonic_handoff_started)
        {
            return false;
        }
        if self.open_subsonic_handoff_started && !self.open_subsonic_pending {
            return false;
        }
        if !self.open_subsonic_pending {
            return true;
        }
        self.open_subsonic_item.is_some()
            && valid_exact_component(&self.id, MAX_EXACT_EVENT_ID_BYTES)
            && valid_exact_component(&self.track_key, MAX_EXACT_TRACK_KEY_BYTES)
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn valid_exact_component(value: &str, max_bytes: usize) -> bool {
    !value.is_empty()
        && value.len() <= max_bytes
        && !value.chars().any(|character| {
            character.is_control()
                || matches!(
                    character,
                    '\u{202a}'..='\u{202e}'
                        | '\u{2066}'..='\u{2069}'
                        | '\u{feff}'
                )
        })
}

/// Allocate one process-unique playback event ID.
///
/// The scrobble actor also uses this at the exact OpenSubsonic threshold so the same owner event
/// keeps one identity across deferred delivery and bridge-store retries.
pub(crate) fn next_event_id(started_unix: i64) -> String {
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

/// What [`QueueFile::load`] found. Any malformed input makes the whole snapshot unreadable so a
/// later compaction can never erase an exact marker hidden in a torn or future-schema line.
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
    /// Deterministic append fault injection. Production builds have no extra state or branch.
    #[cfg(test)]
    append_failures: AtomicUsize,
    /// Deterministic ambiguous append fault: the durable write succeeds, but the caller sees an
    /// error as if a later acknowledgement or sync boundary had failed.
    #[cfg(test)]
    post_append_failures: AtomicUsize,
    /// Deterministic rewrite failures before and after the atomic replacement boundary.
    #[cfg(test)]
    rewrite_failures: AtomicUsize,
    #[cfg(test)]
    post_rewrite_failures: AtomicUsize,
    /// Deterministic blocking-I/O hook used to prove production actor isolation.
    #[cfg(test)]
    append_block: Option<Arc<AppendBlockState>>,
}

#[cfg(test)]
struct AppendBlockState {
    state: Mutex<AppendBlockStatus>,
    released: Condvar,
    blocked: tokio::sync::Notify,
}

#[cfg(test)]
#[derive(Default)]
struct AppendBlockStatus {
    armed: bool,
    release: bool,
}

#[cfg(test)]
#[derive(Clone)]
pub(crate) struct AppendBlockHandle(Arc<AppendBlockState>);

#[cfg(test)]
impl AppendBlockHandle {
    pub(crate) async fn wait_until_blocked(&self) {
        self.0.blocked.notified().await;
    }

    pub(crate) fn release(&self) {
        let mut state = self
            .0
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.release = true;
        self.0.released.notify_all();
    }
}

impl QueueFile {
    pub fn at(path: PathBuf) -> Self {
        Self {
            path,
            #[cfg(test)]
            append_failures: AtomicUsize::new(0),
            #[cfg(test)]
            post_append_failures: AtomicUsize::new(0),
            #[cfg(test)]
            rewrite_failures: AtomicUsize::new(0),
            #[cfg(test)]
            post_rewrite_failures: AtomicUsize::new(0),
            #[cfg(test)]
            append_block: None,
        }
    }

    /// The production location, following the other data-dir stores.
    pub fn default_path() -> Option<PathBuf> {
        crate::paths::data_dir().map(|d| d.join("scrobble-queue.jsonl"))
    }

    #[cfg(test)]
    pub(crate) fn path(&self) -> &std::path::Path {
        &self.path
    }

    #[cfg(test)]
    pub(crate) fn fail_next_appends(&self, count: usize) {
        self.append_failures.store(count, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub(crate) fn fail_next_appends_after_write(&self, count: usize) {
        self.post_append_failures.store(count, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub(crate) fn fail_next_rewrites(&self, count: usize) {
        self.rewrite_failures.store(count, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub(crate) fn fail_next_rewrites_after_replace(&self, count: usize) {
        self.post_rewrite_failures.store(count, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub(crate) fn block_next_append(&mut self) -> AppendBlockHandle {
        let state = Arc::new(AppendBlockState {
            state: Mutex::new(AppendBlockStatus {
                armed: true,
                release: false,
            }),
            released: Condvar::new(),
            blocked: tokio::sync::Notify::new(),
        });
        self.append_block = Some(Arc::clone(&state));
        AppendBlockHandle(state)
    }

    /// Durably append one entry (0600, O_APPEND, file + parent dir synced before return).
    pub fn append(&self, entry: &QueueEntry) -> std::io::Result<()> {
        let Some(lock) = self.try_lock_result()? else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "scrobble queue is owned by another process",
            ));
        };
        self.append_locked(entry, &lock)
    }

    pub(super) fn append_locked(
        &self,
        entry: &QueueEntry,
        _lock: &QueueFlushLock,
    ) -> std::io::Result<()> {
        #[cfg(test)]
        if self
            .append_failures
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
        {
            return Err(std::io::Error::other(
                "fault injection: no space left on device",
            ));
        }
        #[cfg(test)]
        if let Some(block) = &self.append_block {
            let mut state = block
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.armed {
                state.armed = false;
                block.blocked.notify_one();
                while !state.release {
                    state = block
                        .released
                        .wait(state)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                }
            }
        }
        let line = serde_json::to_string(entry)?;
        safe_fs::append_private_jsonl_durable(&self.path, &line)?;
        #[cfg(test)]
        if self
            .post_append_failures
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
        {
            return Err(std::io::Error::other(
                "fault injection: append durability acknowledgement lost",
            ));
        }
        Ok(())
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
        let text = match std::str::from_utf8(&bytes) {
            Ok(text) => text,
            Err(_) => {
                return LoadedQueue {
                    corrupt: 1,
                    read_failed: true,
                    ..LoadedQueue::default()
                };
            }
        };
        let mut out = LoadedQueue::default();
        let mut seen = std::collections::HashMap::<String, usize>::new();
        let mut resources = 0usize;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            resources = resources.saturating_add(1);
            if resources > QUEUE_RESOURCE_MAX {
                tracing::warn!(
                    resources,
                    limit = QUEUE_RESOURCE_MAX,
                    "scrobble queue resource limit exceeded; leaving it intact"
                );
                return LoadedQueue {
                    read_failed: true,
                    ..LoadedQueue::default()
                };
            }
            match serde_json::from_str::<QueueEntry>(line) {
                Ok(entry) if !entry.validate_on_disk() => {
                    return LoadedQueue {
                        corrupt: out.corrupt.saturating_add(1),
                        read_failed: true,
                        ..LoadedQueue::default()
                    };
                }
                Ok(entry) => {
                    if let Some(position) = seen.get(&entry.id).copied() {
                        if out.entries[position] != entry {
                            return LoadedQueue {
                                corrupt: out.corrupt.saturating_add(1),
                                read_failed: true,
                                ..LoadedQueue::default()
                            };
                        }
                    } else {
                        seen.insert(entry.id.clone(), out.entries.len());
                        out.entries.push(entry);
                    }
                }
                Err(_) => {
                    return LoadedQueue {
                        corrupt: out.corrupt.saturating_add(1),
                        read_failed: true,
                        ..LoadedQueue::default()
                    };
                }
            }
        }
        if out
            .entries
            .iter()
            .filter(|entry| entry.open_subsonic_pending && entry.open_subsonic_handoff_started)
            .count()
            > OPEN_SUBSONIC_QUEUE_CAP
        {
            tracing::warn!(
                limit = OPEN_SUBSONIC_QUEUE_CAP,
                "protected music server handoff marker limit exceeded; leaving queue intact"
            );
            return LoadedQueue {
                corrupt: out.corrupt.saturating_add(1),
                read_failed: true,
                ..LoadedQueue::default()
            };
        }
        out
    }

    /// Atomically replace the file with `entries` (compaction). An empty queue removes
    /// the file entirely so an idle setup leaves no residue.
    pub fn rewrite(&self, entries: &[QueueEntry]) -> std::io::Result<()> {
        let Some(lock) = self.try_lock_result()? else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "scrobble queue is owned by another process",
            ));
        };
        self.rewrite_locked(entries, &lock)
    }

    pub(super) fn rewrite_locked(
        &self,
        entries: &[QueueEntry],
        _lock: &QueueFlushLock,
    ) -> std::io::Result<()> {
        #[cfg(test)]
        if consume_failure(&self.rewrite_failures) {
            return Err(std::io::Error::other(
                "fault injection: rewrite failed before replacement",
            ));
        }
        if entries.is_empty() {
            safe_fs::remove_private_file_durable(&self.path)?;
        } else {
            let mut buf = String::new();
            for e in entries {
                buf.push_str(&serde_json::to_string(e)?);
                buf.push('\n');
            }
            safe_fs::write_private_atomic(&self.path, buf.as_bytes())?;
        }
        #[cfg(test)]
        if consume_failure(&self.post_rewrite_failures) {
            return Err(std::io::Error::other(
                "fault injection: rewrite durability acknowledgement lost after replacement",
            ));
        }
        Ok(())
    }

    /// Take the queue ownership lock. `None` means another live process owns it; kernel lock
    /// lifetime, not wall-clock age, decides ownership.
    pub fn try_lock(&self) -> Option<QueueFlushLock> {
        match self.try_lock_result() {
            Ok(lock) => lock,
            Err(error) => {
                tracing::warn!(error = %error, "failed to acquire scrobble queue lock");
                None
            }
        }
    }

    pub(super) fn try_lock_result(&self) -> std::io::Result<Option<QueueFlushLock>> {
        let lock_path = self.path.with_extension("jsonl.lock");
        Ok(safe_fs::try_lock_private_file(&lock_path)?
            .map(|guard| QueueFlushLock { _guard: guard }))
    }

    /// Persist a loss record before installing a capped queue replacement.
    pub(super) fn record_drop_audit_locked(
        &self,
        summary: &QueueDropSummary,
        observed_at_unix: i64,
        _lock: &QueueFlushLock,
    ) -> std::io::Result<()> {
        audit::record(&self.path, summary, observed_at_unix)
    }

    #[cfg(test)]
    pub(crate) fn drop_audit_counts(&self) -> std::io::Result<(u64, u64, usize)> {
        audit::counts(&self.path)
    }

    #[cfg(test)]
    pub(crate) fn scope_retirement_audit_count(&self) -> std::io::Result<u64> {
        audit::scope_retired_count(&self.path)
    }
}

#[cfg(test)]
fn consume_failure(counter: &AtomicUsize) -> bool {
    counter
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
            remaining.checked_sub(1)
        })
        .is_ok()
}

/// Held while flushing; releasing drops the kernel advisory lock. The path remains stable.
pub struct QueueFlushLock {
    _guard: safe_fs::AdvisoryFileLock,
}

/// Compaction policy, pure so it's testable: age out Last.fm markers past two weeks, drop
/// fully-delivered entries, then cap generic and exact-server markers independently.
pub fn compact(mut entries: Vec<QueueEntry>, now_unix: i64) -> (Vec<QueueEntry>, QueueDropSummary) {
    let lastfm_cutoff = now_unix - LASTFM_MAX_AGE.as_secs() as i64;
    for e in &mut entries {
        if e.ts < lastfm_cutoff {
            e.pending.retain(|s| *s != ServiceKind::Lastfm);
        }
    }
    entries.retain(QueueEntry::has_pending_delivery);
    entries.sort_by(|left, right| left.ts.cmp(&right.ts).then_with(|| left.id.cmp(&right.id)));

    let mut dropped = QueueDropSummary::default();
    let open_subsonic_excess = entries
        .iter()
        .filter(|entry| entry.open_subsonic_pending)
        .count()
        .saturating_sub(OPEN_SUBSONIC_QUEUE_CAP);
    for entry in entries
        .iter_mut()
        .rev()
        .filter(|entry| entry.open_subsonic_pending && !entry.open_subsonic_handoff_started)
        .take(open_subsonic_excess)
    {
        entry.open_subsonic_pending = false;
        entry.open_subsonic_handoff_started = false;
        entry.open_subsonic_bridge_durable = false;
        dropped.note_open_subsonic(&entry.id);
    }

    let generic_excess = entries
        .iter()
        .filter(|entry| !entry.pending.is_empty())
        .count()
        .saturating_sub(QUEUE_CAP);
    for entry in entries
        .iter_mut()
        .filter(|entry| !entry.pending.is_empty())
        .take(generic_excess)
    {
        entry.pending.clear();
        dropped.note_generic(&entry.id);
    }
    entries.retain(QueueEntry::has_pending_delivery);
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
            "yututui-squeue-{name}-{}-{suffix}",
            std::process::id()
        ));
        let file = QueueFile::at(dir.join("scrobble-queue.jsonl"));
        (dir, file)
    }

    fn entry(id_key: &str, ts: i64, pending: Vec<ServiceKind>) -> QueueEntry {
        QueueEntry {
            id: format!("{ts}-{id_key}"),
            track_key: id_key.to_owned(),
            open_subsonic_item: None,
            ts,
            artist: "artist".to_owned(),
            title: "title".to_owned(),
            album: None,
            duration: Some(200),
            origin_url: None,
            open_subsonic_pending: false,
            open_subsonic_handoff_started: false,
            open_subsonic_bridge_durable: false,
            pending,
        }
    }

    fn server_entry(id_key: &str, ts: i64, pending: Vec<ServiceKind>) -> QueueEntry {
        let mut entry = entry(id_key, ts, pending);
        entry.open_subsonic_item = Some(crate::open_subsonic::OpenSubsonicItemRef::new(
            crate::open_subsonic::BackendId::new("server-backend").unwrap(),
            crate::open_subsonic::AccountScopeId::new("account-scope").unwrap(),
            crate::open_subsonic::ItemId::new(id_key).unwrap(),
        ));
        entry.open_subsonic_pending = true;
        entry
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
    fn corrupt_json_fails_closed_without_returning_a_partial_queue() {
        let (dir, q) = temp_queue("corrupt");
        let a = entry("a", 100, vec![ServiceKind::Lastfm]);
        q.append(&a).unwrap();
        crate::util::safe_fs::append_private_jsonl(q.path(), "{not json").unwrap();
        let b = entry("b", 200, vec![ServiceKind::Lastfm]);
        q.append(&b).unwrap();
        let loaded = q.load();
        assert!(loaded.read_failed);
        assert!(loaded.entries.is_empty());
        assert_eq!(loaded.corrupt, 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn jsonl_loader_fails_closed_on_deterministic_corrupt_corpus() {
        let (dir, q) = temp_queue("corrupt-corpus");
        let a = entry("a", 100, vec![ServiceKind::Lastfm]);
        let b = entry("b", 200, vec![ServiceKind::ListenBrainz]);
        q.append(&a).unwrap();

        let mut state = 0x1319_8a2e_0370_7344u64;
        for idx in 0..128 {
            state = state
                .wrapping_mul(2862933555777941757)
                .wrapping_add(3037000493);
            let line = match state % 5 {
                0 => "{",
                1 => r#"{"id":42}"#,
                2 => r#"["not","an","entry"]"#,
                3 => r#"{"id":"x","pending":["lastfm"]"#,
                _ => r#"{"id":"x","started_unix":"bad","pending":["lastfm"]}"#,
            };
            crate::util::safe_fs::append_private_jsonl(q.path(), line).unwrap();
            if idx == 63 {
                q.append(&b).unwrap();
            }
        }

        let loaded = q.load();

        assert!(loaded.read_failed);
        assert!(loaded.entries.is_empty());
        assert_eq!(loaded.corrupt, 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn invalid_utf8_and_invalid_exact_marker_shape_fail_closed() {
        let (utf8_dir, utf8_queue) = temp_queue("invalid-utf8");
        std::fs::create_dir_all(utf8_queue.path().parent().unwrap()).unwrap();
        std::fs::write(utf8_queue.path(), [0xff, b'\n']).unwrap();
        let loaded = utf8_queue.load();
        assert!(loaded.read_failed);
        assert!(loaded.entries.is_empty());

        let (shape_dir, shape_queue) = temp_queue("invalid-exact-shape");
        let mut malformed = entry("server", 100, Vec::new());
        malformed.open_subsonic_pending = true;
        shape_queue.append(&malformed).unwrap();
        let loaded = shape_queue.load();
        assert!(loaded.read_failed);
        assert!(loaded.entries.is_empty());

        let _ = std::fs::remove_dir_all(utf8_dir);
        let _ = std::fs::remove_dir_all(shape_dir);
    }

    #[test]
    fn oversize_queue_is_flagged_read_failed_not_emptied() {
        let (dir, q) = temp_queue("oversize");
        std::fs::create_dir_all(q.path().parent().unwrap()).unwrap();
        // A file just over the read cap must not read as an empty queue.
        let file = std::fs::File::create(q.path()).unwrap();
        file.set_len(QUEUE_READ_MAX + 1).unwrap();
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
    fn identical_duplicate_ids_are_an_idempotent_single_entry() {
        let (dir, q) = temp_queue("identical-dupe");
        let a = entry(
            "a",
            100,
            vec![ServiceKind::Lastfm, ServiceKind::ListenBrainz],
        );
        q.append(&a).unwrap();
        q.append(&a).unwrap();

        let loaded = q.load();

        assert!(!loaded.read_failed);
        assert_eq!(loaded.corrupt, 0);
        assert_eq!(loaded.entries, vec![a]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn conflicting_duplicate_ids_fail_closed_and_preserve_the_journal() {
        let (dir, q) = temp_queue("conflicting-dupe");
        let a = entry(
            "a",
            100,
            vec![ServiceKind::Lastfm, ServiceKind::ListenBrainz],
        );
        let mut a_later = a.clone();
        a_later.pending = vec![ServiceKind::Lastfm];
        q.append(&a).unwrap();
        q.append(&a_later).unwrap();
        let before = std::fs::read(q.path()).unwrap();

        let loaded = q.load();

        assert!(loaded.read_failed);
        assert_eq!(loaded.corrupt, 1);
        assert!(loaded.entries.is_empty());
        assert_eq!(std::fs::read(q.path()).unwrap(), before);
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
        assert!(dropped.is_empty());
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
        assert_eq!(dropped.generic_dropped(), 10);
        assert_eq!(dropped.open_subsonic_dropped(), 0);
        assert_eq!(kept.len(), QUEUE_CAP);
        assert!(kept.iter().all(|e| e.ts >= now - 100_000 + 10));
    }

    #[test]
    fn generic_cap_clears_external_markers_without_dropping_exact_server_events() {
        let now = 10_000_000;
        let entries = (0..QUEUE_CAP + 10)
            .map(|index| {
                server_entry(
                    &format!("song-{index}"),
                    now - 100_000 + index as i64,
                    vec![ServiceKind::ListenBrainz],
                )
            })
            .collect();

        let (kept, dropped) = compact(entries, now);

        assert_eq!(kept.len(), QUEUE_CAP + 10);
        assert_eq!(dropped.generic_dropped(), 10);
        assert_eq!(dropped.open_subsonic_dropped(), 0);
        assert!(kept.iter().all(|entry| entry.open_subsonic_pending));
        assert_eq!(
            kept.iter()
                .filter(|entry| !entry.pending.is_empty())
                .count(),
            QUEUE_CAP
        );
    }

    #[test]
    fn exact_server_cap_is_independent_and_drops_only_its_newest_unstarted_markers() {
        let now = 10_000_000;
        let entries = (0..OPEN_SUBSONIC_QUEUE_CAP + 10)
            .map(|index| {
                server_entry(
                    &format!("song-{index}"),
                    now - 100_000 + index as i64,
                    Vec::new(),
                )
            })
            .collect();

        let (kept, dropped) = compact(entries, now);

        assert_eq!(kept.len(), OPEN_SUBSONIC_QUEUE_CAP);
        assert_eq!(dropped.generic_dropped(), 0);
        assert_eq!(dropped.open_subsonic_dropped(), 10);
        assert!(
            kept.iter()
                .all(|entry| entry.ts < now - 100_000 + OPEN_SUBSONIC_QUEUE_CAP as i64)
        );
    }

    #[test]
    fn exact_cap_never_evicts_started_or_awaiting_source_handoffs() {
        let now = 10_000_000;
        let mut entries = (0..OPEN_SUBSONIC_QUEUE_CAP)
            .map(|index| {
                let mut entry = server_entry(
                    &format!("protected-{index}"),
                    now - 100_000 + index as i64,
                    Vec::new(),
                );
                entry.open_subsonic_handoff_started = true;
                entry
            })
            .collect::<Vec<_>>();
        entries[0].open_subsonic_bridge_durable = true;
        let newest = server_entry("never-handed-off", now, Vec::new());
        entries.push(newest.clone());

        let (kept, dropped) = compact(entries, now);

        assert_eq!(kept.len(), OPEN_SUBSONIC_QUEUE_CAP);
        assert_eq!(dropped.open_subsonic_dropped(), 1);
        assert!(
            kept.iter().any(
                |entry| entry.id.ends_with("protected-0") && entry.open_subsonic_bridge_durable
            )
        );
        assert!(kept.iter().all(|entry| entry.open_subsonic_handoff_started));
        assert!(!kept.iter().any(|entry| entry.id == newest.id));
    }

    #[test]
    fn drop_audit_is_durable_and_idempotent_across_queue_reopen() {
        let (dir, queue) = temp_queue("drop-audit");
        let now = 10_000_000;
        let entries = (0..QUEUE_CAP + 3)
            .map(|index| {
                entry(
                    &format!("track-{index}"),
                    now - 100_000 + index as i64,
                    vec![ServiceKind::ListenBrainz],
                )
            })
            .collect();
        let (_kept, dropped) = compact(entries, now);
        let lock = queue.try_lock().unwrap();
        queue
            .record_drop_audit_locked(&dropped, now, &lock)
            .unwrap();
        drop(lock);

        let reopened = QueueFile::at(queue.path().to_path_buf());
        let lock = reopened.try_lock().unwrap();
        reopened
            .record_drop_audit_locked(&dropped, now + 1, &lock)
            .unwrap();
        drop(lock);

        assert_eq!(reopened.drop_audit_counts().unwrap(), (3, 0, 1));
        let audit_path = queue.path().with_extension("drops.json");
        let serialized = std::fs::read_to_string(&audit_path).unwrap();
        assert!(!serialized.contains("track-0"));
        assert!(!serialized.contains("https://"));
        assert!(!serialized.contains(&dir.to_string_lossy().to_string()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(audit_path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn capped_exact_queue_and_audit_survive_reopen() {
        let (dir, queue) = temp_queue("exact-cap-reopen");
        let now = 10_000_000;
        let entries = (0..OPEN_SUBSONIC_QUEUE_CAP + 2)
            .map(|index| {
                server_entry(
                    &format!("song-{index}"),
                    now - 100_000 + index as i64,
                    Vec::new(),
                )
            })
            .collect();
        let (kept, dropped) = compact(entries, now);
        let lock = queue.try_lock().unwrap();
        queue
            .record_drop_audit_locked(&dropped, now, &lock)
            .unwrap();
        queue.rewrite_locked(&kept, &lock).unwrap();
        drop(lock);

        let reopened = QueueFile::at(queue.path().to_path_buf());
        let loaded = reopened.load();
        assert!(!loaded.read_failed);
        assert_eq!(loaded.entries.len(), OPEN_SUBSONIC_QUEUE_CAP);
        assert!(
            loaded
                .entries
                .iter()
                .all(|entry| entry.open_subsonic_pending)
        );
        assert_eq!(reopened.drop_audit_counts().unwrap(), (0, 2, 1));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn excessive_resource_count_is_read_failed_not_a_partial_queue() {
        let (dir, queue) = temp_queue("resource-count");
        std::fs::create_dir_all(queue.path().parent().unwrap()).unwrap();
        let rows = "{}\n".repeat(QUEUE_RESOURCE_MAX + 1);
        std::fs::write(queue.path(), rows).unwrap();

        let loaded = queue.load();

        assert!(loaded.read_failed);
        assert!(loaded.entries.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn advisory_lock_excludes_until_owner_drops_even_if_lockfile_is_old() {
        let (dir, q) = temp_queue("lock");
        q.append(&entry("a", 1, vec![ServiceKind::Lastfm])).unwrap();
        let lock = q.try_lock().expect("first lock succeeds");
        assert!(q.try_lock().is_none(), "second concurrent lock is refused");
        // Aging the persistent lock path must never grant ownership while the kernel still has
        // a live owner. This is the race the former stale-file unlink could violate.
        let lock_path = q.path().with_extension("jsonl.lock");
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(&lock_path)
            .unwrap();
        f.set_modified(old).unwrap();
        drop(f);
        assert!(
            q.try_lock().is_none(),
            "mtime never overrides a live advisory-lock owner"
        );
        drop(lock);
        assert!(
            q.try_lock().is_some(),
            "dropping the owner releases the lock"
        );
        assert!(
            lock_path.exists(),
            "the stable lock inode is never unlinked"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn append_cannot_cross_an_in_progress_rewrite_from_another_queue_owner() {
        let (dir, writer) = temp_queue("append-rewrite-exclusion");
        let contender = QueueFile::at(writer.path().to_path_buf());
        let retained = entry("retained", 1, vec![ServiceKind::Lastfm]);
        let appended = entry("appended", 2, vec![ServiceKind::Lastfm]);
        writer.append(&retained).unwrap();

        let rewrite_owner = writer.try_lock().expect("rewrite takes ownership");
        let start = Arc::new(std::sync::Barrier::new(2));
        let contender_start = Arc::clone(&start);
        let contender_entry = appended.clone();
        let attempt = std::thread::spawn(move || {
            contender_start.wait();
            contender.append(&contender_entry)
        });
        start.wait();
        writer
            .rewrite_locked(std::slice::from_ref(&retained), &rewrite_owner)
            .unwrap();
        let error = attempt
            .join()
            .expect("append contender thread joins")
            .expect_err("append cannot enter while rewrite owns the queue");
        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
        assert_eq!(writer.load().entries, vec![retained.clone()]);

        drop(rewrite_owner);
        writer.append(&appended).unwrap();
        assert_eq!(writer.load().entries, vec![retained, appended]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn queue_entry_round_trips_scrobble_track() {
        let (dir, queue) = temp_queue("server-item-round-trip");
        let track = ScrobbleTrack {
            key: "dQw4w9WgXcQ".to_owned(),
            open_subsonic_item: Some(crate::open_subsonic::OpenSubsonicItemRef::new(
                crate::open_subsonic::BackendId::new("server-backend").unwrap(),
                crate::open_subsonic::AccountScopeId::new("account-scope").unwrap(),
                crate::open_subsonic::ItemId::new("song-42").unwrap(),
            )),
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
        queue.append(&e).unwrap();
        let loaded = queue.load();
        assert!(!loaded.read_failed);
        assert_eq!(loaded.entries, vec![e]);
        assert_eq!(loaded.entries[0].to_track(), track);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn same_second_same_track_entries_get_distinct_ids() {
        let track = ScrobbleTrack {
            key: "same".to_owned(),
            open_subsonic_item: None,
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
            open_subsonic_item: None,
            ts: 100,
            artist: "artist".to_owned(),
            title: "title".to_owned(),
            album: None,
            duration: None,
            origin_url: None,
            open_subsonic_pending: false,
            open_subsonic_handoff_started: false,
            open_subsonic_bridge_durable: false,
            pending: vec![ServiceKind::Lastfm],
        };

        assert_eq!(entry.to_track().key, "old-key");
    }
}
