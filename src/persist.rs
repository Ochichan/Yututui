//! Debounced background persistence for the on-disk stores.
//!
//! `runtime::dispatch` used to run every `Cmd::Save*` inline on the main loop task:
//! pretty-JSON serialize + temp-file write + fsync + rename. An fsync can stall for
//! tens of milliseconds, and SaveLibrary/SaveSignals fire on every track load and
//! like/skip — a visible frame hitch. This actor takes an owned snapshot instead,
//! coalesces latest-wins per store, debounces briefly, and does the identical atomic
//! write (each variant delegates to the store's own `save()`) on the blocking pool.
//!
//! Durability contract:
//! - Clean quit (incl. SIGINT/SIGTERM/SIGHUP → `Msg::Quit`): the quit path sends fresh
//!   snapshots and awaits [`PersistHandle::flush`] — zero loss.
//! - Panic (incl. release `panic = "abort"`): [`install_panic_flush`] writes everything
//!   still pending before the inherited hook kills mpv and restores the terminal.
//! - SIGKILL / power loss: at most the per-store debounce below is lost. That window is
//!   the price of not fsyncing on the UI task; every store here is preference/cache
//!   data, not payment ledgers.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum StoreKind {
    Library,
    Signals,
    Downloads,
    Config,
    Playlists,
    Station,
    RomanizedTitles,
    Session,
}

/// Owned copy of one store, taken at send time so the actor never reaches back into
/// `App`. Writing delegates to the store's own `save()` — same path resolution, same
/// atomic temp-write + fsync + rename.
pub enum Snapshot {
    Library(crate::library::Library),
    Signals(crate::signals::Signals),
    Downloads(crate::downloads::DownloadStore),
    Config(Box<crate::config::Config>),
    Playlists(crate::playlists::Playlists),
    Station(crate::station::StationStore),
    RomanizedTitles(crate::romanize::RomanizeCache),
    Session(crate::session::SessionCache),
}

impl Snapshot {
    fn kind(&self) -> StoreKind {
        match self {
            Snapshot::Library(_) => StoreKind::Library,
            Snapshot::Signals(_) => StoreKind::Signals,
            Snapshot::Downloads(_) => StoreKind::Downloads,
            Snapshot::Config(_) => StoreKind::Config,
            Snapshot::Playlists(_) => StoreKind::Playlists,
            Snapshot::Station(_) => StoreKind::Station,
            Snapshot::RomanizedTitles(_) => StoreKind::RomanizedTitles,
            Snapshot::Session(_) => StoreKind::Session,
        }
    }

    fn write(&self) -> std::io::Result<()> {
        match self {
            Snapshot::Library(s) => s.save(),
            Snapshot::Signals(s) => s.save(),
            Snapshot::Downloads(s) => s.save(),
            Snapshot::Config(s) => s.save(),
            Snapshot::Playlists(s) => s.save(),
            Snapshot::Station(s) => s.save(),
            Snapshot::RomanizedTitles(s) => s.save(),
            Snapshot::Session(s) => s.save(),
        }
    }

    fn label(&self) -> &'static str {
        match self.kind() {
            StoreKind::Library => "library",
            StoreKind::Signals => "signals",
            StoreKind::Downloads => "downloads manifest",
            StoreKind::Config => "config",
            StoreKind::Playlists => "playlists",
            StoreKind::Station => "station profile",
            StoreKind::RomanizedTitles => "romanized title cache",
            StoreKind::Session => "session cache",
        }
    }
}

/// How long a store may sit dirty before its write lands. The deadline is armed by the
/// *first* dirty event and not pushed back by later ones, so a continuous stream of
/// saves still hits disk once per window (bounded staleness, no starvation).
fn debounce(kind: StoreKind) -> Duration {
    match kind {
        // Ratings/favorites/likes: flush fast — this is the data a user would miss.
        StoreKind::Library | StoreKind::Signals => Duration::from_millis(300),
        StoreKind::Downloads | StoreKind::Config | StoreKind::Playlists | StoreKind::Station => {
            Duration::from_millis(500)
        }
        // Pure display cache, fully rebuildable — lazy is fine.
        StoreKind::RomanizedTitles => Duration::from_secs(3),
        // Only sent at quit, where flush() drains immediately anyway.
        StoreKind::Session => Duration::ZERO,
    }
}

type SharedPending = Arc<Mutex<HashMap<StoreKind, Snapshot>>>;

enum PersistMsg {
    Save(Snapshot),
    DeleteRomanizedTitles,
    Flush(oneshot::Sender<()>),
}

