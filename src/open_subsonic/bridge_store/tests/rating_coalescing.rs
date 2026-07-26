use super::*;

#[test]
fn newest_rating_observation_coalesces_by_item_and_survives_restart() {
    let mut bridge = bridge();
    let item = ItemId::new("song").unwrap();
    let pending = |raw, mapped, observed_at_unix| PendingRatingImport {
        item_id: item.clone(),
        track: portable("song"),
        raw,
        mapped,
        observed_at_unix,
    };
    bridge
        .queue_rating_import(
            "zz-older-lexically-later".to_owned(),
            pending(
                RawServerRating {
                    user_rating: Some(5),
                    starred: true,
                },
                Rating::Liked,
                10,
            ),
        )
        .unwrap();
    bridge
        .queue_rating_import(
            "aa-newer-lexically-earlier".to_owned(),
            pending(
                RawServerRating {
                    user_rating: Some(1),
                    starred: false,
                },
                Rating::Disliked,
                11,
            ),
        )
        .unwrap();

    let restarted = decode_bridge(&encode_bridge(&bridge).unwrap()).unwrap();
    assert_eq!(restarted.pending_rating_imports().len(), 1);
    let (operation_id, pending) = restarted
        .pending_rating_imports()
        .first_key_value()
        .unwrap();
    assert_eq!(operation_id, "aa-newer-lexically-earlier");
    assert_eq!(pending.mapped, Rating::Disliked);
}
