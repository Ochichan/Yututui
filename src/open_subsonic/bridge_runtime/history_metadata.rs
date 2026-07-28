//! Bounded, durable metadata enrichment for exact native-history rows.

use std::collections::{BTreeMap, BTreeSet};

use futures::StreamExt as _;

use super::*;

const METADATA_ITEMS_PER_REFRESH: usize = 64;
const METADATA_CONCURRENCY: usize = 8;

pub(super) struct MetadataResolution {
    pub observations: Vec<NativeHistoryObservation>,
    pub aggregate_baselines: BTreeMap<ItemId, (u64, Option<String>)>,
    pub pending: Vec<PendingNativeMetadataRow>,
    pub transient_failure: bool,
}

pub(super) async fn resolve(
    store_set: &OpenSubsonicStoreSet,
    client: &OpenSubsonicClient,
    pending: Vec<PendingNativeMetadataRow>,
) -> MetadataResolution {
    let metadata_ids = oldest_metadata_ids(&pending);
    let fetched = futures::stream::iter(metadata_ids.into_iter().map(|item_id| {
        let item = scoped_item(store_set, item_id.clone());
        async move {
            let result = catalog(store_set, client).get_song(&item).await;
            (item_id, result)
        }
    }))
    .buffer_unordered(METADATA_CONCURRENCY)
    .collect::<Vec<_>>()
    .await;
    let metadata = fetched.into_iter().collect();
    resolve_fetched(store_set, pending, &metadata)
}

fn oldest_metadata_ids(pending: &[PendingNativeMetadataRow]) -> Vec<ItemId> {
    let mut seen = BTreeSet::new();
    let mut ids = Vec::new();
    for row in pending.iter().rev() {
        if seen.insert(row.item_id.clone()) {
            ids.push(row.item_id.clone());
            if ids.len() == METADATA_ITEMS_PER_REFRESH {
                break;
            }
        }
    }
    ids
}

fn resolve_fetched(
    store_set: &OpenSubsonicStoreSet,
    mut pending: Vec<PendingNativeMetadataRow>,
    metadata: &BTreeMap<ItemId, Result<ServerSong, super::super::ServerError>>,
) -> MetadataResolution {
    let mut observations = Vec::new();
    let mut aggregate_baselines = BTreeMap::new();
    let mut resolved = 0;
    let mut transient_failure = false;
    for row in pending.iter().rev() {
        let Some(result) = metadata.get(&row.item_id) else {
            break;
        };
        let track = match result {
            Ok(song) => {
                if song.play_count.is_some() || song.played_at.is_some() {
                    aggregate_baselines.insert(
                        row.item_id.clone(),
                        (song.play_count.unwrap_or(0), song.played_at.clone()),
                    );
                }
                portable_server_track(song)
            }
            Err(super::super::ServerError::NotFound) => {
                placeholder_track(&scoped_item(store_set, row.item_id.clone()))
            }
            Err(_) => {
                transient_failure = true;
                break;
            }
        };
        observations.push(NativeHistoryObservation {
            row_id: row.row_id,
            item_id: row.item_id.clone(),
            track,
            observed_at_unix: row.observed_at_unix,
        });
        resolved += 1;
    }
    pending.truncate(pending.len().saturating_sub(resolved));
    observations.reverse();
    MetadataResolution {
        observations,
        aggregate_baselines,
        pending,
        transient_failure,
    }
}

