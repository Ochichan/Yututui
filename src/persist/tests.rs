use super::*;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use serde::{Deserialize, Serialize};

#[path = "tests/handle.rs"]
mod handle;
#[path = "tests/panic_shadow_seal.rs"]
mod panic_shadow_seal;
#[path = "tests/personal_state.rs"]
mod personal_state;
#[path = "tests/protocol_model.rs"]
mod protocol_model;
#[path = "tests/races.rs"]
mod races;
#[path = "tests/removal.rs"]
mod removal;
#[path = "tests/write_queue.rs"]
mod write_queue;

fn temp_dir(name: &str) -> PathBuf {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).unwrap();
    let suffix = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    std::env::temp_dir().join(format!(
        "yututui-persist-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn intent_sidecar_count(directory: &Path, base_name: &str) -> usize {
    let prefix = format!("{base_name}.intent.");
    std::fs::read_dir(directory)
        .unwrap()
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with(&prefix) && name.ends_with(".json"))
        })
        .count()
}

fn journal_order(sequence: u64, marker: u8) -> JournalOrder {
    journal_order_in_epoch(1, u128::from(sequence), marker)
}

fn journal_order_in_epoch(process_epoch: u64, sequence: u128, marker: u8) -> JournalOrder {
    JournalOrder {
        process_epoch,
        sequence,
        generation: JournalGeneration([marker; 16]),
    }
}

fn accepted_order(order: JournalOrder) -> AcceptedJournalOrder {
    AcceptedJournalOrder { order, error: None }
}

fn next_test_order() -> AcceptedJournalOrder {
    static NEXT_SEQUENCE: AtomicU64 = AtomicU64::new(1);
    let sequence = NEXT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    accepted_order(journal_order(
        sequence,
        sequence.to_le_bytes()[0].wrapping_add(1),
    ))
}

fn pending_save(snapshot: Snapshot) -> PendingOperation {
    PendingOperation::save(snapshot, next_test_order())
}

fn test_order_source() -> Arc<JournalOrderSource> {
    Arc::new(JournalOrderSource::for_test(1))
}

fn test_operation(
    kind: StoreKind,
    order: JournalOrder,
    storage_path: Option<PathBuf>,
    writer: Arc<dyn Fn() -> std::io::Result<()> + Send + Sync>,
) -> PendingOperation {
    PendingOperation::save(
        Snapshot::Test {
            kind,
            label: "journal interleaving",
            storage_path,
            writer,
        },
        accepted_order(order),
    )
}

