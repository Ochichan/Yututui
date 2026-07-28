use std::collections::HashMap;

use serde::Deserialize;

use super::{
    EXPORT_KIND, EXPORT_PROFILE, ExportSnapshot, PortableCatalogId, PortableTrackV1, SearchSource,
};

/// Decode either a legacy v1 portable export or a v2 personal-state bundle.
///
/// The caller owns the 192 MiB input cap. This decoder rejects exports whose public privacy
/// metadata claims credentials, paths, or playable URLs are present.
pub fn decode_personal_state_export(
    bytes: &[u8],
) -> Result<crate::personal_state::PersonalStateV2, crate::personal_state::PersonalStateError> {
    #[derive(Deserialize)]
    struct Header {
        kind: String,
        schema_version: u32,
    }

    let header: Header = serde_json::from_slice(bytes)?;
    match header.schema_version {
        2 if header.kind == crate::personal_state::PERSONAL_STATE_KIND => decode_v2(bytes),
        1 if header.kind == EXPORT_KIND => decode_v1(bytes),
        1 | 2 => Err(crate::personal_state::PersonalStateError::UnsupportedKind),
        schema => Err(crate::personal_state::PersonalStateError::UnsupportedSchema(schema)),
    }
}

fn decode_v2(
    bytes: &[u8],
) -> Result<crate::personal_state::PersonalStateV2, crate::personal_state::PersonalStateError> {
    let state: crate::personal_state::PersonalStateV2 = serde_json::from_slice(bytes)?;
    if state.metadata.credentials_included
        || state.metadata.filesystem_paths_included
        || state.metadata.playable_urls_included
    {
        return Err(crate::personal_state::PersonalStateError::InvalidOperation(
            "portable metadata includes private machine data",
        ));
    }
    state.validate()?;
    Ok(state)
}

fn decode_v1(
    bytes: &[u8],
) -> Result<crate::personal_state::PersonalStateV2, crate::personal_state::PersonalStateError> {
    let snapshot: ExportSnapshot = serde_json::from_slice(bytes)?;
    if snapshot.profile != EXPORT_PROFILE
        || snapshot.privacy.credentials_included
        || snapshot.privacy.filesystem_paths_included
        || snapshot.privacy.playable_urls_included
        || snapshot.privacy.media_files_included
    {
        return Err(crate::personal_state::PersonalStateError::InvalidOperation(
            "legacy export is not a portable credential-free snapshot",
        ));
    }
    crate::personal_state::legacy::legacy_state_from_projection(legacy_projection(snapshot)?)
}

