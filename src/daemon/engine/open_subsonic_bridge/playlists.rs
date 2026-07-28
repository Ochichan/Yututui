//! Linked server-playlist projection on the daemon owner's durability lane.

use super::DaemonEngine;

pub(in crate::daemon::engine) struct PendingOpenSubsonicPlaylistProjection {
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

impl DaemonEngine {
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
        handle: &crate::open_subsonic::OpenSubsonicHandle,
        identity: &str,
        receipt_pending: bool,
    ) {
        if receipt_pending || self.open_subsonic_playlist_identity.as_deref() == Some(identity) {
            return;
        }
        match crate::personal_state::personal_playlist_snapshots(&self.personal_state) {
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
}

#[cfg(test)]
mod tests {
    fn track_for(item_id: &str) -> crate::personal_state::PortableTrack {
        crate::personal_state::PortableTrack {
            key: crate::personal_state::PortableTrackKey::OpenSubsonic {
                backend_id: "backend".to_owned(),
                account_scope_id: "account".to_owned(),
                item_id: item_id.to_owned(),
            },
            title: format!("Song {item_id}"),
            artist: "Artist".to_owned(),
            album: None,
            duration_secs: Some(180),
            isrc: None,
        }
    }

    fn track() -> crate::personal_state::PortableTrack {
        track_for("song")
    }

    fn occurrence_values(
        snapshot: &crate::personal_state::PersonalPlaylistSnapshot,
    ) -> Vec<(&str, &str)> {
        snapshot
            .entries
            .iter()
            .map(|entry| {
                let crate::personal_state::PortableTrackKey::OpenSubsonic { item_id, .. } =
                    &entry.track.key
                else {
                    unreachable!("server playlist contains server tracks")
                };
                (entry.entry_id.as_str(), item_id.as_str())
            })
            .collect()
    }

    #[test]
    fn app_and_daemon_reduce_server_playlist_batch_in_lockstep() {
        let mut app = crate::app::App::new(50);
        let initial = crate::personal_state::legacy_state(
            &app.library,
            &app.playlists,
            &app.signals,
            &app.station,
        )
        .unwrap();
        app.install_personal_state_runtime(initial).unwrap();
        let mut engine = super::super::super::tests::engine_with_queue(&[]);
        let import = crate::open_subsonic::OpenSubsonicBridgeImport::Playlist {
            operation_id: "shared-playlist-batch".to_owned(),
            backend_id: crate::open_subsonic::BackendId::new("backend").unwrap(),
            local_playlist_id: crate::personal_state::PlaylistId::new("server-list").unwrap(),
            purpose: crate::open_subsonic::PendingPlaylistImportPurpose::InitialOrImportCopy,
            operations: vec![
                crate::personal_state::ExternalOperationInput {
                    acknowledgement_id: "shared-playlist".to_owned(),
                    operation: crate::personal_state::Operation::UpsertPlaylist {
                        playlist_id: crate::personal_state::PlaylistId::new("server-list").unwrap(),
                        name: "Server list".to_owned(),
                    },
                    recorded_at_unix: 102,
                },
                crate::personal_state::ExternalOperationInput {
                    acknowledgement_id: "shared-playlist-entry".to_owned(),
                    operation: crate::personal_state::Operation::UpsertPlaylistEntry {
                        playlist_id: crate::personal_state::PlaylistId::new("server-list").unwrap(),
                        entry_id: crate::personal_state::PlaylistEntryId::new("entry").unwrap(),
                        track: track(),
                        after_entry_id: None,
                    },
                    recorded_at_unix: 102,
                },
            ],
        };

        app.apply_open_subsonic_bridge_import(&import).unwrap();
        engine.apply_open_subsonic_bridge_import(&import).unwrap();

        let app_projection = crate::personal_state::project(&app.personal_state.ledger).unwrap();
        let daemon_projection = crate::personal_state::project(&engine.personal_state).unwrap();
        assert_eq!(
            app_projection.legacy.playlists,
            daemon_projection.legacy.playlists
        );
        assert_eq!(app_projection.legacy.playlists.len(), 1);
        assert_eq!(app_projection.legacy.playlists[0].entries.len(), 1);
    }

    #[test]
    fn app_and_daemon_replay_third_state_playlist_merge_exactly_once() {
        use crate::personal_state::{
            ExternalOperationInput, Operation, PlaylistEntryId, PlaylistId,
        };

        let mut app = crate::app::App::new(50);
        let initial_state = crate::personal_state::legacy_state(
            &app.library,
            &app.playlists,
            &app.signals,
            &app.station,
        )
        .unwrap();
        app.install_personal_state_runtime(initial_state).unwrap();
        let mut engine = super::super::super::tests::engine_with_queue(&[]);
        let playlist_id = PlaylistId::new("server-list").unwrap();
        let entry_a = PlaylistEntryId::new("entry-a").unwrap();
        let entry_b = PlaylistEntryId::new("entry-b").unwrap();
        let entry_c = PlaylistEntryId::new("remote-entry-c").unwrap();
        let initial = crate::open_subsonic::OpenSubsonicBridgeImport::Playlist {
            operation_id: "initial-local-desired".to_owned(),
            backend_id: crate::open_subsonic::BackendId::new("backend").unwrap(),
            local_playlist_id: playlist_id.clone(),
            purpose: crate::open_subsonic::PendingPlaylistImportPurpose::InitialOrImportCopy,
            operations: vec![
                ExternalOperationInput {
                    acknowledgement_id: "initial-playlist".to_owned(),
                    operation: Operation::UpsertPlaylist {
                        playlist_id: playlist_id.clone(),
                        name: "Desired".to_owned(),
                    },
                    recorded_at_unix: 100,
                },
                ExternalOperationInput {
                    acknowledgement_id: "initial-a".to_owned(),
                    operation: Operation::UpsertPlaylistEntry {
                        playlist_id: playlist_id.clone(),
                        entry_id: entry_a.clone(),
                        track: track_for("a"),
                        after_entry_id: None,
                    },
                    recorded_at_unix: 100,
                },
                ExternalOperationInput {
                    acknowledgement_id: "initial-b".to_owned(),
                    operation: Operation::UpsertPlaylistEntry {
                        playlist_id: playlist_id.clone(),
                        entry_id: entry_b.clone(),
                        track: track_for("b"),
                        after_entry_id: Some(entry_a.clone()),
                    },
                    recorded_at_unix: 100,
                },
            ],
        };
        let third_state = crate::open_subsonic::OpenSubsonicBridgeImport::Playlist {
            operation_id: "third-state-batch".to_owned(),
            backend_id: crate::open_subsonic::BackendId::new("backend").unwrap(),
            local_playlist_id: playlist_id.clone(),
            purpose: crate::open_subsonic::PendingPlaylistImportPurpose::RemoteObservation,
            operations: vec![
                ExternalOperationInput {
                    acknowledgement_id: "third-state-c".to_owned(),
                    operation: Operation::UpsertPlaylistEntry {
                        playlist_id: playlist_id.clone(),
                        entry_id: entry_c,
                        track: track_for("c"),
                        after_entry_id: Some(entry_b.clone()),
                    },
                    recorded_at_unix: 101,
                },
                ExternalOperationInput {
                    acknowledgement_id: "third-state-move-a".to_owned(),
                    operation: Operation::MovePlaylistEntry {
                        playlist_id: playlist_id.clone(),
                        entry_id: entry_a.clone(),
                        after_entry_id: None,
                    },
                    recorded_at_unix: 101,
                },
                ExternalOperationInput {
                    acknowledgement_id: "third-state-move-b".to_owned(),
                    operation: Operation::MovePlaylistEntry {
                        playlist_id: playlist_id.clone(),
                        entry_id: entry_b,
                        after_entry_id: Some(entry_a),
                    },
                    recorded_at_unix: 101,
                },
            ],
        };

        app.apply_open_subsonic_bridge_import(&initial).unwrap();
        engine.apply_open_subsonic_bridge_import(&initial).unwrap();
        app.apply_open_subsonic_bridge_import(&third_state).unwrap();
        engine
            .apply_open_subsonic_bridge_import(&third_state)
            .unwrap();
        let app_operation_count = app.personal_state.ledger.operations.len();
        let daemon_operation_count = engine.personal_state.operations.len();

        app.apply_open_subsonic_bridge_import(&third_state).unwrap();
        engine
            .apply_open_subsonic_bridge_import(&third_state)
            .unwrap();

        assert_eq!(
            app.personal_state.ledger.operations.len(),
            app_operation_count
        );
        assert_eq!(
            engine.personal_state.operations.len(),
            daemon_operation_count
        );
        let app_snapshot = crate::personal_state::personal_playlist_snapshot(
            &app.personal_state.ledger,
            &playlist_id,
        )
        .unwrap()
        .unwrap();
        let daemon_snapshot =
            crate::personal_state::personal_playlist_snapshot(&engine.personal_state, &playlist_id)
                .unwrap()
                .unwrap();
        assert_eq!(
            occurrence_values(&app_snapshot),
            vec![("entry-a", "a"), ("entry-b", "b"), ("remote-entry-c", "c")]
        );
        assert_eq!(
            occurrence_values(&daemon_snapshot),
            occurrence_values(&app_snapshot)
        );
    }

    #[test]
    fn app_and_daemon_retire_queued_remote_observation_after_local_delete() {
        use crate::personal_state::{ExternalOperationInput, Operation, PlaylistId};

        let mut app = crate::app::App::new(50);
        let initial_state = crate::personal_state::legacy_state(
            &app.library,
            &app.playlists,
            &app.signals,
            &app.station,
        )
        .unwrap();
        app.install_personal_state_runtime(initial_state).unwrap();
        let mut engine = super::super::super::tests::engine_with_queue(&[]);
        let playlist_id = PlaylistId::new("server-list").unwrap();
        let initial = crate::open_subsonic::OpenSubsonicBridgeImport::Playlist {
            operation_id: "initial-before-local-delete".to_owned(),
            backend_id: crate::open_subsonic::BackendId::new("backend").unwrap(),
            local_playlist_id: playlist_id.clone(),
            purpose: crate::open_subsonic::PendingPlaylistImportPurpose::InitialOrImportCopy,
            operations: vec![ExternalOperationInput {
                acknowledgement_id: "initial-before-local-delete-name".to_owned(),
                operation: Operation::UpsertPlaylist {
                    playlist_id: playlist_id.clone(),
                    name: "Current".to_owned(),
                },
                recorded_at_unix: 100,
            }],
        };
        app.apply_open_subsonic_bridge_import(&initial).unwrap();
        engine.apply_open_subsonic_bridge_import(&initial).unwrap();
        assert!(
            std::sync::Arc::make_mut(&mut app.playlists)
                .delete("Current")
                .is_some()
        );
        assert!(engine.playlists.delete("Current").is_some());

        let stale = crate::open_subsonic::OpenSubsonicBridgeImport::Playlist {
            operation_id: "queued-before-local-delete".to_owned(),
            backend_id: crate::open_subsonic::BackendId::new("backend").unwrap(),
            local_playlist_id: playlist_id.clone(),
            purpose: crate::open_subsonic::PendingPlaylistImportPurpose::RemoteObservation,
            operations: vec![ExternalOperationInput {
                acknowledgement_id: "queued-before-local-delete-name".to_owned(),
                operation: Operation::UpsertPlaylist {
                    playlist_id: playlist_id.clone(),
                    name: "Stale remote name".to_owned(),
                },
                recorded_at_unix: 101,
            }],
        };

        assert!(
            app.apply_open_subsonic_bridge_import(&stale)
                .unwrap()
                .is_empty()
        );
        assert!(
            engine
                .apply_open_subsonic_bridge_import(&stale)
                .unwrap()
                .is_empty()
        );
        assert!(
            crate::personal_state::personal_playlist_snapshot(
                &app.personal_state.ledger,
                &playlist_id,
            )
            .unwrap()
            .is_none()
        );
        assert!(
            crate::personal_state::personal_playlist_snapshot(
                &engine.personal_state,
                &playlist_id,
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn daemon_playlist_receipt_progresses_while_rating_receipt_is_pending() {
        let mut engine = super::super::super::tests::engine_with_queue(&[]);
        let identity = engine.personal_state.identity().unwrap();
        let (rating_reply, rating_receipt) = tokio::sync::oneshot::channel();
        let (playlist_reply, playlist_receipt) = tokio::sync::oneshot::channel();
        engine.open_subsonic_pending_rating =
            Some(super::super::PendingOpenSubsonicRatingProjection {
                identity: identity.clone(),
                receipt: rating_receipt,
            });
        engine.open_subsonic_pending_playlist =
            Some(super::PendingOpenSubsonicPlaylistProjection {
                identity: identity.clone(),
                receipt: playlist_receipt,
            });

        engine.maintain_open_subsonic_bridge();
        assert_eq!(engine.open_subsonic_rating_identity, None);
        assert_eq!(engine.open_subsonic_playlist_identity, None);

        playlist_reply.send(Ok(())).unwrap();
        engine.maintain_open_subsonic_bridge();
        assert_eq!(
            engine.open_subsonic_playlist_identity.as_deref(),
            Some(identity.as_str())
        );
        assert!(engine.open_subsonic_pending_playlist.is_none());
        assert!(engine.open_subsonic_pending_rating.is_some());

        rating_reply.send(Ok(())).unwrap();
        engine.maintain_open_subsonic_bridge();
        assert_eq!(
            engine.open_subsonic_rating_identity.as_deref(),
            Some(identity.as_str())
        );
        assert!(engine.open_subsonic_pending_rating.is_none());
    }
}
