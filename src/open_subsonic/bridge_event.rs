//! Secret-free messages crossing from the server bridge into a playback owner.

use std::sync::Arc;

use crate::personal_state::{
    EngagementKind, ExternalOperationInput, Operation, OperationOrigin, PersonalStateV2,
    PlaylistId, PortableTrack, PortableTrackKey, Rating,
};

use super::bridge_store::PendingPlaylistImportPurpose;
use super::model::{BackendId, ServerSong};

pub type OpenSubsonicBridgeSink = Arc<dyn Fn(OpenSubsonicBridgeImport) + Send + Sync + 'static>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenSubsonicScrobbleKind {
    NowPlaying,
    Submission,
}

/// One durable server observation waiting for the App or daemon personal-state owner.
///
/// The bridge keeps this record on disk until the owner confirms its personal-state transaction.
/// Re-delivery is therefore expected and safe: `operation_id` is the portable acknowledgement
/// key. The personal-state owner derives a device-scoped ledger envelope from it, while an
/// engagement keeps this value as its cross-device event dedupe key.
#[derive(Debug, Clone, PartialEq)]
pub enum OpenSubsonicBridgeImport {
    Rating {
        operation_id: String,
        track: PortableTrack,
        rating: Rating,
        observed_at_unix: i64,
    },
    Engagement {
        operation_id: String,
        track: PortableTrack,
        engagement: EngagementKind,
        played_duration_ms: Option<u64>,
        total_duration_ms: Option<u64>,
        artist_key: String,
        observed_at_unix: i64,
    },
    /// One ordered playlist observation committed by the personal-state owner as a single
    /// transaction. The outer ID acknowledges the durable bridge queue; every inner ID is the
    /// stable, replay-safe acknowledgement key for one causal ledger operation.
    Playlist {
        operation_id: String,
        backend_id: BackendId,
        local_playlist_id: PlaylistId,
        purpose: PendingPlaylistImportPurpose,
        operations: Vec<ExternalOperationInput>,
    },
}

impl OpenSubsonicBridgeImport {
    pub fn operation_id(&self) -> &str {
        match self {
            Self::Rating { operation_id, .. }
            | Self::Engagement { operation_id, .. }
            | Self::Playlist { operation_id, .. } => operation_id,
        }
    }

    fn track(&self) -> Option<&PortableTrack> {
        match self {
            Self::Rating { track, .. } | Self::Engagement { track, .. } => Some(track),
            Self::Playlist { .. } => None,
        }
    }

    pub fn backend_id(&self) -> Result<BackendId, crate::personal_state::PersonalStateError> {
        match self {
            Self::Playlist { backend_id, .. } => Ok(backend_id.clone()),
            Self::Rating { .. } | Self::Engagement { .. } => {
                let Some(track) = self.track() else {
                    unreachable!("rating and engagement imports always carry a track");
                };
                let PortableTrackKey::OpenSubsonic { backend_id, .. } = &track.key else {
                    return Err(crate::personal_state::PersonalStateError::InvalidOperation(
                        "server bridge import is not an OpenSubsonic track",
                    ));
                };
                BackendId::new(backend_id.clone()).map_err(|_| {
                    crate::personal_state::PersonalStateError::InvalidOperation(
                        "server bridge import has an invalid backend",
                    )
                })
            }
        }
    }

    pub fn origin(&self) -> Result<OperationOrigin, crate::personal_state::PersonalStateError> {
        Ok(OperationOrigin::OpenSubsonic {
            backend_id: self.backend_id()?.into_string(),
        })
    }

    /// A later local deletion retires an older remote observation instead of letting event-loop
    /// scheduling turn that stale observation into a causally newer playlist resurrection.
    pub(crate) fn remote_playlist_is_absent(
        &self,
        state: &PersonalStateV2,
    ) -> Result<bool, crate::personal_state::PersonalStateError> {
        let Self::Playlist {
            local_playlist_id,
            purpose: PendingPlaylistImportPurpose::RemoteObservation,
            ..
        } = self
        else {
            return Ok(false);
        };
        Ok(!crate::personal_state::personal_playlist_snapshots(state)?
            .iter()
            .any(|snapshot| snapshot.playlist_id == *local_playlist_id))
    }

