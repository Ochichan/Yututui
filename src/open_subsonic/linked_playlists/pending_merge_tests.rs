use super::*;

fn item(value: &str) -> ItemId {
    ItemId::new(value).unwrap()
}

fn entry(entry_id: &str, item_id: &str) -> LinkedPlaylistEntry {
    LinkedPlaylistEntry::new(PlaylistEntryId::new(entry_id).unwrap(), item(item_id))
}

fn occurrence_labels(plan: &PendingRemoteMergePlan) -> Vec<String> {
    plan.ordered_occurrences()
        .iter()
        .map(|occurrence| match occurrence {
            PendingMergeOccurrence::Existing(existing) => format!(
                "existing:{}@{}",
                existing.entry.entry_id.as_str(),
                existing
                    .remote_index
                    .map_or_else(|| "local".to_owned(), |index| index.to_string())
            ),
            PendingMergeOccurrence::RemoteOnly(remote) => {
                format!("remote:{}@{}", remote.item_id.as_str(), remote.index)
            }
        })
        .collect()
}

fn remote_labels(plan: &PendingRemoteMergePlan) -> Vec<String> {
    plan.remote_occurrences()
        .iter()
        .map(|occurrence| match occurrence {
            PendingRemoteOccurrence::Existing(existing) => format!(
                "existing:{}@{}",
                existing.entry.entry_id.as_str(),
                existing.remote_index
            ),
            PendingRemoteOccurrence::RemoteOnly(remote) => {
                format!("remote:{}@{}", remote.item_id.as_str(), remote.index)
            }
        })
        .collect()
}

fn item_values(items: &[ItemId]) -> Vec<&str> {
    items.iter().map(ItemId::as_str).collect()
}

#[test]
fn unknown_delivery_reuses_exact_local_echo_and_preserves_remote_addition() {
    let base = [entry("entry-a", "a")];
    let desired = [entry("entry-a", "a"), entry("entry-b", "b")];
    let current = [item("a"), item("b"), item("c")];

    let plan = plan_pending_remote_merge(
        &base,
        &desired,
        &current,
        PendingRemoteMergeMode::DeliveryUnknown,
    )
    .unwrap();

    assert_eq!(
        occurrence_labels(&plan),
        ["existing:entry-a@0", "existing:entry-b@1", "remote:c@2"]
    );
    assert_eq!(item_values(plan.desired_remote()), ["a", "b", "c"]);
    assert!(plan.removed_existing().is_empty());
}

#[test]
fn queued_equal_local_and_remote_additions_remain_distinct_occurrences() {
    let base = [entry("entry-a", "a")];
    let desired = [entry("entry-a", "a"), entry("local-b", "b")];
    let current = [item("a"), item("b")];

    let plan = plan_pending_remote_merge(
        &base,
        &desired,
        &current,
        PendingRemoteMergeMode::LocalNotDelivered,
    )
    .unwrap();

    assert_eq!(
        occurrence_labels(&plan),
        ["existing:entry-a@0", "existing:local-b@local", "remote:b@1"]
    );
    assert_eq!(item_values(plan.desired_remote()), ["a", "b", "b"]);
}

#[test]
fn queued_local_and_remote_additions_merge_without_deletion() {
    let base = [entry("entry-a", "a"), entry("entry-b", "b")];
    let desired = [
        entry("entry-a", "a"),
        entry("local", "local"),
        entry("entry-b", "b"),
    ];
    let current = [item("a"), item("remote"), item("b")];

    let plan = plan_pending_remote_merge(
        &base,
        &desired,
        &current,
        PendingRemoteMergeMode::LocalNotDelivered,
    )
    .unwrap();

    assert_eq!(
        occurrence_labels(&plan),
        [
            "existing:entry-a@0",
            "existing:local@local",
            "remote:remote@1",
            "existing:entry-b@2"
        ]
    );
}

