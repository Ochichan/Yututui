use super::*;

#[test]
fn local_capacity_marks_all_pending_rows_after_999_unique_matches() {
    let mut tracks = (0..crate::playlists::SONGS_PER_PLAYLIST_MAX)
        .map(|index| {
            entry(
                input(&format!("Matched {index}"), &["Artist"]),
                Some(matched(&format!("video-{index}"))),
            )
        })
        .collect::<Vec<_>>();
    tracks.push(entry(input("Pending A", &["Artist"]), None));
    tracks.push(entry(input("Pending B", &["Artist"]), None));
    let mut cp = Checkpoint::new(
        "job_capacity_no_persist".to_owned(),
        spec(TransferDest::LocalPlaylist {
            name: Some("Capacity".to_owned()),
        }),
        tracks,
    );
    let mut unique_keys = HashSet::new();

    extend_matched_keys(&mut unique_keys, &cp.tracks);
    assert_eq!(unique_keys.len(), crate::playlists::SONGS_PER_PLAYLIST_MAX);
    let skipped = if unique_keys.len() >= crate::playlists::SONGS_PER_PLAYLIST_MAX {
        mark_pending_capacity_skipped(&mut cp)
    } else {
        Vec::new()
    };

    assert_eq!(
        skipped,
        vec![
            crate::playlists::SONGS_PER_PLAYLIST_MAX,
            crate::playlists::SONGS_PER_PLAYLIST_MAX + 1,
        ]
    );
    assert!(matches!(
        cp.tracks[crate::playlists::SONGS_PER_PLAYLIST_MAX].outcome,
        Some(MatchOutcome::SkippedCapacity)
    ));
    assert!(matches!(
        cp.tracks[crate::playlists::SONGS_PER_PLAYLIST_MAX + 1].outcome,
        Some(MatchOutcome::SkippedCapacity)
    ));
    assert_eq!(cp.match_stats.capacity_skipped, 2);
    assert_eq!(
        cp.match_stats.terminal_reasons.get("destination_capacity"),
        Some(&1)
    );
}

#[test]
fn duplicate_match_keys_do_not_consume_local_playlist_capacity() {
    let mut tracks = (0..crate::playlists::SONGS_PER_PLAYLIST_MAX)
        .map(|index| {
            let key_index = if index + 1 == crate::playlists::SONGS_PER_PLAYLIST_MAX {
                0
            } else {
                index
            };
            entry(
                input(&format!("Matched {index}"), &["Artist"]),
                Some(matched(&format!("video-{key_index}"))),
            )
        })
        .collect::<Vec<_>>();
    tracks.push(entry(input("Still Pending", &["Artist"]), None));
    let mut cp = Checkpoint::new(
        "job_duplicate_capacity_no_persist".to_owned(),
        spec(TransferDest::LocalPlaylist {
            name: Some("Capacity".to_owned()),
        }),
        tracks,
    );
    let mut unique_keys = HashSet::new();

    extend_matched_keys(&mut unique_keys, &cp.tracks);
    assert_eq!(
        unique_keys.len(),
        crate::playlists::SONGS_PER_PLAYLIST_MAX - 1
    );
    if unique_keys.len() >= crate::playlists::SONGS_PER_PLAYLIST_MAX {
        mark_pending_capacity_skipped(&mut cp);
    }

    assert!(
        cp.tracks[crate::playlists::SONGS_PER_PLAYLIST_MAX]
            .outcome
            .is_none()
    );
    assert_eq!(cp.match_stats.capacity_skipped, 0);
    assert!(
        !cp.match_stats
            .terminal_reasons
            .contains_key("destination_capacity")
    );
}

#[test]
fn cached_matches_respect_remaining_local_capacity_in_source_order() {
    let mut existing = (0..crate::playlists::SONGS_PER_PLAYLIST_MAX - 1)
        .map(|index| format!("existing-{index}"))
        .collect::<HashSet<_>>();
    let duplicate_existing = "existing-0";
    let mut cp = Checkpoint::new(
        "job_cached_capacity_no_persist".to_owned(),
        spec(TransferDest::LocalPlaylist {
            name: Some("Capacity".to_owned()),
        }),
        vec![
            entry(
                input("Existing duplicate", &["Artist"]),
                Some(matched(duplicate_existing)),
            ),
            entry(
                input("Last free slot", &["Artist"]),
                Some(matched("last-free-slot")),
            ),
            entry(
                input("First overflow", &["Artist"]),
                Some(matched("overflow")),
            ),
            entry(
                input("Duplicate of kept row", &["Artist"]),
                Some(matched("last-free-slot")),
            ),
        ],
    );

    let skipped = enforce_matched_capacity(&mut cp, &mut existing);

    assert_eq!(skipped, vec![2]);
    assert_eq!(existing.len(), crate::playlists::SONGS_PER_PLAYLIST_MAX);
    assert!(matches!(
        cp.tracks[0].outcome,
        Some(MatchOutcome::Matched { ref key, .. }) if key == duplicate_existing
    ));
    assert!(matches!(
        cp.tracks[1].outcome,
        Some(MatchOutcome::Matched { ref key, .. }) if key == "last-free-slot"
    ));
    assert!(matches!(
        cp.tracks[2].outcome,
        Some(MatchOutcome::SkippedCapacity)
    ));
    assert!(matches!(
        cp.tracks[3].outcome,
        Some(MatchOutcome::Matched { ref key, .. }) if key == "last-free-slot"
    ));
    assert_eq!(cp.match_stats.capacity_skipped, 1);

    // Resume/idempotency: a row already marked at capacity is not counted twice.
    assert!(enforce_matched_capacity(&mut cp, &mut existing).is_empty());
    assert_eq!(cp.match_stats.capacity_skipped, 1);
}