    pub fn external_operations(&self) -> Vec<ExternalOperationInput> {
        match self {
            Self::Rating {
                operation_id,
                track,
                rating,
                observed_at_unix,
            } => vec![ExternalOperationInput {
                acknowledgement_id: operation_id.clone(),
                operation: Operation::SetRating {
                    track: track.clone(),
                    rating: *rating,
                },
                recorded_at_unix: *observed_at_unix,
            }],
            Self::Engagement {
                operation_id,
                track,
                engagement,
                played_duration_ms,
                total_duration_ms,
                artist_key,
                observed_at_unix,
            } => vec![ExternalOperationInput {
                acknowledgement_id: operation_id.clone(),
                operation: Operation::RecordEngagement {
                    event_id: operation_id.clone(),
                    track: track.clone(),
                    engagement: *engagement,
                    played_duration_ms: *played_duration_ms,
                    total_duration_ms: *total_duration_ms,
                    artist_key: artist_key.clone(),
                },
                recorded_at_unix: *observed_at_unix,
            }],
            Self::Playlist { operations, .. } => operations.clone(),
        }
    }
}

pub(crate) fn portable_server_track(song: &ServerSong) -> PortableTrack {
    PortableTrack {
        key: PortableTrackKey::OpenSubsonic {
            backend_id: song.item.backend_id().as_str().to_owned(),
            account_scope_id: song.item.account_scope_id().as_str().to_owned(),
            item_id: song.item.item_id().as_str().to_owned(),
        },
        title: song.title.clone(),
        artist: song.artist.clone(),
        album: song.album.clone(),
        duration_secs: song.duration_secs,
        isrc: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::open_subsonic::{
        AccountScopeId, BackendId, ItemId, OpenSubsonicItemRef, ServerSong,
    };

    fn song() -> ServerSong {
        ServerSong {
            item: OpenSubsonicItemRef::new(
                BackendId::new("backend").unwrap(),
                AccountScopeId::new("account").unwrap(),
                ItemId::new("song").unwrap(),
            ),
            title: "Title".to_owned(),
            artist: "Artist".to_owned(),
            artists: Vec::new(),
            album: Some("Album".to_owned()),
            album_id: None,
            album_artist: None,
            duration_secs: Some(180),
            track_number: None,
            disc_number: None,
            year: None,
            cover_art_id: None,
            content_type: None,
            suffix: None,
            starred: true,
            user_rating: Some(5),
            play_count: Some(3),
            played_at: None,
        }
    }

    #[test]
    fn server_song_becomes_path_and_url_free_portable_identity() {
        let track = portable_server_track(&song());
        assert_eq!(
            track.key,
            PortableTrackKey::OpenSubsonic {
                backend_id: "backend".to_owned(),
                account_scope_id: "account".to_owned(),
                item_id: "song".to_owned(),
            }
        );
        track.validate().unwrap();
    }

    #[test]
    fn engagement_uses_the_import_id_as_event_dedupe_key() {
        let import = OpenSubsonicBridgeImport::Engagement {
            operation_id: "native-row-7".to_owned(),
            track: portable_server_track(&song()),
            engagement: EngagementKind::Play,
            played_duration_ms: None,
            total_duration_ms: Some(180_000),
            artist_key: "artist".to_owned(),
            observed_at_unix: 10,
        };
        assert!(matches!(
            import.external_operations().as_slice(),
            [ExternalOperationInput {
                operation: Operation::RecordEngagement { event_id, .. },
                ..
            }] if event_id == "native-row-7"
        ));
        assert_eq!(
            import.origin().unwrap(),
            OperationOrigin::OpenSubsonic {
                backend_id: "backend".to_owned()
            }
        );
    }

    #[test]
    fn playlist_batch_keeps_individual_acknowledgements_and_explicit_backend() {
        let import = OpenSubsonicBridgeImport::Playlist {
            operation_id: "playlist-batch".to_owned(),
            backend_id: BackendId::new("backend").unwrap(),
            local_playlist_id: crate::personal_state::PlaylistId::new("playlist").unwrap(),
            purpose: PendingPlaylistImportPurpose::InitialOrImportCopy,
            operations: vec![ExternalOperationInput {
                acknowledgement_id: "playlist-name".to_owned(),
                operation: Operation::UpsertPlaylist {
                    playlist_id: crate::personal_state::PlaylistId::new("playlist").unwrap(),
                    name: "Mix".to_owned(),
                },
                recorded_at_unix: 10,
            }],
        };

        assert_eq!(import.operation_id(), "playlist-batch");
        assert_eq!(import.backend_id().unwrap().as_str(), "backend");
        assert_eq!(
            import.origin().unwrap(),
            OperationOrigin::OpenSubsonic {
                backend_id: "backend".to_owned()
            }
        );
        assert_eq!(
            import.external_operations()[0].acknowledgement_id,
            "playlist-name"
        );
    }
}
