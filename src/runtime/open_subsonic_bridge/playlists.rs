//! Linked server-playlist projection on the interactive owner's durability lane.

use super::RuntimeHandles;
use crate::app::App;

pub(in crate::runtime) struct PendingOpenSubsonicPlaylistProjection {
    identity: String,
    receipt: crate::open_subsonic::OpenSubsonicPlaylistReceipt,
}

enum PlaylistProjectionReceipt {
    Idle,
    Waiting,
    Durable(String),
    Retry(Option<crate::open_subsonic::ServiceError>),
}

fn poll_playlist_projection_receipt(
    pending: &mut Option<PendingOpenSubsonicPlaylistProjection>,
) -> PlaylistProjectionReceipt {
    let Some(receipt) = pending.as_mut().map(|pending| &mut pending.receipt) else {
        return PlaylistProjectionReceipt::Idle;
    };
    match receipt.try_recv() {
        Ok(Ok(())) => PlaylistProjectionReceipt::Durable(
            pending
                .take()
                .expect("playlist receipt was present")
                .identity,
        ),
        Ok(Err(error)) => {
            *pending = None;
            PlaylistProjectionReceipt::Retry(Some(error))
        }
        Err(tokio::sync::oneshot::error::TryRecvError::Empty) => PlaylistProjectionReceipt::Waiting,
        Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
            *pending = None;
            PlaylistProjectionReceipt::Retry(None)
        }
    }
}

impl RuntimeHandles {
    pub(super) fn poll_open_subsonic_playlist_projection(&mut self) -> bool {
        match poll_playlist_projection_receipt(&mut self.open_subsonic_pending_playlist) {
            PlaylistProjectionReceipt::Idle => false,
            PlaylistProjectionReceipt::Waiting => true,
            PlaylistProjectionReceipt::Durable(identity) => {
                self.open_subsonic_playlist_identity = Some(identity);
                false
            }
            PlaylistProjectionReceipt::Retry(Some(error)) => {
                tracing::warn!(
                    reason = %error,
                    "music server playlists are waiting for local durability"
                );
                false
            }
            PlaylistProjectionReceipt::Retry(None) => false,
        }
    }

    pub(super) fn reconcile_open_subsonic_playlists(
        &mut self,
        app: &App,
        handle: &crate::open_subsonic::OpenSubsonicHandle,
        identity: &str,
        receipt_pending: bool,
    ) {
        if receipt_pending || self.open_subsonic_playlist_identity.as_deref() == Some(identity) {
            return;
        }
        match crate::personal_state::personal_playlist_snapshots(&app.personal_state.ledger) {
            Ok(snapshots) => {
                if let Ok(receipt) = handle.reconcile_playlists(snapshots) {
                    self.open_subsonic_pending_playlist =
                        Some(PendingOpenSubsonicPlaylistProjection {
                            identity: identity.to_owned(),
                            receipt,
                        });
                }
            }
            Err(error) => {
                tracing::warn!(
                    %error,
                    "music server playlist projection could not be prepared"
                );
            }
        }
    }

    pub(super) fn reset_open_subsonic_playlist_projection(&mut self) {
        self.open_subsonic_playlist_identity = None;
        self.open_subsonic_pending_playlist = None;
    }

    pub(super) fn poll_open_subsonic_playlist_for_shutdown(
        &mut self,
    ) -> Option<crate::open_subsonic::ServiceError> {
        match poll_playlist_projection_receipt(&mut self.open_subsonic_pending_playlist) {
            PlaylistProjectionReceipt::Durable(identity) => {
                self.open_subsonic_playlist_identity = Some(identity);
                None
            }
            PlaylistProjectionReceipt::Retry(Some(error)) => Some(error),
            PlaylistProjectionReceipt::Idle
            | PlaylistProjectionReceipt::Waiting
            | PlaylistProjectionReceipt::Retry(None) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{
        PendingOpenSubsonicRatingProjection, RatingProjectionReceipt,
        poll_rating_projection_receipt,
    };
    use super::*;

    #[test]
    fn tui_playlist_projection_waits_for_durability_receipt() {
        let (reply, receipt) = tokio::sync::oneshot::channel();
        let mut pending = Some(PendingOpenSubsonicPlaylistProjection {
            identity: "durable-playlists".to_owned(),
            receipt,
        });

        assert!(matches!(
            poll_playlist_projection_receipt(&mut pending),
            PlaylistProjectionReceipt::Waiting
        ));
        assert!(pending.is_some());

        reply.send(Ok(())).unwrap();
        assert!(matches!(
            poll_playlist_projection_receipt(&mut pending),
            PlaylistProjectionReceipt::Durable(identity) if identity == "durable-playlists"
        ));
        assert!(pending.is_none());
    }

    #[test]
    fn tui_playlist_receipt_progresses_while_rating_receipt_is_pending() {
        let (_rating_reply, rating_receipt) = tokio::sync::oneshot::channel();
        let (playlist_reply, playlist_receipt) = tokio::sync::oneshot::channel();
        let mut rating = Some(PendingOpenSubsonicRatingProjection {
            identity: "rating-ledger".to_owned(),
            receipt: rating_receipt,
        });
        let mut playlist = Some(PendingOpenSubsonicPlaylistProjection {
            identity: "playlist-ledger".to_owned(),
            receipt: playlist_receipt,
        });
        playlist_reply.send(Ok(())).unwrap();

        assert!(matches!(
            poll_rating_projection_receipt(&mut rating),
            RatingProjectionReceipt::Waiting
        ));
        assert!(matches!(
            poll_playlist_projection_receipt(&mut playlist),
            PlaylistProjectionReceipt::Durable(identity) if identity == "playlist-ledger"
        ));
        assert!(rating.is_some());
        assert!(playlist.is_none());
    }
}