#[test]
fn confirmed_delivery_uses_desired_as_prior_for_remote_reorder_and_remove() {
    let base = [entry("entry-a", "a")];
    let desired = [entry("entry-a", "a"), entry("local-b", "b")];
    let current = [item("b"), item("c")];

    let plan = plan_pending_remote_merge(
        &base,
        &desired,
        &current,
        PendingRemoteMergeMode::LocalDelivered,
    )
    .unwrap();

    assert_eq!(
        occurrence_labels(&plan),
        ["existing:local-b@0", "remote:c@1"]
    );
    assert_eq!(
        plan.removed_existing()[0].entry.entry_id.as_str(),
        "entry-a"
    );
}

#[test]
fn concurrent_base_reorder_follows_remote_order() {
    let base = [
        entry("entry-a", "a"),
        entry("entry-b", "b"),
        entry("entry-c", "c"),
    ];
    let desired = [
        entry("entry-b", "b"),
        entry("entry-a", "a"),
        entry("entry-c", "c"),
    ];
    let current = [item("c"), item("a"), item("b")];

    let plan = plan_pending_remote_merge(
        &base,
        &desired,
        &current,
        PendingRemoteMergeMode::LocalNotDelivered,
    )
    .unwrap();

    assert_eq!(
        occurrence_labels(&plan),
        [
            "existing:entry-c@0",
            "existing:entry-a@1",
            "existing:entry-b@2"
        ]
    );
}

#[test]
fn deletion_of_base_occurrence_wins_over_other_side_move() {
    let base = [
        entry("entry-a", "a"),
        entry("entry-b", "b"),
        entry("entry-c", "c"),
    ];
    let locally_moved = [
        entry("entry-b", "b"),
        entry("entry-a", "a"),
        entry("entry-c", "c"),
    ];
    let remote_deleted = [item("c"), item("a")];
    let remote_wins = plan_pending_remote_merge(
        &base,
        &locally_moved,
        &remote_deleted,
        PendingRemoteMergeMode::LocalNotDelivered,
    )
    .unwrap();
    assert_eq!(
        occurrence_labels(&remote_wins),
        ["existing:entry-c@0", "existing:entry-a@1"]
    );
    assert_eq!(
        remote_wins.removed_existing()[0].entry.entry_id.as_str(),
        "entry-b"
    );

    let locally_deleted = [entry("entry-a", "a"), entry("entry-c", "c")];
    let remote_moved = [item("c"), item("b"), item("a")];
    let local_wins = plan_pending_remote_merge(
        &base,
        &locally_deleted,
        &remote_moved,
        PendingRemoteMergeMode::LocalNotDelivered,
    )
    .unwrap();
    assert_eq!(
        occurrence_labels(&local_wins),
        ["existing:entry-c@0", "existing:entry-a@2"]
    );
    assert_eq!(
        remote_labels(&local_wins),
        [
            "existing:entry-c@0",
            "existing:entry-b@1",
            "existing:entry-a@2"
        ],
        "the exact server shadow retains provenance even for the locally deleted occurrence"
    );
    assert!(local_wins.removed_existing().is_empty());
}

#[test]
fn duplicate_occurrences_keep_distinct_stable_ids() {
    let base = [
        entry("first-a", "a"),
        entry("second-a", "a"),
        entry("entry-b", "b"),
    ];
    let desired = [
        entry("entry-b", "b"),
        entry("second-a", "a"),
        entry("first-a", "a"),
        entry("local-a", "a"),
    ];
    let current = [item("a"), item("b"), item("a"), item("a")];

    let plan = plan_pending_remote_merge(
        &base,
        &desired,
        &current,
        PendingRemoteMergeMode::DeliveryUnknown,
    )
    .unwrap();

    assert_eq!(
        occurrence_labels(&plan),
        [
            "existing:first-a@0",
            "existing:entry-b@1",
            "existing:second-a@2",
            "existing:local-a@3"
        ]
    );
}