fn placeholder_track(item: &OpenSubsonicItemRef) -> PortableTrack {
    PortableTrack {
        key: PortableTrackKey::OpenSubsonic {
            backend_id: item.backend_id().as_str().to_owned(),
            account_scope_id: item.account_scope_id().as_str().to_owned(),
            item_id: item.item_id().as_str().to_owned(),
        },
        title: "Server track".to_owned(),
        artist: "Unknown artist".to_owned(),
        album: None,
        duration_secs: None,
        isrc: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::open_subsonic::model::OpenSubsonicItemRef;
    use crate::open_subsonic::private_store::{OpenSubsonicPrivateState, ServerCredential};
    use crate::open_subsonic::profile::OpenSubsonicProfile;
    use crate::open_subsonic::{ConfiguredPrivateOrigin, OpenSubsonicBridgeState};
    use age::secrecy::SecretString;

    fn store_set() -> OpenSubsonicStoreSet {
        let backend = BackendId::new("backend-history-metadata").unwrap();
        let account = AccountScopeId::new("account-history-metadata").unwrap();
        let profile = OpenSubsonicProfile::with_ids(
            0,
            backend.clone(),
            account.clone(),
            "Server",
            ConfiguredPrivateOrigin::new("http://127.0.0.1:4040/", true).unwrap(),
            None,
        )
        .unwrap();
        let private = OpenSubsonicPrivateState::new(
            backend.clone(),
            account.clone(),
            ServerCredential::api_key(SecretString::from("test-key".to_owned())).unwrap(),
        );
        OpenSubsonicStoreSet::new(
            profile,
            private,
            OpenSubsonicBridgeState::new(backend, account),
        )
        .unwrap()
    }

    fn pending_rows(count: u64) -> Vec<PendingNativeMetadataRow> {
        (1..=count)
            .rev()
            .map(|id| PendingNativeMetadataRow {
                row_id: id,
                item_id: ItemId::new(format!("song-{id}")).unwrap(),
                observed_at_unix: i64::try_from(id).unwrap(),
            })
            .collect()
    }

    fn song(store_set: &OpenSubsonicStoreSet, item_id: ItemId) -> ServerSong {
        ServerSong {
            item: OpenSubsonicItemRef::new(
                store_set.profile.backend_id().clone(),
                store_set.profile.account_scope_id().clone(),
                item_id,
            ),
            title: "Resolved song".to_owned(),
            artist: "Resolved artist".to_owned(),
            artists: Vec::new(),
            album: None,
            album_id: None,
            album_artist: None,
            duration_secs: Some(180),
            track_number: None,
            disc_number: None,
            year: None,
            cover_art_id: None,
            content_type: None,
            suffix: None,
            starred: false,
            user_rating: None,
            play_count: Some(1),
            played_at: None,
        }
    }

    #[test]
    fn more_than_sixty_four_unique_items_continue_without_unknown_metadata() {
        let store_set = store_set();
        let pending = pending_rows(65);
        let mut metadata = BTreeMap::new();
        for id in 1..=64 {
            let item_id = ItemId::new(format!("song-{id}")).unwrap();
            metadata.insert(item_id.clone(), Ok(song(&store_set, item_id)));
        }

        let resolution = resolve_fetched(&store_set, pending, &metadata);

        assert_eq!(resolution.observations.len(), 64);
        assert_eq!(resolution.pending.len(), 1);
        assert_eq!(resolution.pending[0].row_id, 65);
        assert!(resolution.observations.iter().all(|observation| {
            observation.track.title == "Resolved song"
                && observation.track.artist == "Resolved artist"
        }));
    }

    #[test]
    fn transient_metadata_failure_keeps_the_row_and_every_newer_row_pending() {
        let store_set = store_set();
        let pending = pending_rows(3);
        let mut metadata = BTreeMap::new();
        metadata.insert(
            ItemId::new("song-1").unwrap(),
            Err(super::super::super::ServerError::TemporarilyUnavailable),
        );
        for id in 2..=3 {
            let item_id = ItemId::new(format!("song-{id}")).unwrap();
            metadata.insert(item_id.clone(), Ok(song(&store_set, item_id)));
        }

        let resolution = resolve_fetched(&store_set, pending.clone(), &metadata);

        assert!(resolution.observations.is_empty());
        assert_eq!(resolution.pending, pending);
        assert!(resolution.transient_failure);
    }

    #[test]
    fn only_definitive_not_found_uses_placeholder_metadata() {
        let store_set = store_set();
        let pending = pending_rows(1);
        let metadata = BTreeMap::from([(
            ItemId::new("song-1").unwrap(),
            Err(super::super::super::ServerError::NotFound),
        )]);

        let resolution = resolve_fetched(&store_set, pending, &metadata);

        assert!(resolution.pending.is_empty());
        assert!(!resolution.transient_failure);
        assert_eq!(resolution.observations[0].track.title, "Server track");
        assert_eq!(resolution.observations[0].track.artist, "Unknown artist");
    }
}
