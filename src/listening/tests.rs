use std::collections::BTreeMap;

use crate::personal_state::{
    CausalStamp, DeviceId, Dot, Operation, OperationEnvelope, OperationOrigin,
    PERSONAL_STATE_LISTENING_SCHEMA_VERSION, PersonalStateError, PersonalStateV2, PortableTrack,
    PortableTrackKey, VersionVector,
};
use crate::streaming::TasteSnapshot;

use super::*;

fn device(value: &str) -> DeviceId {
    DeviceId::new(value).unwrap()
}

fn track(id: &str) -> PortableTrack {
    PortableTrack {
        key: PortableTrackKey::Catalog {
            provider: "ytmusic".to_owned(),
            exact_catalog_id: id.to_owned(),
        },
        title: format!("Track {id}"),
        artist: "Artist".to_owned(),
        album: None,
        duration_secs: Some(7_200),
        isrc: None,
    }
}

fn stamp(device_id: &str, sequence: u64, observed: &[(&str, u64)]) -> CausalStamp {
    CausalStamp {
        dot: Dot {
            device_id: device(device_id),
            sequence,
        },
        observed: VersionVector(
            observed
                .iter()
                .map(|(id, sequence)| (device(id), *sequence))
                .collect::<BTreeMap<_, _>>(),
        ),
        recorded_at_unix: 0,
    }
}

fn envelope(
    operation_id: &str,
    device_id: &str,
    sequence: u64,
    observed: &[(&str, u64)],
    change: ListeningOperation,
) -> OperationEnvelope {
    OperationEnvelope {
        operation_id: operation_id.to_owned(),
        stamp: stamp(device_id, sequence, observed),
        origin: OperationOrigin::Local,
        operation: Operation::Listening { change },
    }
}

fn state(operations: Vec<OperationEnvelope>) -> PersonalStateV2 {
    let mut state = PersonalStateV2::empty("listening-tests".to_owned()).unwrap();
    state.schema_version = PERSONAL_STATE_LISTENING_SCHEMA_VERSION;
    for operation in &operations {
        state.version_vector.merge(&operation.stamp.observed);
        state.version_vector.observe(&operation.stamp.dot);
    }
    state.operations = operations;
    state
}

fn bookmark(id: &str, label: &str, position_ms: u64) -> BookmarkRecord {
    BookmarkRecord {
        bookmark_id: BookmarkId::new(id).unwrap(),
        track: track("long-form"),
        position_ms,
        label: label.to_owned(),
    }
}

fn provenance(device_id: &str, session: &str) -> ResumeProvenance {
    ResumeProvenance {
        playback_session_id: session.to_owned(),
        device_id: device(device_id),
    }
}

fn visit(first: i64, last: i64, country: Option<&str>) -> PassportVisit {
    PassportVisit {
        station_uuid: "radio-1".to_owned(),
        station_name: "Station".to_owned(),
        country_code: country.map(str::to_owned),
        first_listened_at_unix: first,
        last_listened_at_unix: last,
    }
}

#[test]
fn independent_bookmarks_and_concurrent_edits_converge() {
    let operations = vec![
        envelope(
            "a-create",
            "a",
            1,
            &[],
            ListeningOperation::UpsertBookmark {
                bookmark: bookmark("one", "first", 10_000),
            },
        ),
        envelope(
            "b-create",
            "b",
            1,
            &[],
            ListeningOperation::UpsertBookmark {
                bookmark: bookmark("two", "second", 20_000),
            },
        ),
        envelope(
            "a-edit",
            "a",
            2,
            &[("a", 1)],
            ListeningOperation::UpsertBookmark {
                bookmark: bookmark("one", "left edit", 11_000),
            },
        ),
        envelope(
            "b-edit",
            "b",
            2,
            &[("a", 1), ("b", 1)],
            ListeningOperation::UpsertBookmark {
                bookmark: bookmark("one", "right edit", 9_000),
            },
        ),
    ];
    let forward = ListeningProjection::from_ledger(&state(operations.clone())).unwrap();
    let mut reversed = operations;
    reversed.reverse();
    let backward = ListeningProjection::from_ledger(&state(reversed)).unwrap();

    assert_eq!(forward, backward);
    assert_eq!(forward.bookmarks.len(), 2);
    let variants = &forward.bookmarks[&BookmarkId::new("one").unwrap()];
    assert_eq!(variants.len(), 2);
    assert!(variants.iter().any(|row| row.position_ms == 9_000));
    assert!(variants.iter().any(|row| row.position_ms == 11_000));
}

