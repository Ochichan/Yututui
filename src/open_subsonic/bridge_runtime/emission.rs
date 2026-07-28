//! Replay durable imports to the current personal-state owner.

use super::super::bridge_event::OpenSubsonicBridgeImport;
use super::super::transaction::OpenSubsonicStoreSet;
use super::BridgeRuntime;

impl BridgeRuntime {
    pub(crate) fn emit_pending(&self, store_set: &OpenSubsonicStoreSet) {
        let Some(sink) = &self.sink else {
            return;
        };
        let ratings = store_set.bridge_state.pending_rating_imports().iter().map(
            |(operation_id, pending)| OpenSubsonicBridgeImport::Rating {
                operation_id: operation_id.clone(),
                track: pending.track.clone(),
                rating: pending.mapped,
                observed_at_unix: pending.observed_at_unix,
            },
        );
        let engagements = store_set
            .bridge_state
            .pending_engagement_imports()
            .iter()
            .map(
                |(operation_id, pending)| OpenSubsonicBridgeImport::Engagement {
                    operation_id: operation_id.clone(),
                    track: pending.track.clone(),
                    engagement: pending.engagement,
                    played_duration_ms: pending.played_duration_ms,
                    total_duration_ms: pending.total_duration_ms,
                    artist_key: pending.artist_key.clone(),
                    observed_at_unix: pending.observed_at_unix,
                },
            );
        let playlists = store_set
            .bridge_state
            .pending_playlist_imports()
            .values()
            .map(|pending| OpenSubsonicBridgeImport::Playlist {
                operation_id: pending.operation_id.clone(),
                backend_id: store_set.profile.backend_id().clone(),
                local_playlist_id: pending.local_playlist_id.clone(),
                purpose: pending.purpose,
                operations: pending.operations.clone(),
            });
        for event in ratings.chain(engagements).chain(playlists) {
            sink(event);
        }
    }
}
