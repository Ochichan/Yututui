use super::*;

#[test]
fn external_operation_replay_is_exactly_once_and_keeps_origin() {
    let device = DeviceId::new("device-a").unwrap();
    let state = state_with_keyed_devices(&[device.as_str()]);
    let operation = Operation::SetRating {
        track: open_subsonic_track("server-track"),
        rating: Rating::Liked,
    };
    let origin = OperationOrigin::OpenSubsonic {
        backend_id: "backend-a".to_owned(),
    };
    let first = append_external_operation_as(
        &state,
        &device,
        "open-subsonic-rating-backend-a-1".to_owned(),
        origin.clone(),
        operation.clone(),
        100,
    )
    .unwrap();
    let repeated = append_external_operation_as(
        &first,
        &device,
        "open-subsonic-rating-backend-a-1".to_owned(),
        origin.clone(),
        operation.clone(),
        200,
    )
    .unwrap();

    assert_eq!(repeated, first);
    let envelope_id =
        external_operation_envelope_id(&device, "open-subsonic-rating-backend-a-1").unwrap();
    let observed = first
        .operations
        .iter()
        .find(|candidate| candidate.operation_id == envelope_id)
        .unwrap();
    assert_ne!(observed.operation_id, "open-subsonic-rating-backend-a-1");
    assert_eq!(observed.origin, origin);
    assert_eq!(observed.operation, operation);
}

#[test]
fn external_observations_use_device_scoped_envelopes_and_portable_event_dedupe() {
    let device_a = DeviceId::new("device-a").unwrap();
    let device_b = DeviceId::new("device-b").unwrap();
    let state = state_with_keyed_devices(&[device_a.as_str(), device_b.as_str()]);
    let track = open_subsonic_track("server-track");
    let origin = OperationOrigin::OpenSubsonic {
        backend_id: "backend-a".to_owned(),
    };
    let engagement = Operation::RecordEngagement {
        event_id: "portable-native-row-7".to_owned(),
        track: track.clone(),
        engagement: EngagementKind::Play,
        played_duration_ms: None,
        total_duration_ms: Some(180_000),
        artist_key: "artist".to_owned(),
    };
    let from_a = append_external_operation_as(
        &state,
        &device_a,
        "portable-native-row-7".to_owned(),
        origin.clone(),
        engagement.clone(),
        100,
    )
    .unwrap();
    let from_b = append_external_operation_as(
        &state,
        &device_b,
        "portable-native-row-7".to_owned(),
        origin.clone(),
        engagement,
        101,
    )
    .unwrap();
    let id_a = external_operation_envelope_id(&device_a, "portable-native-row-7").unwrap();
    let id_b = external_operation_envelope_id(&device_b, "portable-native-row-7").unwrap();
    assert_ne!(id_a, id_b);

    let (merged_ab, _) = merge(&from_a, &from_b).unwrap();
    let (merged_ba, _) = merge(&from_b, &from_a).unwrap();
    assert_eq!(merged_ab.operations, merged_ba.operations);
    let projection = super::super::reducer::project_at(&merged_ab, 101).unwrap();
    let signal = projection.legacy.signals.tracks.get(&track.key).unwrap();
    assert_eq!(signal.play_count, 1);
    assert_eq!(projection.legacy.signals.play_log.len(), 1);
    assert_eq!(
        projection.legacy.signals.play_log[0].event_id,
        "portable-native-row-7"
    );

    let liked = append_external_operation_as(
        &state,
        &device_a,
        "portable-rating-observation".to_owned(),
        origin.clone(),
        Operation::SetRating {
            track: track.clone(),
            rating: Rating::Liked,
        },
        200,
    )
    .unwrap();
    let disliked = append_external_operation_as(
        &state,
        &device_b,
        "portable-rating-observation".to_owned(),
        origin,
        Operation::SetRating {
            track,
            rating: Rating::Disliked,
        },
        1,
    )
    .unwrap();
    let (rating_ab, _) = merge(&liked, &disliked).unwrap();
    let (rating_ba, _) = merge(&disliked, &liked).unwrap();
    let winner_ab = open_subsonic_rating_winners(&rating_ab)
        .unwrap()
        .pop()
        .unwrap();
    let winner_ba = open_subsonic_rating_winners(&rating_ba)
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(winner_ab, winner_ba);
    assert_eq!(winner_ab.rating, Rating::Disliked);
}