#[test]
fn resolving_edit_observes_and_replaces_all_bookmark_variants() {
    let operations = vec![
        envelope(
            "left",
            "a",
            1,
            &[],
            ListeningOperation::UpsertBookmark {
                bookmark: bookmark("same", "left", 1_000),
            },
        ),
        envelope(
            "right",
            "b",
            1,
            &[],
            ListeningOperation::UpsertBookmark {
                bookmark: bookmark("same", "right", 2_000),
            },
        ),
        envelope(
            "resolved",
            "a",
            2,
            &[("a", 1), ("b", 1)],
            ListeningOperation::UpsertBookmark {
                bookmark: bookmark("same", "resolved", 1_500),
            },
        ),
    ];
    let projection = ListeningProjection::from_ledger(&state(operations)).unwrap();
    let variants = &projection.bookmarks[&BookmarkId::new("same").unwrap()];
    assert_eq!(variants.as_slice(), &[bookmark("same", "resolved", 1_500)]);
}

#[test]
fn concurrent_delete_wins_and_later_observed_edit_recreates() {
    let deleted = vec![
        envelope(
            "create",
            "a",
            1,
            &[],
            ListeningOperation::UpsertBookmark {
                bookmark: bookmark("same", "created", 1_000),
            },
        ),
        envelope(
            "edit",
            "a",
            2,
            &[("a", 1)],
            ListeningOperation::UpsertBookmark {
                bookmark: bookmark("same", "offline edit", 2_000),
            },
        ),
        envelope(
            "delete",
            "b",
            1,
            &[("a", 1)],
            ListeningOperation::DeleteBookmark {
                bookmark_id: BookmarkId::new("same").unwrap(),
            },
        ),
    ];
    let projection = ListeningProjection::from_ledger(&state(deleted.clone())).unwrap();
    assert!(projection.bookmarks.is_empty());

    let mut recreated = deleted;
    recreated.push(envelope(
        "recreate",
        "b",
        2,
        &[("a", 2), ("b", 1)],
        ListeningOperation::UpsertBookmark {
            bookmark: bookmark("same", "new", 3_000),
        },
    ));
    let projection = ListeningProjection::from_ledger(&state(recreated)).unwrap();
    assert_eq!(
        projection.bookmarks[&BookmarkId::new("same").unwrap()].as_slice(),
        &[bookmark("same", "new", 3_000)]
    );
}

#[test]
fn resume_uses_causality_for_rewinds_and_keeps_concurrent_clear() {
    let media = track("resume");
    let operations = vec![
        envelope(
            "high",
            "a",
            1,
            &[],
            ListeningOperation::SetResume {
                point: ResumePoint {
                    track: media.clone(),
                    position_ms: 600_000,
                    provenance: provenance("a", "session-a"),
                },
            },
        ),
        envelope(
            "rewind",
            "a",
            2,
            &[("a", 1)],
            ListeningOperation::SetResume {
                point: ResumePoint {
                    track: media.clone(),
                    position_ms: 120_000,
                    provenance: provenance("a", "session-a"),
                },
            },
        ),
        envelope(
            "clear",
            "b",
            1,
            &[],
            ListeningOperation::ClearResume {
                clear: ResumeClear {
                    track: media.clone(),
                    provenance: provenance("b", "session-b"),
                },
            },
        ),
    ];
    let projection = ListeningProjection::from_ledger(&state(operations)).unwrap();
    let resume = &projection.resumes[&media.key];
    assert!(resume.is_conflicted());
    assert_eq!(resume.candidates.len(), 2);
    assert!(resume.automatic_point().is_none());
    assert!(resume.candidates.iter().any(|candidate| matches!(
        candidate,
        ResumeCandidate::Position(point) if point.position_ms == 120_000
    )));
    assert!(
        resume
            .candidates
            .iter()
            .any(|candidate| matches!(candidate, ResumeCandidate::Clear(_)))
    );
}

#[test]
fn unique_clear_is_unambiguous_but_never_automatic() {
    let media = track("resume");
    let operations = vec![
        envelope(
            "position",
            "a",
            1,
            &[],
            ListeningOperation::SetResume {
                point: ResumePoint {
                    track: media.clone(),
                    position_ms: 80_000,
                    provenance: provenance("a", "session"),
                },
            },
        ),
        envelope(
            "clear",
            "a",
            2,
            &[("a", 1)],
            ListeningOperation::ClearResume {
                clear: ResumeClear {
                    track: media.clone(),
                    provenance: provenance("a", "session"),
                },
            },
        ),
    ];
    let projection = ListeningProjection::from_ledger(&state(operations)).unwrap();
    let resume = &projection.resumes[&media.key];
    assert_eq!(resume.candidates.len(), 1);
    assert!(!resume.is_conflicted());
    assert!(resume.automatic_point().is_none());
}

