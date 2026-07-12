use super::*;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use serde::{Deserialize, Serialize};

#[path = "tests/handle.rs"]
mod handle;
#[path = "tests/panic_shadow_seal.rs"]
mod panic_shadow_seal;
#[path = "tests/races.rs"]
mod races;
#[path = "tests/removal.rs"]
mod removal;

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
    let pending: SharedPending = Arc::new(Mutex::new(HashMap::new()));

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
fn rename_visible_journal_failure_keeps_only_the_referenced_sidecar() {
    #[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
    struct Tiny {
        value: u8,
    }

    let dir = temp_dir("visible-failure-cleanup");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("tiny.json");
    write_journal_intent(&JournalIntent::Replace {
        order: journal_order(1, 1),
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
        assert_eq!(intent_sidecar_count(&dir, "tiny.json"), 1);
        assert!(
            unique_intent_sidecar_path(&path, new_order)
                .unwrap()
                .exists()
        );
        let journal = std::fs::read_to_string(intent_journal_path(&path).unwrap()).unwrap();
        assert!(journal.lines().count() <= 2);
    }
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
    delayed_old.journaled = true;
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

#[tokio::test]
async fn write_stores_clears_stale_due_entries_when_snapshot_is_missing() {
    let pending: SharedPending = Arc::new(Mutex::new(HashMap::new()));
    let mut due = HashMap::from([(StoreKind::Library, tokio::time::Instant::now())]);
    let mut retries = HashMap::new();
    let events = Arc::new(Mutex::new(None));

    write_stores(&pending, &mut due, &mut retries, &events, false).await;

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
    let Some(OwnedSnapshot::Playlists(playlists)) = guard
        .get(&StoreKind::Playlists)
        .and_then(PendingOperation::snapshot)
    else {
        panic!("expected playlists snapshot");
    };
    let focus = playlists.find("Focus").expect("focus playlist");
    assert_eq!(focus.songs.len(), 1);
    assert_eq!(focus.songs[0].video_id, "id0");
}

#[tokio::test]
async fn write_stores_requeues_failed_snapshot_and_retries_until_success() {
    let pending: SharedPending = Arc::new(Mutex::new(HashMap::new()));
    let mut due = HashMap::from([(StoreKind::Config, tokio::time::Instant::now())]);
    let mut retries = HashMap::new();
    let events = Arc::new(Mutex::new(None));
    let attempts = Arc::new(AtomicUsize::new(0));
    let writer_attempts = Arc::clone(&attempts);
    lock(&pending).insert(
        StoreKind::Config,
        pending_save(Snapshot::Test {
            kind: StoreKind::Config,
            label: "config",
            storage_path: None,
            writer: Arc::new(move || {
                if writer_attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                    Err(std::io::Error::other("disk full"))
                } else {
                    Ok(())
                }
            }),
        }),
    );

    let clean = write_stores(&pending, &mut due, &mut retries, &events, false).await;

    assert!(!clean);
    assert!(lock(&pending).contains_key(&StoreKind::Config));
    assert!(due.contains_key(&StoreKind::Config));
    assert_eq!(retries[&StoreKind::Config].retry_count, 1);

    let clean = write_stores(&pending, &mut due, &mut retries, &events, true).await;

    assert!(clean);
    assert!(lock(&pending).is_empty());
    assert!(!retries.contains_key(&StoreKind::Config));
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn panicking_blocking_writer_is_requeued_and_can_succeed_on_retry() {
    let pending: SharedPending = Arc::new(Mutex::new(HashMap::new()));
    let mut due = HashMap::from([(StoreKind::Config, tokio::time::Instant::now())]);
    let mut retries = HashMap::new();
    let events = Arc::new(Mutex::new(None));
    let attempts = Arc::new(AtomicUsize::new(0));
    let writer_attempts = Arc::clone(&attempts);
    lock(&pending).insert(
        StoreKind::Config,
        pending_save(Snapshot::Test {
            kind: StoreKind::Config,
            label: "panicking config",
            storage_path: None,
            writer: Arc::new(move || {
                if writer_attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                    panic!("fault injection: blocking persistence writer panic");
                }
                Ok(())
            }),
        }),
    );

    assert!(!write_stores(&pending, &mut due, &mut retries, &events, false).await);
    assert!(lock(&pending).contains_key(&StoreKind::Config));
    assert!(due.contains_key(&StoreKind::Config));

    assert!(write_stores(&pending, &mut due, &mut retries, &events, true).await);
    assert!(lock(&pending).is_empty());
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn disk_full_during_write_preserves_a_newer_coalesced_snapshot() {
    let pending: SharedPending = Arc::new(Mutex::new(HashMap::new()));
    let captured_events = Arc::new(Mutex::new(Vec::new()));
    let event_log = Arc::clone(&captured_events);
    let events: EventSinkSlot = Arc::new(Mutex::new(Some(Arc::new(move |event| {
        event_log
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(event);
    }))));
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let release_rx = Arc::new(Mutex::new(release_rx));
    let writer_release = Arc::clone(&release_rx);
    lock(&pending).insert(
        StoreKind::Config,
        pending_save(Snapshot::Test {
            kind: StoreKind::Config,
            label: "older config",
            storage_path: None,
            writer: Arc::new(move || {
                started_tx.send(()).expect("test observes writer start");
                writer_release
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .recv_timeout(Duration::from_secs(5))
                    .expect("test releases injected disk write");
                Err(std::io::Error::other(
                    "fault injection: no space left on device",
                ))
            }),
        }),
    );

    let write_pending = Arc::clone(&pending);
    let write_events = Arc::clone(&events);
    let write_task = tokio::spawn(async move {
        let mut due = HashMap::from([(StoreKind::Config, tokio::time::Instant::now())]);
        let mut retries = HashMap::new();
        let clean =
            write_stores(&write_pending, &mut due, &mut retries, &write_events, false).await;
        (clean, due, retries)
    });

    tokio::task::spawn_blocking(move || {
        started_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("injected writer starts")
    })
    .await
    .expect("writer-start observer joins");
    lock(&pending).insert(
        StoreKind::Config,
        pending_save(Snapshot::Test {
            kind: StoreKind::Config,
            label: "newer config",
            storage_path: None,
            writer: Arc::new(|| Ok(())),
        }),
    );
    release_tx.send(()).expect("release injected writer");

    let (clean, mut due, mut retries) = write_task.await.expect("write task joins");
    assert!(!clean, "the newer snapshot is still dirty");
    {
        let guard = lock(&pending);
        let Some(OwnedSnapshot::Test { label, .. }) = guard
            .get(&StoreKind::Config)
            .and_then(PendingOperation::snapshot)
        else {
            panic!("expected injected config snapshot");
        };
        assert_eq!(*label, "newer config");
    }
    {
        let failures = captured_events
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        assert_eq!(failures.len(), 1);
        let PersistEvent::WriteFailed { store, error } = &failures[0];
        assert_eq!(*store, StoreKind::Config);
        assert!(error.contains("no space left on device"));
    }

    assert!(
        write_stores(&pending, &mut due, &mut retries, &events, true).await,
        "a later flush writes the retained latest snapshot"
    );
    assert!(lock(&pending).is_empty());
    assert!(!retries.contains_key(&StoreKind::Config));
}

#[test]
fn failed_snapshot_does_not_overwrite_newer_pending_snapshot() {
    let pending: SharedPending = Arc::new(Mutex::new(HashMap::new()));
    let mut due = HashMap::new();
    let mut retries = HashMap::new();
    let events = Arc::new(Mutex::new(None));
    lock(&pending).insert(
        StoreKind::Config,
        pending_save(Snapshot::Test {
            kind: StoreKind::Config,
            label: "newer",
            storage_path: None,
            writer: Arc::new(|| Ok(())),
        }),
    );

    requeue_failed_operation(
        &pending,
        &mut due,
        &mut retries,
        &events,
        pending_save(Snapshot::Test {
            kind: StoreKind::Config,
            label: "older",
            storage_path: None,
            writer: Arc::new(|| Ok(())),
        }),
        "transient".to_owned(),
    );

    let guard = lock(&pending);
    let Some(OwnedSnapshot::Test { label, .. }) = guard
        .get(&StoreKind::Config)
        .and_then(PendingOperation::snapshot)
    else {
        panic!("expected test snapshot");
    };
    assert_eq!(*label, "newer");
}

#[tokio::test]
async fn flush_returns_false_when_write_keeps_failing() {
    let handle = spawn();
    let _ = handle
        .save(Snapshot::Test {
            kind: StoreKind::Config,
            label: "config",
            storage_path: None,
            writer: Arc::new(|| Err(std::io::Error::other("still full"))),
        })
        .unwrap();

    assert!(!handle.flush(Duration::from_secs(1)).await);
    assert!(lock(&handle.pending().inner).contains_key(&StoreKind::Config));
}

#[tokio::test]
async fn first_high_value_failure_emits_one_status_event() {
    let handle = spawn();
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&events);
    handle.set_event_sink(move |event| {
        captured.lock().unwrap().push(event);
    });
    let _ = handle
        .save(Snapshot::Test {
            kind: StoreKind::Library,
            label: "library",
            storage_path: None,
            writer: Arc::new(|| Err(std::io::Error::other("permission denied"))),
        })
        .unwrap();

    assert!(!handle.flush(Duration::from_secs(1)).await);
    assert!(!handle.flush(Duration::from_secs(1)).await);

    let guard = events.lock().unwrap();
    assert_eq!(guard.len(), 1);
    let PersistEvent::WriteFailed { store, error } = &guard[0];
    assert_eq!(*store, StoreKind::Library);
    assert!(error.contains("permission denied"));
}

#[tokio::test]
async fn delete_is_latest_wins_with_save_in_both_orders() {
    let (tx, _rx) = crate::util::backpressure::bounded_channel(
        crate::util::backpressure::PERSIST_CONTROL_QUEUE,
    );
    let pending: SharedPending = Arc::new(Mutex::new(HashMap::new()));
    let handle = PersistHandle {
        tx,
        pending: Arc::clone(&pending),
        inflight: Arc::new(Mutex::new(HashMap::new())),
        dirty: Arc::new(Notify::new()),
        events: Arc::new(Mutex::new(None)),
        order_source: test_order_source(),
        admission_open: Arc::new(AtomicBool::new(true)),
        panic_shadow: Arc::new(PanicShadow::new()),
    };
    let saves = Arc::new(AtomicUsize::new(0));
    let old_save_count = Arc::clone(&saves);
    let deletes = Arc::new(AtomicUsize::new(0));
    let delete_count = Arc::clone(&deletes);
    let _ = handle
        .save(Snapshot::Test {
            kind: StoreKind::RomanizedTitles,
            label: "romanized title cache",
            storage_path: None,
            writer: Arc::new(move || {
                old_save_count.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }),
        })
        .unwrap();
    handle.delete_romanized_titles_with(move || {
        delete_count.fetch_add(1, Ordering::SeqCst);
        Ok(())
    });

    let mut due = HashMap::new();
    let mut retries = HashMap::new();
    assert!(write_stores(&pending, &mut due, &mut retries, &handle.events, true).await);
    assert_eq!(saves.load(Ordering::SeqCst), 0);
    assert_eq!(deletes.load(Ordering::SeqCst), 1);

    let replaced_delete_count = Arc::clone(&deletes);
    handle.delete_romanized_titles_with(move || {
        replaced_delete_count.fetch_add(1, Ordering::SeqCst);
        Ok(())
    });
    let new_save_count = Arc::clone(&saves);
    let _ = handle
        .save(Snapshot::Test {
            kind: StoreKind::RomanizedTitles,
            label: "newer romanized title cache",
            storage_path: None,
            writer: Arc::new(move || {
                new_save_count.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }),
        })
        .unwrap();

    assert!(write_stores(&pending, &mut due, &mut retries, &handle.events, true).await);
    assert_eq!(deletes.load(Ordering::SeqCst), 1);
    assert_eq!(saves.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn delete_queued_during_an_older_save_runs_after_that_save() {
    let pending: SharedPending = Arc::new(Mutex::new(HashMap::new()));
    let events = Arc::new(Mutex::new(None));
    let order = Arc::new(Mutex::new(Vec::new()));
    let save_order = Arc::clone(&order);
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let release_rx = Arc::new(Mutex::new(release_rx));
    let save_release = Arc::clone(&release_rx);
    lock(&pending).insert(
        StoreKind::RomanizedTitles,
        pending_save(Snapshot::Test {
            kind: StoreKind::RomanizedTitles,
            label: "older romanized title cache",
            storage_path: None,
            writer: Arc::new(move || {
                started_tx.send(()).expect("test observes save start");
                save_release
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .recv_timeout(Duration::from_secs(5))
                    .expect("test releases old save");
                save_order.lock().unwrap().push("save");
                Ok(())
            }),
        }),
    );
    let write_pending = Arc::clone(&pending);
    let write_events = Arc::clone(&events);
    let write_task = tokio::spawn(async move {
        let mut due = HashMap::from([(StoreKind::RomanizedTitles, tokio::time::Instant::now())]);
        let mut retries = HashMap::new();
        let clean =
            write_stores(&write_pending, &mut due, &mut retries, &write_events, false).await;
        (clean, due, retries)
    });
    tokio::task::spawn_blocking(move || {
        started_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("old save starts")
    })
    .await
    .expect("save-start observer joins");

    let (tx, _rx) = crate::util::backpressure::bounded_channel(
        crate::util::backpressure::PERSIST_CONTROL_QUEUE,
    );
    let handle = PersistHandle {
        tx,
        pending: Arc::clone(&pending),
        inflight: Arc::new(Mutex::new(HashMap::new())),
        dirty: Arc::new(Notify::new()),
        events: Arc::clone(&events),
        order_source: test_order_source(),
        admission_open: Arc::new(AtomicBool::new(true)),
        panic_shadow: Arc::new(PanicShadow::new()),
    };
    let delete_order = Arc::clone(&order);
    handle.delete_romanized_titles_with(move || {
        delete_order.lock().unwrap().push("delete");
        Ok(())
    });
    release_tx.send(()).expect("release old save");

    let (clean, mut due, mut retries) = write_task.await.expect("old save task joins");
    assert!(!clean, "the newer delete must remain pending");
    assert!(
        write_stores(&pending, &mut due, &mut retries, &events, true).await,
        "the newer delete completes on the next drain"
    );
    assert_eq!(*order.lock().unwrap(), ["save", "delete"]);
}

#[tokio::test]
async fn failed_delete_remains_pending_and_flush_retry_confirms_success() {
    let handle = spawn();
    let attempts = Arc::new(AtomicUsize::new(0));
    let delete_attempts = Arc::clone(&attempts);
    let fail = Arc::new(AtomicBool::new(true));
    let delete_fail = Arc::clone(&fail);
    handle.delete_romanized_titles_with(move || {
        delete_attempts.fetch_add(1, Ordering::SeqCst);
        if delete_fail.load(Ordering::SeqCst) {
            Err(std::io::Error::other(
                "fault injection: read-only filesystem",
            ))
        } else {
            Ok(())
        }
    });

    assert!(!handle.flush(Duration::from_secs(1)).await);
    assert!(lock(&handle.pending().inner).contains_key(&StoreKind::RomanizedTitles));
    fail.store(false, Ordering::SeqCst);
    assert!(handle.flush(Duration::from_secs(1)).await);
    assert!(attempts.load(Ordering::SeqCst) >= 2);
    assert!(!lock(&handle.pending().inner).contains_key(&StoreKind::RomanizedTitles));
}

#[tokio::test]
async fn saturated_control_queue_cannot_lose_delete_before_immediate_flush() {
    let (tx, rx) = crate::util::backpressure::bounded_channel(
        crate::util::backpressure::PERSIST_CONTROL_QUEUE,
    );
    let capacity = crate::util::backpressure::PERSIST_CONTROL_QUEUE
        .capacity()
        .expect("bounded control queue");
    for _ in 0..capacity {
        let (ack, ack_rx) = oneshot::channel();
        drop(ack_rx);
        tx.try_send(PersistMsg::Flush(ack))
            .expect("prefill persist control queue");
    }
    let pending: SharedPending = Arc::new(Mutex::new(HashMap::new()));
    let inflight: SharedInflight = Arc::new(Mutex::new(HashMap::new()));
    let dirty = Arc::new(Notify::new());
    let events = Arc::new(Mutex::new(None));
    let handle = PersistHandle {
        tx,
        pending: Arc::clone(&pending),
        inflight: Arc::clone(&inflight),
        dirty: Arc::clone(&dirty),
        events: Arc::clone(&events),
        order_source: test_order_source(),
        admission_open: Arc::new(AtomicBool::new(true)),
        panic_shadow: Arc::new(PanicShadow::new()),
    };
    let deletes = Arc::new(AtomicUsize::new(0));
    let delete_count = Arc::clone(&deletes);
    handle.delete_romanized_titles_with(move || {
        delete_count.fetch_add(1, Ordering::SeqCst);
        Ok(())
    });
    let actor = tokio::spawn(run_actor(
        rx,
        pending,
        inflight,
        dirty,
        events,
        Arc::clone(&handle.panic_shadow),
    ));

    assert!(handle.flush(Duration::from_secs(2)).await);
    assert_eq!(
        deletes.load(Ordering::SeqCst),
        1,
        "flush must not acknowledge before the shared delete is applied"
    );
    drop(handle);
    actor.await.expect("persist actor stops after sender drop");
}

#[tokio::test]
async fn flush_acknowledges_when_there_is_no_pending_work() {
    let handle = spawn();

    assert!(handle.flush(Duration::from_secs(1)).await);
    assert!(lock(&handle.pending().inner).is_empty());
}