#[test]
fn take_best_review_rows_use_the_same_local_capacity_accounting_as_writes() {
    let mut existing = (0..crate::playlists::SONGS_PER_PLAYLIST_MAX - 1)
        .map(|index| format!("existing-{index}"))
        .collect::<HashSet<_>>();
    let mut transfer_spec = spec(TransferDest::LocalPlaylist {
        name: Some("Capacity".to_owned()),
    });
    transfer_spec.take_best = true;
    let mut cp = Checkpoint::new(
        "job_take_best_capacity_no_persist".to_owned(),
        transfer_spec,
        vec![
            entry(
                input("First review row", &["Artist"]),
                Some(ambiguous(vec![ambiguous_candidate(
                    "review-first",
                    0.79,
                    false,
                    None,
                )])),
            ),
            entry(
                input("Second review row", &["Artist"]),
                Some(ambiguous(vec![ambiguous_candidate(
                    "review-overflow",
                    0.78,
                    false,
                    None,
                )])),
            ),
        ],
    );

    let skipped = enforce_matched_capacity(&mut cp, &mut existing);

    assert_eq!(skipped, vec![1]);
    assert!(matches!(
        cp.tracks[0].outcome,
        Some(MatchOutcome::Ambiguous { .. })
    ));
    assert!(matches!(
        cp.tracks[1].outcome,
        Some(MatchOutcome::SkippedCapacity)
    ));
    assert_eq!(
        collect_writes_without_deduping(&cp),
        vec![(0, "review-first".to_owned())]
    );
}

#[test]
fn completed_provider_results_are_drained_in_frozen_source_order() {
    let mut ready = BTreeMap::new();
    let mut next = 0usize;

    ready.insert(1, "fast-second");
    assert_eq!(take_ready_in_source_order(&mut ready, &mut next), None);
    ready.insert(0, "slow-first");

    assert_eq!(
        take_ready_in_source_order(&mut ready, &mut next),
        Some("slow-first")
    );
    assert_eq!(
        take_ready_in_source_order(&mut ready, &mut next),
        Some("fast-second")
    );
    assert_eq!(next, 2);
}

#[test]
fn local_destination_resolution_reuses_case_insensitive_name_and_prefers_id() {
    let mut store = crate::playlists::Playlists::default();
    let mix_id = store.create("Mix").expect("create Mix");
    let other_id = store.create("Other").expect("create Other");
    let mut cp = Checkpoint::new(
        "job_local_destination_resolution".to_owned(),
        spec(TransferDest::LocalPlaylist {
            name: Some("mix".to_owned()),
        }),
        Vec::new(),
    );
    cp.dest_name = Some("mix".to_owned());

    assert_eq!(
        find_local_destination(&store, &cp).map(|playlist| playlist.id.as_str()),
        Some(mix_id.as_str())
    );

    cp.dest_id = Some(other_id.clone());
    assert_eq!(
        find_local_destination(&store, &cp).map(|playlist| playlist.id.as_str()),
        Some(other_id.as_str())
    );
}

#[test]
fn post_write_report_rebuild_keeps_capacity_bucket_exclusive_and_refreshes_stats() {
    let mut cp = Checkpoint::new(
        "job_report_rebuild_no_persist".to_owned(),
        spec(TransferDest::LocalPlaylist { name: None }),
        vec![entry(
            input("Review row", &["Artist"]),
            Some(ambiguous(vec![ambiguous_candidate(
                "review-candidate",
                0.78,
                false,
                None,
            )])),
        )],
    );
    let mut report = build_report(&cp, 0);
    report.auto_accepted = 3;
    report.duplicates_dropped = 2;
    assert_eq!(report.ambiguous.len(), 1);

    cp.tracks[0].outcome = Some(MatchOutcome::SkippedCapacity);
    cp.match_stats.capacity_skipped = 1;
    cp.match_stats.checkpoint_flushes = 4;
    rebuild_report_after_write(&cp, 0, &mut report);

    assert!(report.ambiguous.is_empty());
    assert_eq!(report.capacity_skipped.len(), 1);
    assert_eq!(report.auto_accepted, 3);
    assert_eq!(report.duplicates_dropped, 2);
    assert_eq!(report.match_stats.capacity_skipped, 1);
    assert_eq!(report.match_stats.checkpoint_flushes, 4);
}