#[test]
fn local_same_anchor_siblings_use_stable_entry_id_order() {
    let base = [entry("entry-a", "a"), entry("entry-b", "b")];
    let desired = [
        entry("entry-a", "a"),
        entry("z-local", "z"),
        entry("a-local", "x"),
        entry("entry-b", "b"),
    ];
    let current = [item("a"), item("b")];

    let plan = plan_pending_remote_merge(
        &base,
        &desired,
        &current,
        PendingRemoteMergeMode::LocalNotDelivered,
    )
    .unwrap();

    assert_eq!(
        occurrence_labels(&plan),
        [
            "existing:entry-a@0",
            "existing:a-local@local",
            "existing:z-local@local",
            "existing:entry-b@1"
        ]
    );
}

#[test]
fn pending_merge_is_idempotently_deterministic() {
    let base = [entry("entry-a", "a"), entry("entry-b", "b")];
    let desired = [entry("entry-b", "b"), entry("local", "x")];
    let current = [item("remote"), item("b"), item("x")];

    let first = plan_pending_remote_merge(
        &base,
        &desired,
        &current,
        PendingRemoteMergeMode::DeliveryUnknown,
    )
    .unwrap();
    let second = plan_pending_remote_merge(
        &base,
        &desired,
        &current,
        PendingRemoteMergeMode::DeliveryUnknown,
    )
    .unwrap();

    assert_eq!(first, second);
}

#[test]
fn duplicate_entry_ids_and_changed_exact_items_are_rejected() {
    let duplicate = [entry("same", "a"), entry("same", "a")];
    assert_eq!(
        plan_pending_remote_merge(
            &[],
            &duplicate,
            &[],
            PendingRemoteMergeMode::LocalNotDelivered
        ),
        Err(LinkedPlaylistError::DuplicateEntryId {
            sequence: PlaylistSequence::DesiredRemote,
            entry_id: PlaylistEntryId::new("same").unwrap(),
        })
    );

    assert_eq!(
        plan_pending_remote_merge(
            &[entry("same", "a")],
            &[entry("same", "b")],
            &[item("a")],
            PendingRemoteMergeMode::DeliveryUnknown,
        ),
        Err(LinkedPlaylistError::EntryItemMismatch {
            entry_id: PlaylistEntryId::new("same").unwrap(),
        })
    );
}

#[test]
fn pending_inputs_and_merged_union_enforce_occurrence_bound() {
    let too_many = (0..=MAX_LINKED_PLAYLIST_ENTRIES)
        .map(|index| entry(&format!("entry-{index}"), "same"))
        .collect::<Vec<_>>();
    assert_eq!(
        plan_pending_remote_merge(
            &too_many,
            &[],
            &[],
            PendingRemoteMergeMode::LocalNotDelivered,
        ),
        Err(LinkedPlaylistError::TooManyEntries {
            sequence: PlaylistSequence::PreviousRemoteShadow,
            actual: 1000,
            maximum: 999,
        })
    );
    assert_eq!(
        plan_pending_remote_merge(
            &[],
            &too_many,
            &[],
            PendingRemoteMergeMode::LocalNotDelivered,
        ),
        Err(LinkedPlaylistError::TooManyEntries {
            sequence: PlaylistSequence::DesiredRemote,
            actual: 1000,
            maximum: 999,
        })
    );

    let local = (0..600)
        .map(|index| entry(&format!("local-{index}"), &format!("local-item-{index}")))
        .collect::<Vec<_>>();
    let remote = (0..600)
        .map(|index| item(&format!("remote-item-{index}")))
        .collect::<Vec<_>>();
    assert_eq!(
        plan_pending_remote_merge(
            &[],
            &local,
            &remote,
            PendingRemoteMergeMode::LocalNotDelivered,
        ),
        Err(LinkedPlaylistError::TooManyEntries {
            sequence: PlaylistSequence::DesiredRemote,
            actual: 1200,
            maximum: 999,
        })
    );
}
