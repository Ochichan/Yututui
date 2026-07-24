use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::Duration;

use super::*;

const ALL_STORES: [persist::StoreKind; 9] = [
    persist::StoreKind::PersonalState,
    persist::StoreKind::Library,
    persist::StoreKind::Signals,
    persist::StoreKind::Downloads,
    persist::StoreKind::Config,
    persist::StoreKind::Playlists,
    persist::StoreKind::Station,
    persist::StoreKind::RomanizedTitles,
    persist::StoreKind::Session,
];

fn test_dir(label: &str) -> std::path::PathBuf {
    let mut suffix = [0_u8; 8];
    getrandom::fill(&mut suffix).unwrap();
    let suffix = suffix
        .into_iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    std::env::temp_dir().join(format!(
        "yututui-runner-all-store-fallback-{}-{label}-{suffix}",
        std::process::id()
    ))
}

#[tokio::test]
async fn quit_timeout_retries_every_newest_store_when_each_store_is_contended_in_turn() {
    for blocked_kind in ALL_STORES {
        let directory = test_dir(blocked_kind.label());
        std::fs::create_dir_all(&directory).unwrap();
        let persist = persist::spawn();
        let counters: Vec<_> = ALL_STORES
            .iter()
            .map(|_| Arc::new(AtomicUsize::new(0)))
            .collect();
        let blocked_first = Arc::new(AtomicBool::new(true));
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let release_rx = Arc::new(std::sync::Mutex::new(release_rx));

        let snapshots = ALL_STORES
            .into_iter()
            .enumerate()
            .map(|(index, kind)| {
                let path = directory.join(format!("{}.json", kind.label().replace(' ', "-")));
                let writer_path = path.clone();
                let writes = Arc::clone(&counters[index]);
                let first = Arc::clone(&blocked_first);
                let release = Arc::clone(&release_rx);
                let started = started_tx.clone();
                persist::Snapshot::Test {
                    kind,
                    label: "final owner snapshot",
                    storage_path: Some(path),
                    writer: Arc::new(move || {
                        if kind == blocked_kind && first.swap(false, Ordering::SeqCst) {
                            started.send(()).unwrap();
                            release
                                .lock()
                                .unwrap()
                                .recv_timeout(Duration::from_secs(5))
                                .expect("test releases the contended store writer");
                        }
                        writes.fetch_add(1, Ordering::SeqCst);
                        crate::util::safe_fs::write_private_atomic(
                            &writer_path,
                            kind.label().as_bytes(),
                        )
                    }),
                }
            })
            .collect::<Vec<_>>();

        persist
            .seal_with_snapshots(snapshots)
            .expect("all-store quit seal is admitted atomically");
        let actor_handle = persist.clone();
        let actor_flush =
            tokio::spawn(async move { actor_handle.flush(Duration::from_secs(3)).await });
        tokio::task::spawn_blocking(move || {
            started_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("the selected store enters its intent-locked writer")
        })
        .await
        .unwrap();

        assert!(
            !persist.flush(Duration::from_millis(25)).await,
            "{} contention must make the bounded actor flush truthful",
            blocked_kind.label()
        );
        let fallback_handle = persist.clone();
        let mut fallback =
            tokio::spawn(async move { fallback_handle.fallback_newest_owned().await });
        assert!(
            tokio::time::timeout(Duration::from_millis(25), &mut fallback)
                .await
                .is_err(),
            "{} fallback must respect the actor's intent lock",
            blocked_kind.label()
        );

        release_tx.send(()).unwrap();
        fallback
            .await
            .unwrap()
            .expect("every newest shadow-owned operation becomes durable");
        assert!(actor_flush.await.unwrap());
        assert!(persist.flush(Duration::from_secs(2)).await);
        for (kind, writes) in ALL_STORES.into_iter().zip(&counters) {
            assert_eq!(
                writes.load(Ordering::SeqCst),
                1,
                "{} final snapshot was omitted or written twice when {} was contended",
                kind.label(),
                blocked_kind.label()
            );
        }

        let late = persist.save(persist::Snapshot::Test {
            kind: persist::StoreKind::Config,
            label: "post-seal save",
            storage_path: None,
            writer: Arc::new(|| Ok(())),
        });
        assert_eq!(late, Err(crate::util::delivery::DeliveryError::Closed));
        drop(persist);
        let _ = std::fs::remove_dir_all(directory);
    }
}