#[test]
fn debounce_windows_match_store_durability_policy() {
    assert_eq!(
        debounce(StoreKind::PersonalState),
        Duration::from_millis(300)
    );
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
    let pending: SharedPending = Arc::new(Mutex::new(PendingQueue::new()));

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

#[test]
fn journaled_snapshot_replays_and_clears() {
    #[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
    struct Tiny {
        value: u8,
    }

    let dir = temp_dir("intent");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("tiny.json");
    let bytes = serde_json::to_vec_pretty(&Tiny { value: 7 }).unwrap();

    write_journal_intent(&JournalIntent::Replace {
        order: journal_order(1, 1),
        kind: StoreKind::Config,
        path: path.clone(),
        bytes,
    })
    .unwrap();

    let record_text = std::fs::read_to_string(intent_journal_path(&path).unwrap()).unwrap();
    let record: serde_json::Value = serde_json::from_str(record_text.trim()).unwrap();
    assert_eq!(record.get("v").and_then(|value| value.as_u64()), Some(1));
    assert_eq!(
        record.get("op").and_then(|value| value.as_str()),
        Some("replace")
    );
    assert_eq!(
        record.get("kind").and_then(|value| value.as_str()),
        Some(StoreKind::Config.label())
    );
    assert!(
        record
            .get("sidecar")
            .and_then(|value| value.as_str())
            .is_some()
    );
    assert!(
        record
            .get("sha256")
            .and_then(|value| value.as_str())
            .is_some()
    );
    assert!(
        record
            .get("generation")
            .and_then(|value| value.as_str())
            .is_some()
    );
    assert!(
        record
            .get("process_epoch")
            .and_then(|value| value.as_str())
            .is_some()
    );
    assert!(
        record
            .get("sequence")
            .and_then(|value| value.as_str())
            .is_some()
    );

    let replayed = replay_journaled_snapshot(StoreKind::Config, &path, Tiny { value: 1 }, 1024);
    assert_eq!(replayed, Tiny { value: 7 });

    clear_store_journal(&path);
    let replayed = replay_journaled_snapshot(StoreKind::Config, &path, Tiny { value: 1 }, 1024);
    assert_eq!(replayed, Tiny { value: 1 });
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn journaled_delete_supersedes_an_older_save_and_a_newer_save_supersedes_delete() {
    #[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
    struct Tiny {
        value: u8,
    }

    let dir = temp_dir("delete-intent");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("tiny.json");
    let replace = |value| JournalIntent::Replace {
        order: next_test_order().order,
        kind: StoreKind::RomanizedTitles,
        path: path.clone(),
        bytes: serde_json::to_vec_pretty(&Tiny { value }).unwrap(),
    };
    write_journal_intent(&replace(7)).unwrap();
    write_journal_intent(&JournalIntent::Delete {
        order: next_test_order().order,
        kind: StoreKind::RomanizedTitles,
        path: path.clone(),
    })
    .unwrap();
    assert_eq!(
        replay_journaled_snapshot(StoreKind::RomanizedTitles, &path, Tiny { value: 1 }, 1024,),
        Tiny::default()
    );

    write_journal_intent(&replace(9)).unwrap();
    assert_eq!(
        replay_journaled_snapshot(StoreKind::RomanizedTitles, &path, Tiny { value: 1 }, 1024,),
        Tiny { value: 9 }
    );
    clear_store_journal(&path);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn generationless_v1_latest_sidecar_remains_replayable() {
    #[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
    struct Tiny {
        value: u8,
    }

    let dir = temp_dir("legacy-intent");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("tiny.json");
    let sidecar = intent_sidecar_path(&path).unwrap();
    let bytes = serde_json::to_vec_pretty(&Tiny { value: 17 }).unwrap();
    crate::util::safe_fs::write_private_atomic(&sidecar, &bytes).unwrap();
    let record = serde_json::json!({
        "v": 1,
        "op": "replace",
        "kind": StoreKind::Config.label(),
        "sidecar": sidecar.file_name().unwrap().to_str().unwrap(),
        "sha256": sha256_hex(&bytes),
    });
    append_journal_record(&path, &record).unwrap();

    assert_eq!(
        replay_journaled_snapshot(StoreKind::Config, &path, Tiny::default(), 1024),
        Tiny { value: 17 }
    );
    clear_store_journal(&path);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn sequential_generationless_rollback_replays_until_a_new_ordered_successor() {
    #[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
    struct Tiny {
        value: u8,
    }

    let dir = temp_dir("legacy-after-frontier");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("tiny.json");
    let ordered = journal_order(100, 1);
    write_journal_intent(&JournalIntent::Replace {
        order: ordered,
        kind: StoreKind::Config,
        path: path.clone(),
        bytes: serde_json::to_vec_pretty(&Tiny { value: 10 }).unwrap(),
    })
    .unwrap();
    commit_journal_generation(StoreKind::Config, &path, ordered).unwrap();

    // A complete generation-less record directly after the ordered frontier is evidence that an
    // older binary ran sequentially after the newer binary stopped.
    let legacy_sidecar = intent_sidecar_path(&path).unwrap();
    let legacy_bytes = serde_json::to_vec_pretty(&Tiny { value: 9 }).unwrap();
    crate::util::safe_fs::write_private_atomic(&legacy_sidecar, &legacy_bytes).unwrap();
    append_journal_record(
        &path,
        &serde_json::json!({
            "v": 1,
            "op": "replace",
            "kind": StoreKind::Config.label(),
            "sidecar": legacy_sidecar.file_name().unwrap().to_str().unwrap(),
            "sha256": sha256_hex(&legacy_bytes),
        }),
    )
    .unwrap();

    assert_eq!(
        replay_journaled_snapshot(StoreKind::Config, &path, Tiny { value: 10 }, 1024),
        Tiny { value: 9 }
    );

    let successor = journal_order_in_epoch(2, 1, 2);
    write_journal_intent(&JournalIntent::Replace {
        order: successor,
        kind: StoreKind::Config,
        path: path.clone(),
        bytes: serde_json::to_vec_pretty(&Tiny { value: 11 }).unwrap(),
    })
    .unwrap();
    assert_eq!(
        replay_journaled_snapshot(StoreKind::Config, &path, Tiny { value: 10 }, 1024),
        Tiny { value: 11 }
    );
    clear_store_journal(&path);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn torn_segment_does_not_masquerade_as_a_sequential_legacy_rollback() {
    use std::io::Write as _;

    #[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
    struct Tiny {
        value: u8,
    }

    let dir = temp_dir("legacy-torn-boundary");
    std::fs::create_dir_all(&dir).unwrap();
    for (name, invalid_before_legacy) in [("before.json", true), ("after.json", false)] {
        let path = dir.join(name);
        let ordered = journal_order(100, 1);
        write_journal_intent(&JournalIntent::Replace {
            order: ordered,
            kind: StoreKind::Config,
            path: path.clone(),
            bytes: serde_json::to_vec_pretty(&Tiny { value: 10 }).unwrap(),
        })
        .unwrap();
        commit_journal_generation(StoreKind::Config, &path, ordered).unwrap();
        let journal_path = intent_journal_path(&path).unwrap();
        if invalid_before_legacy {
            let mut journal = std::fs::OpenOptions::new()
                .append(true)
                .open(&journal_path)
                .unwrap();
            journal.write_all(b"{\"v\":1\n").unwrap();
            journal.sync_all().unwrap();
        }

        let legacy_sidecar = intent_sidecar_path(&path).unwrap();
        let legacy_bytes = serde_json::to_vec_pretty(&Tiny { value: 9 }).unwrap();
        crate::util::safe_fs::write_private_atomic(&legacy_sidecar, &legacy_bytes).unwrap();
        append_journal_record(
            &path,
            &serde_json::json!({
                "v": 1,
                "op": "replace",
                "kind": StoreKind::Config.label(),
                "sidecar": legacy_sidecar.file_name().unwrap().to_str().unwrap(),
                "sha256": sha256_hex(&legacy_bytes),
            }),
        )
        .unwrap();
        if !invalid_before_legacy {
            let mut journal = std::fs::OpenOptions::new()
                .append(true)
                .open(&journal_path)
                .unwrap();
            journal.write_all(b"{\"v\":1").unwrap();
            journal.sync_all().unwrap();
        }

        assert_eq!(
            replay_journaled_snapshot(StoreKind::Config, &path, Tiny { value: 10 }, 1024),
            Tiny { value: 10 },
            "a torn segment {name} must keep the ordered frontier authoritative"
        );
        clear_store_journal(&path);
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn stale_journal_completion_cannot_replace_newer_delete_or_save() {
    #[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
    struct Tiny {
        value: u8,
    }

    let dir = temp_dir("stale-journal-completion");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("tiny.json");
    let pending: SharedPending = Arc::new(Mutex::new(PendingQueue::new()));

    let old_save_order = journal_order(10, 1);
    lock(&pending).insert(
        StoreKind::RomanizedTitles,
        test_operation(
            StoreKind::RomanizedTitles,
            old_save_order,
            None,
            Arc::new(|| Ok(())),
        ),
    );
    let old_save = JournalIntent::Replace {
        order: old_save_order,
        kind: StoreKind::RomanizedTitles,
        path: path.clone(),
        bytes: serde_json::to_vec_pretty(&Tiny { value: 3 }).unwrap(),
    };
    let newer_delete_order = journal_order(20, 2);
    lock(&pending).insert(
        StoreKind::RomanizedTitles,
        test_operation(
            StoreKind::RomanizedTitles,
            newer_delete_order,
            None,
            Arc::new(|| Ok(())),
        ),
    );
    assert!(matches!(
        write_journal_intent_if_current(&old_save, &pending).unwrap(),
        JournalAppend::Stale
    ));
    write_journal_intent(&JournalIntent::Delete {
        order: newer_delete_order,
        kind: StoreKind::RomanizedTitles,
        path: path.clone(),
    })
    .unwrap();
    assert_eq!(
        replay_journaled_snapshot(StoreKind::RomanizedTitles, &path, Tiny { value: 8 }, 1024,),
        Tiny::default()
    );

    clear_store_journal(&path);
    let old_delete_order = journal_order(30, 3);
    lock(&pending).insert(
        StoreKind::RomanizedTitles,
        test_operation(
            StoreKind::RomanizedTitles,
            old_delete_order,
            None,
            Arc::new(|| Ok(())),
        ),
    );
    let old_delete = JournalIntent::Delete {
        order: old_delete_order,
        kind: StoreKind::RomanizedTitles,
        path: path.clone(),
    };
    let newer_save_order = journal_order(40, 4);
    lock(&pending).insert(
        StoreKind::RomanizedTitles,
        test_operation(
            StoreKind::RomanizedTitles,
            newer_save_order,
            None,
            Arc::new(|| Ok(())),
        ),
    );
    assert!(matches!(
        write_journal_intent_if_current(&old_delete, &pending).unwrap(),
        JournalAppend::Stale
    ));
    write_journal_intent(&JournalIntent::Replace {
        order: newer_save_order,
        kind: StoreKind::RomanizedTitles,
        path: path.clone(),
        bytes: serde_json::to_vec_pretty(&Tiny { value: 9 }).unwrap(),
    })
    .unwrap();
    assert_eq!(
        replay_journaled_snapshot(StoreKind::RomanizedTitles, &path, Tiny::default(), 1024,),
        Tiny { value: 9 }
    );
    clear_store_journal(&path);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn sidecar_before_record_cutoff_preserves_previous_replay() {
    #[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
    struct Tiny {
        value: u8,
    }

    let dir = temp_dir("sidecar-cutoff");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("tiny.json");
    let older_order = journal_order(10, 1);
    write_journal_intent(&JournalIntent::Replace {
        order: older_order,
        kind: StoreKind::Config,
        path: path.clone(),
        bytes: serde_json::to_vec_pretty(&Tiny { value: 5 }).unwrap(),
    })
    .unwrap();
    let newer_order = journal_order(20, 2);
    let _lock = acquire_intent_lock(&path).unwrap();
    let prepared = prepare_journal_record(&JournalIntent::Replace {
        order: newer_order,
        kind: StoreKind::Config,
        path: path.clone(),
        bytes: serde_json::to_vec_pretty(&Tiny { value: 6 }).unwrap(),
    })
    .unwrap();
    assert!(prepared.value.get("sidecar").is_some());
    drop(_lock);

    assert_eq!(
        replay_journaled_snapshot(StoreKind::Config, &path, Tiny::default(), 1024),
        Tiny { value: 5 }
    );
    assert!(
        unique_intent_sidecar_path(&path, older_order)
            .unwrap()
            .exists()
    );
    assert!(
        unique_intent_sidecar_path(&path, newer_order)
            .unwrap()
            .exists()
    );
    clear_store_journal(&path);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn old_commit_cannot_remove_a_newer_generation_or_sidecar() {
    #[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
    struct Tiny {
        value: u8,
    }

    let dir = temp_dir("conditional-commit");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("tiny.json");
    let old_order = journal_order(10, 1);
    let new_order = journal_order(20, 2);
    for (order, value) in [(old_order, 1), (new_order, 2)] {
        write_journal_intent(&JournalIntent::Replace {
            order,
            kind: StoreKind::Config,
            path: path.clone(),
            bytes: serde_json::to_vec_pretty(&Tiny { value }).unwrap(),
        })
        .unwrap();
    }

    commit_journal_generation(StoreKind::Config, &path, old_order).unwrap();

    assert_eq!(
        replay_journaled_snapshot(StoreKind::Config, &path, Tiny::default(), 1024),
        Tiny { value: 2 }
    );
    assert!(
        unique_intent_sidecar_path(&path, new_order)
            .unwrap()
            .exists()
    );
    clear_store_journal(&path);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn compaction_bounds_journal_and_orphans_while_preserving_latest() {
    #[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
    struct Tiny {
        value: u8,
    }

    let dir = temp_dir("bounded-compaction");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("tiny.json");
    let mut latest = None;
    for value in 1..=32_u8 {
        let order = journal_order(u64::from(value), value);
        latest = Some(order);
        write_journal_intent(&JournalIntent::Replace {
            order,
            kind: StoreKind::Config,
            path: path.clone(),
            bytes: serde_json::to_vec_pretty(&Tiny { value }).unwrap(),
        })
        .unwrap();
    }

    let journal = std::fs::read_to_string(intent_journal_path(&path).unwrap()).unwrap();
    assert_eq!(journal.lines().count(), 1);
    let sidecars = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .filter(|entry| {
            entry.file_name().to_str().is_some_and(|name| {
                name.starts_with("tiny.json.intent.") && name.ends_with(".json")
            })
        })
        .count();
    assert_eq!(sidecars, 1);
    assert_eq!(
        replay_journaled_snapshot(StoreKind::Config, &path, Tiny::default(), 1024),
        Tiny { value: 32 }
    );

    commit_journal_generation(StoreKind::Config, &path, latest.unwrap()).unwrap();
    let journal = std::fs::read_to_string(intent_journal_path(&path).unwrap()).unwrap();
    assert_eq!(
        journal.lines().count(),
        1,
        "the ordered commit frontier remains durable"
    );
    clear_store_journal(&path);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_torn_jsonl_tail_is_normalized_before_the_next_intent() {
    use std::io::Write as _;

    #[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
    struct Tiny {
        value: u8,
    }

    let dir = temp_dir("normalize-torn-tail");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("tiny.json");
    write_journal_intent(&JournalIntent::Replace {
        order: journal_order(1, 1),
        kind: StoreKind::Config,
        path: path.clone(),
        bytes: serde_json::to_vec_pretty(&Tiny { value: 1 }).unwrap(),
    })
    .unwrap();
    let journal_path = intent_journal_path(&path).unwrap();
    let mut journal = std::fs::OpenOptions::new()
        .append(true)
        .open(&journal_path)
        .unwrap();
    journal.write_all(b"{\"v\":1").unwrap();
    journal.sync_all().unwrap();
    drop(journal);

    write_journal_intent(&JournalIntent::Replace {
        order: journal_order(2, 2),
        kind: StoreKind::Config,
        path: path.clone(),
        bytes: serde_json::to_vec_pretty(&Tiny { value: 2 }).unwrap(),
    })
    .unwrap();

    let normalized = std::fs::read_to_string(&journal_path).unwrap();
    assert!(normalized.lines().count() <= 2);
    assert!(
        normalized
            .lines()
            .all(|line| serde_json::from_str::<serde_json::Value>(line).is_ok())
    );
    assert_eq!(intent_sidecar_count(&dir, "tiny.json"), 1);
    assert_eq!(
        replay_journaled_snapshot(StoreKind::Config, &path, Tiny::default(), 1024),
        Tiny { value: 2 }
    );
    clear_store_journal(&path);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn failed_prewrite_journal_replacement_removes_its_prepared_sidecar() {
    #[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
    struct Tiny {
        value: u8,
    }

    let dir = temp_dir("prewrite-cleanup");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("tiny.json");
    let old_order = journal_order(1, 1);
    write_journal_intent(&JournalIntent::Replace {
        order: old_order,
        kind: StoreKind::Config,
        path: path.clone(),
        bytes: serde_json::to_vec_pretty(&Tiny { value: 1 }).unwrap(),
    })
    .unwrap();
    let new_order = journal_order(2, 2);
    let new_intent = JournalIntent::Replace {
        order: new_order,
        kind: StoreKind::Config,
        path: path.clone(),
        bytes: serde_json::to_vec_pretty(&Tiny { value: 2 }).unwrap(),
    };

    for _ in 0..3 {
        let _lock = acquire_intent_lock(&path).unwrap();
        let record = prepare_journal_record(&new_intent).unwrap();
        let error =
            replace_journal_with_record_locked_by(StoreKind::Config, &path, &record, |_, _| {
                Err(std::io::Error::other(
                    "fault injection: disk full before rename",
                ))
            })
            .err()
            .expect("the prewrite failure is returned");
        assert!(error.to_string().contains("disk full"));
        assert!(
            !unique_intent_sidecar_path(&path, new_order)
                .unwrap()
                .exists()
        );
        assert_eq!(intent_sidecar_count(&dir, "tiny.json"), 1);
    }
    assert_eq!(
        replay_journaled_snapshot(StoreKind::Config, &path, Tiny::default(), 1024),
        Tiny { value: 1 }
    );
    clear_store_journal(&path);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn journal_read_failure_removes_each_newly_created_exact_sidecar() {
    let dir = temp_dir("read-failure-cleanup");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("tiny.json");
    let journal_path = intent_journal_path(&path).unwrap();
    crate::util::safe_fs::write_private_atomic(
        &journal_path,
        &vec![b'x'; INTENT_JOURNAL_MAX_BYTES as usize + 1],
    )
    .unwrap();

    for sequence in 1..=8_u64 {
        let order = journal_order(sequence, sequence.to_le_bytes()[0]);
        let intent = JournalIntent::Replace {
            order,
            kind: StoreKind::Config,
            path: path.clone(),
            bytes: format!("{{\"value\":{sequence}}}").into_bytes(),
        };
        let _lock = acquire_intent_lock(&path).unwrap();
        let record = prepare_journal_record(&intent).unwrap();
        assert!(record.created_sidecar.is_some());
        assert!(replace_journal_with_record_locked(StoreKind::Config, &path, &record).is_err());
        assert!(!unique_intent_sidecar_path(&path, order).unwrap().exists());
        assert_eq!(intent_sidecar_count(&dir, "tiny.json"), 0);
    }
    clear_store_journal(&path);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn sidecar_artifact_limit_is_a_hard_creation_bound() {
    let dir = temp_dir("sidecar-hard-cap");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("tiny.json");
    for index in 0..INTENT_SIDECAR_MAX_COUNT {
        std::fs::write(
            dir.join(format!("tiny.json.intent.orphan-{index}.json")),
            b"orphan",
        )
        .unwrap();
    }
    let order = journal_order(1, 1);
    let error = prepare_journal_record(&JournalIntent::Replace {
        order,
        kind: StoreKind::Config,
        path: path.clone(),
        bytes: br#"{"value":1}"#.to_vec(),
    })
    .err()
    .expect("the hard cap rejects another artifact");
    assert_eq!(error.kind(), std::io::ErrorKind::StorageFull);
    assert_eq!(
        intent_sidecar_count(&dir, "tiny.json"),
        INTENT_SIDECAR_MAX_COUNT
    );
    assert!(!unique_intent_sidecar_path(&path, order).unwrap().exists());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn rename_visible_journal_failure_keeps_both_durable_possibilities() {
    #[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
    struct Tiny {
        value: u8,
    }

    let dir = temp_dir("visible-failure-cleanup");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("tiny.json");
    let old_order = journal_order(1, 1);
    write_journal_intent(&JournalIntent::Replace {
        order: old_order,
        kind: StoreKind::Config,
        path: path.clone(),
        bytes: serde_json::to_vec_pretty(&Tiny { value: 1 }).unwrap(),
    })
    .unwrap();
    let journal_path = intent_journal_path(&path).unwrap();
    let old_journal = std::fs::read(&journal_path).unwrap();
    let old_sidecar = unique_intent_sidecar_path(&path, old_order).unwrap();
    let new_order = journal_order(2, 2);
    let new_intent = JournalIntent::Replace {
        order: new_order,
        kind: StoreKind::Config,
        path: path.clone(),
        bytes: serde_json::to_vec_pretty(&Tiny { value: 2 }).unwrap(),
    };

    {
        let _lock = acquire_intent_lock(&path).unwrap();
        let record = prepare_journal_record(&new_intent).unwrap();
        let error = replace_journal_with_record_locked_by(
            StoreKind::Config,
            &path,
            &record,
            |journal_path, bytes| {
                crate::util::safe_fs::write_private_atomic(journal_path, bytes)?;
                Err(std::io::Error::other(
                    "fault injection: parent sync after visible rename",
                ))
            },
        )
        .err()
        .expect("the visible rename failure is returned");
        assert!(error.to_string().contains("parent sync"));
        assert_eq!(intent_sidecar_count(&dir, "tiny.json"), 2);
        assert!(old_sidecar.exists());
        assert!(
            unique_intent_sidecar_path(&path, new_order)
                .unwrap()
                .exists()
        );
        let journal = std::fs::read_to_string(&journal_path).unwrap();
        assert!(journal.lines().count() <= 2);
    }
    assert_eq!(
        replay_journaled_snapshot(StoreKind::Config, &path, Tiny::default(), 1024),
        Tiny { value: 2 }
    );

    crate::util::safe_fs::write_private_atomic(&journal_path, &old_journal).unwrap();
    assert_eq!(
        replay_journaled_snapshot(StoreKind::Config, &path, Tiny::default(), 1024),
        Tiny { value: 1 },
        "a crash rollback to the previous journal must retain its payload"
    );

    write_journal_intent(&new_intent).unwrap();
    assert_eq!(intent_sidecar_count(&dir, "tiny.json"), 1);
    assert!(!old_sidecar.exists());
    assert_eq!(
        replay_journaled_snapshot(StoreKind::Config, &path, Tiny::default(), 1024),
        Tiny { value: 2 }
    );
    clear_store_journal(&path);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn delayed_old_cross_instance_writer_cannot_overtake_newer_frontier() {
    let dir = temp_dir("cross-instance-order");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("tiny.json");
    let old_order = journal_order_in_epoch(7, u128::MAX, 1);
    let new_order = journal_order_in_epoch(8, 1, 2);
    let intent = |order, value| JournalIntent::Replace {
        order,
        kind: StoreKind::Config,
        path: path.clone(),
        bytes: format!("{{\"value\":{value}}}").into_bytes(),
    };

    // Simulated process B journals and commits after accepting a newer operation. Process A
    // then completes its delayed journal append; the durable order key, not append order,
    // keeps B's commit frontier authoritative.
    write_journal_intent(&intent(new_order, 2)).unwrap();
    commit_journal_generation(StoreKind::Config, &path, new_order).unwrap();
    write_journal_intent(&intent(old_order, 1)).unwrap();
    let state = read_journal_state(StoreKind::Config, &path).unwrap();
    assert_eq!(state.committed_through, Some(new_order));
    assert!(state.candidate.is_none());

    let writes = Arc::new(AtomicUsize::new(0));
    let writer_writes = Arc::clone(&writes);
    let mut delayed_old = test_operation(
        StoreKind::Config,
        old_order,
        Some(path.clone()),
        Arc::new(move || {
            writer_writes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }),
    );
    delayed_old.resolve_journal_for_test();
    write_operation_durable(&delayed_old).unwrap();
    assert_eq!(writes.load(Ordering::SeqCst), 0);

    clear_store_journal(&path);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn process_epoch_orders_equal_and_backward_sequences_without_wall_clock() {
    let predecessor_equal = journal_order_in_epoch(7, 42, 1);
    let successor_equal = journal_order_in_epoch(8, 42, 2);
    assert!(successor_equal > predecessor_equal);

    let predecessor_high = journal_order_in_epoch(7, u128::MAX, 3);
    let successor_reset = journal_order_in_epoch(8, 1, 4);
    assert!(successor_reset > predecessor_high);
}

#[test]
fn process_epoch_recovers_from_lost_restored_and_corrupt_counters() {
    let dir = temp_dir("epoch-counter-recovery");
    std::fs::create_dir_all(&dir).unwrap();
    let counter = dir.join(".ytt-persist-order.json");

    assert_eq!(allocate_process_epoch_at(&counter).unwrap(), 1);
    assert_eq!(allocate_process_epoch_at(&counter).unwrap(), 2);
    std::fs::remove_file(&counter).unwrap();
    assert_eq!(
        allocate_process_epoch_at(&counter).unwrap(),
        3,
        "the durable marker prevents reuse after counter loss"
    );

    crate::util::safe_fs::write_private_atomic(
        &counter,
        serde_json::json!({ "v": 1, "last_epoch": "1" })
            .to_string()
            .as_bytes(),
    )
    .unwrap();
    assert_eq!(
        allocate_process_epoch_at(&counter).unwrap(),
        4,
        "restoring an older counter must not move below the marker frontier"
    );

    crate::util::safe_fs::write_private_atomic(&counter, b"not-json").unwrap();
    assert_eq!(
        allocate_process_epoch_at(&counter).unwrap(),
        5,
        "a corrupt counter recovers only because a durable marker is observable"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn process_epoch_scans_journal_frontiers_and_sidecar_names() {
    let dir = temp_dir("epoch-artifact-recovery");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("tiny.json");
    let counter = dir.join(".ytt-persist-order.json");
    let journal_order = journal_order_in_epoch(41, 9, 1);
    append_journal_record(&path, &commit_record(StoreKind::Config, journal_order)).unwrap();
    assert_eq!(allocate_process_epoch_at(&counter).unwrap(), 42);

    let sidecar_order = journal_order_in_epoch(55, 1, 2);
    let sidecar = unique_intent_sidecar_path(&path, sidecar_order).unwrap();
    crate::util::safe_fs::write_private_atomic(&sidecar, b"artifact").unwrap();
    assert_eq!(
        allocate_process_epoch_at(&counter).unwrap(),
        56,
        "a sidecar from a newer observed owner must raise the epoch frontier"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn process_epoch_and_acceptance_sequence_exhaustion_fail_explicitly() {
    let dir = temp_dir("epoch-exhaustion");
    std::fs::create_dir_all(&dir).unwrap();
    let counter = dir.join(".ytt-persist-order.json");
    crate::util::safe_fs::write_private_atomic(
        &counter,
        serde_json::json!({ "v": 1, "last_epoch": u64::MAX.to_string() })
            .to_string()
            .as_bytes(),
    )
    .unwrap();
    let epoch_error = allocate_process_epoch_at(&counter).unwrap_err();
    assert_eq!(epoch_error.kind(), std::io::ErrorKind::InvalidData);
    assert!(epoch_error.to_string().contains("exhausted"));

    let source = JournalOrderSource {
        process_epoch: 9,
        allocation_error: None,
        next_sequence: Mutex::new(u128::MAX - 1),
    };
    let last = source.accept();
    assert_eq!(last.order.sequence, u128::MAX);
    assert!(last.error.is_none());
    let exhausted = source.accept();
    assert_eq!(exhausted.order.sequence, u128::MAX);
    assert!(
        exhausted
            .error
            .as_deref()
            .is_some_and(|error| error.contains("sequence exhausted"))
    );
    let _ = std::fs::remove_dir_all(dir);
}