#[test]
fn visit_merge_does_not_cross_a_delete_tombstone() {
    let operations = vec![
        envelope(
            "old-visit",
            "a",
            1,
            &[],
            ListeningOperation::RecordPassportVisit {
                visit: visit(10, 20, Some("KR")),
            },
        ),
        envelope(
            "delete",
            "b",
            1,
            &[("a", 1)],
            ListeningOperation::DeletePassportVisit {
                station_uuid: "radio-1".to_owned(),
            },
        ),
        envelope(
            "new-visit",
            "a",
            2,
            &[("a", 1), ("b", 1)],
            ListeningOperation::RecordPassportVisit {
                visit: visit(50, 50, None),
            },
        ),
        envelope(
            "concurrent-visit",
            "b",
            2,
            &[("a", 1), ("b", 1)],
            ListeningOperation::RecordPassportVisit {
                visit: visit(40, 60, Some("US")),
            },
        ),
    ];
    let projection = ListeningProjection::from_ledger(&state(operations)).unwrap();
    let merged = &projection.passport_visits["radio-1"];
    assert_eq!(merged.first_listened_at_unix, 40);
    assert_eq!(merged.last_listened_at_unix, 60);
    assert_eq!(merged.country_code.as_deref(), Some("US"));
}

#[test]
fn sequential_visit_updates_keep_the_oldest_first_timestamp() {
    let operations = vec![
        envelope(
            "first-visit",
            "a",
            1,
            &[],
            ListeningOperation::RecordPassportVisit {
                visit: visit(10, 10, Some("KR")),
            },
        ),
        envelope(
            "later-visit",
            "a",
            2,
            &[("a", 1)],
            ListeningOperation::RecordPassportVisit {
                visit: visit(50, 50, None),
            },
        ),
    ];
    let projection = ListeningProjection::from_ledger(&state(operations)).unwrap();
    let merged = &projection.passport_visits["radio-1"];
    assert_eq!(merged.first_listened_at_unix, 10);
    assert_eq!(merged.last_listened_at_unix, 50);
    assert_eq!(merged.country_code.as_deref(), Some("KR"));
}

#[test]
fn notes_are_independent_from_visit_deletion() {
    let operations = vec![
        envelope(
            "visit",
            "a",
            1,
            &[],
            ListeningOperation::RecordPassportVisit {
                visit: visit(10, 20, Some("KR")),
            },
        ),
        envelope(
            "note",
            "a",
            2,
            &[("a", 1)],
            ListeningOperation::SetPassportNote {
                note: PassportNote {
                    station_uuid: "radio-1".to_owned(),
                    note: "Evening program".to_owned(),
                },
            },
        ),
        envelope(
            "delete-visit",
            "a",
            3,
            &[("a", 2)],
            ListeningOperation::DeletePassportVisit {
                station_uuid: "radio-1".to_owned(),
            },
        ),
    ];
    let projection = ListeningProjection::from_ledger(&state(operations)).unwrap();
    assert!(projection.passport_visits.is_empty());
    assert_eq!(projection.passport_notes["radio-1"].len(), 1);
}

#[test]
fn clear_passport_is_global_remove_wins_and_allows_later_visits() {
    let operations = vec![
        envelope(
            "visit",
            "a",
            1,
            &[],
            ListeningOperation::RecordPassportVisit {
                visit: visit(10, 20, Some("KR")),
            },
        ),
        envelope(
            "note",
            "a",
            2,
            &[("a", 1)],
            ListeningOperation::SetPassportNote {
                note: PassportNote {
                    station_uuid: "radio-1".to_owned(),
                    note: "Before clear".to_owned(),
                },
            },
        ),
        envelope(
            "clear-all",
            "b",
            1,
            &[("a", 1)],
            ListeningOperation::ClearPassport,
        ),
        envelope(
            "new-visit",
            "b",
            2,
            &[("a", 2), ("b", 1)],
            ListeningOperation::RecordPassportVisit {
                visit: visit(50, 50, Some("US")),
            },
        ),
    ];
    let projection = ListeningProjection::from_ledger(&state(operations)).unwrap();
    assert_eq!(
        projection.passport_visits["radio-1"],
        visit(50, 50, Some("US"))
    );
    assert!(projection.passport_notes.is_empty());
}