#[derive(Clone)]
pub struct PersistHandle {
    tx: mpsc::UnboundedSender<PersistMsg>,
    pending: SharedPending,
}

impl PersistHandle {
    pub fn save(&self, snapshot: Snapshot) {
        let _ = self.tx.send(PersistMsg::Save(snapshot));
    }

    /// Delete the romanize cache file. Routed through the actor so channel order makes
    /// it impossible for an older pending save to resurrect the file afterwards.
    pub fn delete_romanized_titles(&self) {
        let _ = self.tx.send(PersistMsg::DeleteRomanizedTitles);
    }

    /// Drain every pending write, bounded by `budget`. Returns `false` on timeout (the
    /// actor keeps writing in the background even then).
    pub async fn flush(&self, budget: Duration) -> bool {
        let (ack_tx, ack_rx) = oneshot::channel();
        if self.tx.send(PersistMsg::Flush(ack_tx)).is_err() {
            return false;
        }
        tokio::time::timeout(budget, ack_rx).await.is_ok()
    }

    /// The shared dirty-snapshot map, for [`install_panic_flush`].
    pub fn pending(&self) -> SharedPending {
        Arc::clone(&self.pending)
    }
}

pub fn spawn() -> PersistHandle {
    let (tx, rx) = mpsc::unbounded_channel();
    let pending: SharedPending = Arc::new(Mutex::new(HashMap::new()));
    tokio::spawn(run_actor(rx, Arc::clone(&pending)));
    PersistHandle { tx, pending }
}

/// Wrap the current panic hook so pending snapshots hit disk before the inherited chain
/// (mpv kill, terminal restore) runs. Must be installed *after*
/// `player::lifetime::install_panic_hook` — last installed runs first.
///
/// `try_lock` (never block): if the persist actor itself panicked while holding the
/// lock, skipping beats deadlocking. The lock is only ever held for a map insert/remove,
/// so in practice this always succeeds. A snapshot the actor already removed and was
/// mid-writing when another thread panicked is not re-written here; the temp+rename
/// write keeps the old file intact (same exposure as a SIGKILL mid-write today).
pub fn install_panic_flush(pending: SharedPending) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if let Ok(map) = pending.try_lock() {
            for snapshot in map.values() {
                let _ = snapshot.write();
            }
        }
        previous(info);
    }));
}

async fn run_actor(mut rx: mpsc::UnboundedReceiver<PersistMsg>, pending: SharedPending) {
    let mut due: HashMap<StoreKind, tokio::time::Instant> = HashMap::new();
    loop {
        let next_due = due.values().min().copied();
        tokio::select! {
            msg = rx.recv() => match msg {
                Some(PersistMsg::Save(snapshot)) => {
                    queue_pending_save(&pending, &mut due, snapshot);
                }
                Some(PersistMsg::DeleteRomanizedTitles) => {
                    lock(&pending).remove(&StoreKind::RomanizedTitles);
                    due.remove(&StoreKind::RomanizedTitles);
                    let result = tokio::task::spawn_blocking(
                        crate::romanize::RomanizeCache::delete_saved,
                    )
                    .await;
                    if let Ok(Err(e)) = result {
                        tracing::warn!(error = %e, "failed to delete romanized title cache");
                    }
                }
                Some(PersistMsg::Flush(ack)) => {
                    write_stores(&pending, &mut due, true).await;
                    let _ = ack.send(());
                }
                // All senders dropped (quit already flushed; this is a backstop).
                None => {
                    write_stores(&pending, &mut due, true).await;
                    break;
                }
            },
            _ = tokio::time::sleep_until(next_due.unwrap_or_else(tokio::time::Instant::now)),
                if next_due.is_some() =>
            {
                write_stores(&pending, &mut due, false).await;
            }
        }
    }
}

fn lock(pending: &SharedPending) -> std::sync::MutexGuard<'_, HashMap<StoreKind, Snapshot>> {
    // A panicking writer can't leave the map half-mutated in a harmful way (it's a
    // plain insert/remove), so recover from poisoning instead of propagating it.
    pending.lock().unwrap_or_else(PoisonError::into_inner)
}

fn queue_pending_save(
    pending: &SharedPending,
    due: &mut HashMap<StoreKind, tokio::time::Instant>,
    snapshot: Snapshot,
) {
    let kind = snapshot.kind();
    lock(pending).insert(kind, snapshot);
    due.entry(kind)
        .or_insert_with(|| tokio::time::Instant::now() + debounce(kind));
}