fn legacy_projection(
    snapshot: ExportSnapshot,
) -> Result<crate::personal_state::LegacyProjection, crate::personal_state::PersonalStateError> {
    use crate::personal_state::legacy::{
        LegacyPlayEvent, LegacyPlaylist, LegacyPlaylistEntry, LegacySignals, LegacyStation,
        LegacyTrackSignal, stable_hash,
    };
    use crate::personal_state::{PlaylistEntryId, PlaylistId, PortableTrack};

    let mut catalog = HashMap::<PortableCatalogId, PortableTrack>::new();
    let mut convert_track = |track: PortableTrackV1| {
        let portable = portable_track(&track);
        if let Some(catalog_id) = track.catalog {
            catalog.insert(catalog_id, portable.clone());
        }
        portable
    };
    let favorites = snapshot
        .library
        .favorites
        .into_iter()
        .map(&mut convert_track)
        .collect();
    let history = snapshot
        .library
        .history
        .into_iter()
        .map(&mut convert_track)
        .collect();
    let radio_favorites = snapshot
        .library
        .radio_favorites
        .into_iter()
        .map(&mut convert_track)
        .collect();
    let radio_history = snapshot
        .library
        .radio_history
        .into_iter()
        .map(&mut convert_track)
        .collect();

    let playlists = snapshot
        .playlists
        .into_iter()
        .enumerate()
        .map(|(playlist_index, playlist)| {
            let slug = playlist.id.unwrap_or_else(|| {
                format!(
                    "imported-{}",
                    stable_hash(&format!("{}\u{0}{playlist_index}", playlist.name))
                )
            });
            let playlist_id = PlaylistId(format!(
                "v1-playlist-{}",
                stable_hash(&format!(
                    "{slug}\u{0}{}\u{0}{playlist_index}",
                    playlist.name
                ))
            ));
            let entries = playlist
                .tracks
                .into_iter()
                .enumerate()
                .map(|(entry_index, track)| {
                    let track = convert_track(track);
                    LegacyPlaylistEntry {
                        entry_id: PlaylistEntryId(format!(
                            "v1-entry-{}",
                            stable_hash(&format!(
                                "{}\u{0}{entry_index}\u{0}{:?}",
                                playlist_id.as_str(),
                                track.key
                            ))
                        )),
                        track,
                    }
                })
                .collect();
            LegacyPlaylist {
                playlist_id,
                slug,
                name: playlist.name,
                entries,
            }
        })
        .collect();

    let mut signals = LegacySignals {
        artist_affinity: snapshot.preferences.signals.artist_weights,
        ..LegacySignals::default()
    };
    for signal in snapshot.preferences.signals.track_signals {
        let track = catalog
            .get(&signal.catalog)
            .cloned()
            .unwrap_or_else(|| placeholder_track(&signal.catalog));
        signals.tracks.insert(
            track.key.clone(),
            LegacyTrackSignal {
                track,
                play_count: signal.play_count,
                completed_count: signal.completed_count,
                skip_count: signal.skip_count,
                last_played_at: signal.last_played_at,
                last_completion: signal.last_completion,
                disliked: signal.disliked,
            },
        );
    }
    signals.play_log = snapshot
        .preferences
        .signals
        .play_log
        .into_iter()
        .enumerate()
        .map(|(index, event)| {
            let track = catalog
                .get(&event.catalog)
                .cloned()
                .unwrap_or_else(|| placeholder_track(&event.catalog));
            LegacyPlayEvent {
                event_id: format!(
                    "v1-event-{}",
                    stable_hash(&format!(
                        "{}\u{0}{}\u{0}{index}",
                        event.catalog.id, event.played_at
                    ))
                ),
                track,
                played_at: event.played_at,
            }
        })
        .collect();
    let station =
        snapshot
            .preferences
            .station
            .active
            .map_or_else(LegacyStation::default, |active| LegacyStation {
                query: active.query,
                explore: active.explore,
                avoid_artist_keys: active.avoid_artist_keys,
            });
    let projection = crate::personal_state::LegacyProjection {
        favorites,
        history,
        radio_favorites,
        radio_history,
        playlists,
        signals,
        station,
    };
    projection.validate()?;
    Ok(projection)
}

fn portable_track(track: &PortableTrackV1) -> crate::personal_state::PortableTrack {
    use crate::personal_state::{PortableTrack, PortableTrackKey};

    let key = track.catalog.as_ref().map_or_else(
        || PortableTrackKey::LocalPlaceholder {
            portable_placeholder_id: crate::personal_state::legacy::stable_hash(&format!(
                "{}\u{0}{}\u{0}{}\u{0}{:?}",
                track.title, track.artist, track.duration, track.album
            )),
        },
        portable_key,
    );
    PortableTrack {
        key,
        title: track.title.clone(),
        artist: track.artist.clone(),
        album: track.album.clone(),
        duration_secs: track.duration_secs,
        isrc: track.isrc.clone(),
    }
}

fn placeholder_track(catalog: &PortableCatalogId) -> crate::personal_state::PortableTrack {
    crate::personal_state::PortableTrack {
        key: portable_key(catalog),
        title: "Imported track".to_owned(),
        artist: "Unknown artist".to_owned(),
        album: None,
        duration_secs: None,
        isrc: None,
    }
}

fn portable_key(catalog: &PortableCatalogId) -> crate::personal_state::PortableTrackKey {
    crate::personal_state::PortableTrackKey::Catalog {
        provider: if catalog.source == SearchSource::Youtube {
            "youtube".to_owned()
        } else {
            catalog.source.id_prefix().to_owned()
        },
        exact_catalog_id: catalog.id.clone(),
    }
}