#[test]
fn concurrent_preset_edits_remain_atomic_variants() {
    let preset_id = DjPresetId::new("focus").unwrap();
    let operations = vec![
        envelope(
            "left-preset",
            "a",
            1,
            &[],
            ListeningOperation::UpsertDjPreset {
                preset: DjPreset {
                    preset_id: preset_id.clone(),
                    name: "Left".to_owned(),
                    snapshot: TasteSnapshot::default(),
                },
            },
        ),
        envelope(
            "right-preset",
            "b",
            1,
            &[],
            ListeningOperation::UpsertDjPreset {
                preset: DjPreset {
                    preset_id: preset_id.clone(),
                    name: "Right".to_owned(),
                    snapshot: TasteSnapshot::default(),
                },
            },
        ),
    ];
    let projection = ListeningProjection::from_ledger(&state(operations)).unwrap();
    let variants = &projection.dj_presets[&preset_id];
    assert_eq!(variants.len(), 2);
    assert!(variants.iter().any(|preset| preset.name == "Left"));
    assert!(variants.iter().any(|preset| preset.name == "Right"));
}

#[test]
fn operation_serde_requires_validation_and_rejects_locations() {
    let operation = ListeningOperation::UpsertDjPreset {
        preset: DjPreset {
            preset_id: DjPresetId::new("quiet").unwrap(),
            name: "Quiet".to_owned(),
            snapshot: TasteSnapshot::default(),
        },
    };
    let encoded = serde_json::to_vec(&operation).unwrap();
    let decoded: ListeningOperation = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(decoded, operation);
    decoded.validate().unwrap();

    let unsafe_label = ListeningOperation::UpsertBookmark {
        bookmark: bookmark("unsafe", "file:///Users/example/secret.mp3", 10_000),
    };
    assert_eq!(
        unsafe_label.validate(),
        Err(PersonalStateError::InvalidOperation(
            "listening records must not contain paths or URLs"
        ))
    );
}

#[test]
fn recreation_never_revives_a_concurrent_offline_edit_in_any_delivery_order() {
    let operations = [
        envelope(
            "delete",
            "a",
            1,
            &[],
            ListeningOperation::DeleteBookmark {
                bookmark_id: BookmarkId::new("one").unwrap(),
            },
        ),
        envelope(
            "recreate",
            "a",
            2,
            &[("a", 1)],
            ListeningOperation::UpsertBookmark {
                bookmark: bookmark("one", "Recreated", 20_000),
            },
        ),
        envelope(
            "offline",
            "b",
            1,
            &[],
            ListeningOperation::UpsertBookmark {
                bookmark: bookmark("one", "Offline edit", 10_000),
            },
        ),
    ];
    for order in [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ] {
        let projection =
            ListeningProjection::from_ledger(&state(order.map(|i| operations[i].clone()).to_vec()))
                .unwrap();
        assert_eq!(
            projection.bookmarks[&BookmarkId::new("one").unwrap()],
            vec![bookmark("one", "Recreated", 20_000)],
            "order {order:?}"
        );
    }
}

#[test]
fn global_clear_filters_offline_notes_even_after_a_new_note_in_any_order() {
    let note = |text: &str| PassportNote {
        station_uuid: "radio-1".to_owned(),
        note: text.to_owned(),
    };
    let operations = [
        envelope("clear", "a", 1, &[], ListeningOperation::ClearPassport),
        envelope(
            "recreate",
            "a",
            2,
            &[("a", 1)],
            ListeningOperation::SetPassportNote {
                note: note("After clear"),
            },
        ),
        envelope(
            "offline",
            "b",
            1,
            &[],
            ListeningOperation::SetPassportNote {
                note: note("Offline edit"),
            },
        ),
    ];
    for order in [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ] {
        let projection =
            ListeningProjection::from_ledger(&state(order.map(|i| operations[i].clone()).to_vec()))
                .unwrap();
        assert_eq!(
            projection.passport_notes["radio-1"],
            vec![note("After clear")],
            "order {order:?}"
        );
    }
}

#[test]
fn portable_track_metadata_cannot_leak_machine_locations() {
    for location in [
        "/Users/alice/Music/private.mp3",
        "C:\\Users\\alice\\Music\\private.mp3",
        "file:///private/file.mp3",
    ] {
        for field in 0..3 {
            let mut record = bookmark("private", "Point", 30_000);
            match field {
                0 => record.track.title = location.to_owned(),
                1 => record.track.artist = location.to_owned(),
                _ => record.track.album = Some(location.to_owned()),
            }
            assert!(
                ListeningOperation::UpsertBookmark { bookmark: record }
                    .validate()
                    .is_err()
            );
        }
    }
    let song = crate::api::Song::local_file("/Users/alice/Music/ .mp3".into());
    let mut record = bookmark("fallback", "Point", 30_000);
    record.track.title = song.title;
    assert!(
        ListeningOperation::UpsertBookmark { bookmark: record }
            .validate()
            .is_err()
    );
}