/// Write every store whose deadline has passed (`all: false`) or everything pending
/// (`all: true`). Snapshots are removed from the map before writing — short lock, and
/// the panic-flush hook then can't double-write a file the actor is mid-writing.
/// Writes run sequentially, which is what guarantees per-file ordering.
async fn write_stores(
    pending: &SharedPending,
    due: &mut HashMap<StoreKind, tokio::time::Instant>,
    all: bool,
) {
    let now = tokio::time::Instant::now();
    let kinds: Vec<StoreKind> = if all {
        // Everything dirty, whether or not its deadline armed (flush/backstop path).
        lock(pending).keys().copied().collect()
    } else {
        due.iter()
            .filter(|(_, deadline)| **deadline <= now)
            .map(|(kind, _)| *kind)
            .collect()
    };
    for kind in kinds {
        due.remove(&kind);
        let Some(snapshot) = lock(pending).remove(&kind) else {
            continue;
        };
        let result = tokio::task::spawn_blocking(move || {
            let outcome = snapshot.write();
            (outcome, snapshot)
        })
        .await;
        match result {
            Ok((Err(e), snapshot)) => {
                tracing::warn!(error = %e, "failed to save {}", snapshot.label());
            }
            Err(e) => tracing::warn!(error = %e, "persist write task failed"),
            Ok((Ok(()), _)) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic::AssertUnwindSafe;

    #[test]
    fn debounce_windows_match_store_durability_policy() {
        assert_eq!(debounce(StoreKind::Library), Duration::from_millis(300));
        assert_eq!(debounce(StoreKind::Signals), Duration::from_millis(300));
        assert_eq!(debounce(StoreKind::Downloads), Duration::from_millis(500));
        assert_eq!(debounce(StoreKind::Config), Duration::from_millis(500));
        assert_eq!(debounce(StoreKind::Playlists), Duration::from_millis(500));
        assert_eq!(debounce(StoreKind::Station), Duration::from_millis(500));
        assert_eq!(debounce(StoreKind::RomanizedTitles), Duration::from_secs(3));
        assert_eq!(debounce(StoreKind::Session), Duration::ZERO);
    }

    #[test]
    fn pending_lock_recovers_from_poisoned_mutex() {
        let pending: SharedPending = Arc::new(Mutex::new(HashMap::new()));

        let _ = std::panic::catch_unwind(AssertUnwindSafe({
            let pending = Arc::clone(&pending);
            move || {
                let _guard = pending.lock().unwrap();
                panic!("poison pending map");
            }
        }));

        let guard = lock(&pending);
        assert!(guard.is_empty());
    }

    #[tokio::test]
    async fn write_stores_clears_stale_due_entries_when_snapshot_is_missing() {
        let pending: SharedPending = Arc::new(Mutex::new(HashMap::new()));
        let mut due = HashMap::from([(StoreKind::Library, tokio::time::Instant::now())]);

        write_stores(&pending, &mut due, false).await;

        assert!(due.is_empty());
        assert!(lock(&pending).is_empty());
    }

    #[test]
    fn queued_saves_are_latest_wins_without_extending_deadline() {
        let pending: SharedPending = Arc::new(Mutex::new(HashMap::new()));
        let mut due = HashMap::new();
        let mut created = crate::playlists::Playlists::default();
        created.create("Focus").expect("playlist created");
        let mut added = created.clone();
        assert_eq!(
            added.add(
                "Focus",
                crate::api::Song::remote("id0", "Track", "Artist", "3:00")
            ),
            crate::playlists::AddResult::Added
        );

        queue_pending_save(&pending, &mut due, Snapshot::Playlists(created));
        let first_due = due[&StoreKind::Playlists];
        queue_pending_save(&pending, &mut due, Snapshot::Playlists(added));

        assert_eq!(due[&StoreKind::Playlists], first_due);
        let guard = lock(&pending);
        let Snapshot::Playlists(playlists) = guard.get(&StoreKind::Playlists).unwrap() else {
            panic!("expected playlists snapshot");
        };
        let focus = playlists.find("Focus").expect("focus playlist");
        assert_eq!(focus.songs.len(), 1);
        assert_eq!(focus.songs[0].video_id, "id0");
    }

    #[tokio::test]
    async fn flush_acknowledges_when_there_is_no_pending_work() {
        let handle = spawn();

        assert!(handle.flush(Duration::from_secs(1)).await);
        assert!(lock(&handle.pending()).is_empty());
    }
}
