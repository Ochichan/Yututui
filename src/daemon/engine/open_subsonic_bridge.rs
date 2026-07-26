//! Daemon owner commit for exactly-once OpenSubsonic observations.

use super::DaemonEngine;

pub(super) struct PendingOpenSubsonicScrobble {
    event_id: String,
    kind: crate::open_subsonic::OpenSubsonicScrobbleKind,
    track: crate::scrobble::ScrobbleTrack,
    confirmation: Option<crate::scrobble::OpenSubsonicSubmissionAck>,
    receipt: Option<crate::open_subsonic::OpenSubsonicScrobbleReceipt>,
    source_receipt: Option<crate::open_subsonic::OpenSubsonicScrobbleReceipt>,
    bridge_durable: bool,
    source_ack_durable: bool,
}

pub(super) struct PendingOpenSubsonicRatingProjection {
    identity: String,
    receipt: crate::open_subsonic::OpenSubsonicRatingReceipt,
}

fn same_server_track(
    left: &crate::scrobble::ScrobbleTrack,
    right: &crate::scrobble::ScrobbleTrack,
) -> bool {
    match (&left.open_subsonic_item, &right.open_subsonic_item) {
        (Some(left), Some(right)) => left == right,
        _ => left.key == right.key,
    }
}

fn wrong_account_scope(error: &crate::open_subsonic::ServiceError) -> bool {
    matches!(
        error,
        crate::open_subsonic::ServiceError::Server(
            crate::open_subsonic::ServerError::WrongAccountScope
        )
    )
}

fn request_scope_retirement(pending: &PendingOpenSubsonicScrobble) {
    if let Some(confirmation) = pending.confirmation.as_ref()
        && let Err(error) = confirmation.retire_account_scope()
    {
        tracing::debug!(
            %error,
            "retired daemon music-server account marker waits for source-journal durability"
        );
    }
}

fn admit_pending_scrobble(
    queue: &mut std::collections::VecDeque<PendingOpenSubsonicScrobble>,
    pending: PendingOpenSubsonicScrobble,
) -> bool {
    use crate::open_subsonic::{OWNER_PLAYBACK_REPORT_QUEUE_MAX, OpenSubsonicScrobbleKind as Kind};

    if queue
        .iter()
        .any(|queued| queued.event_id == pending.event_id)
    {
        return false;
    }

    if pending.kind == Kind::NowPlaying {
        let mut index = 0;
        while index < queue.len() {
            let duplicate = queue.get(index).is_some_and(|queued| {
                queued.kind == Kind::NowPlaying && same_server_track(&queued.track, &pending.track)
            });
            if duplicate {
                queue.remove(index);
            } else {
                index += 1;
            }
        }
    }

    while queue.len() >= OWNER_PLAYBACK_REPORT_QUEUE_MAX {
        if let Some(index) = queue
            .iter()
            .position(|queued| queued.kind == Kind::NowPlaying)
        {
            queue.remove(index);
            continue;
        }
        if pending.kind == Kind::NowPlaying {
            tracing::debug!(
                capacity = OWNER_PLAYBACK_REPORT_QUEUE_MAX,
                "ephemeral daemon music server now-playing report coalesced at capacity"
            );
            return false;
        }
        // All retained submissions may be between two durable acknowledgement boundaries. Keep
        // them stable and defer this newly announced marker to restart replay.
        tracing::warn!(
            capacity = OWNER_PLAYBACK_REPORT_QUEUE_MAX,
            "daemon music server submission queue full; newest journaled report deferred"
        );
        if let Some(confirmation) = pending.confirmation.as_ref() {
            confirmation.defer_submission();
        }
        return false;
    }
    queue.push_back(pending);
    true
}

impl DaemonEngine {
    /// Persist a server observation before exposing its projection or acknowledging the bridge.
    pub(in crate::daemon) fn apply_open_subsonic_bridge_import(
        &mut self,
        import: &crate::open_subsonic::OpenSubsonicBridgeImport,
    ) -> Result<String, crate::personal_state::PersonalStateError> {
        let current = match &self.personal_state_device_id {
            Some(device_id) => crate::personal_state::reconcile_runtime_as(
                &self.personal_state,
                device_id,
                &self.library,
                &self.playlists,
                &self.signals,
                &self.station,
            )?,
            None => crate::personal_state::reconcile_runtime(
                &self.personal_state,
                &self.library,
                &self.playlists,
                &self.signals,
                &self.station,
            )?,
        };
        let (candidate, envelope_id) = match &self.personal_state_device_id {
            Some(device_id) => {
                let envelope_id = crate::personal_state::external_operation_envelope_id(
                    device_id,
                    import.operation_id(),
                )?;
                let candidate = crate::personal_state::append_external_operation_as(
                    &current,
                    device_id,
                    import.operation_id().to_owned(),
                    import.origin()?,
                    import.operation(),
                    import.observed_at_unix(),
                )?;
                (candidate, envelope_id)
            }
            None => {
                let envelope_id = crate::personal_state::external_operation_envelope_id_for_state(
                    &current,
                    import.operation_id(),
                )?;
                let candidate = crate::personal_state::append_external_operation(
                    &current,
                    import.operation_id().to_owned(),
                    import.origin()?,
                    import.operation(),
                    import.observed_at_unix(),
                )?;
                (candidate, envelope_id)
            }
        };
        let commit = crate::personal_state::PersonalStateCommit::prepare_for_runtime(
            candidate,
            self.playlists.revision(),
        )?;
        let paths = self.personal_state_paths()?;
        let installed = commit.commit(&paths)?;
        if installed != *commit.state() {
            return Err(crate::personal_state::PersonalStateError::ProjectionMismatch);
        }

        let (library, mut playlists, signals, station) = commit.runtime_stores();
        let playlists_changed = playlists.inherit_revision_from(&self.playlists);
        self.install_personal_state(installed);
        self.library = library;
        self.playlists = playlists;
        self.signals = signals;
        self.station = station;
        if playlists_changed {
            self.bump_playlists_rev();
        }
        self.library_invalidations = self.library_invalidations.wrapping_add(1);
        Ok(envelope_id)
    }

    pub(in crate::daemon) fn accept_open_subsonic_bridge_import(
        &mut self,
        import: &crate::open_subsonic::OpenSubsonicBridgeImport,
    ) {
        if let Err(error) = self.apply_open_subsonic_bridge_import(import) {
            tracing::warn!(%error, "music server observation could not be committed");
            return;
        }
        if let Some(handle) = crate::open_subsonic::current_handle()
            && let Err(error) = handle.acknowledge_bridge_import(import.operation_id())
        {
            tracing::debug!(%error, "music server observation acknowledgement will retry");
        }
    }

    pub(in crate::daemon) fn queue_open_subsonic_scrobble(
        &mut self,
        event_id: String,
        kind: crate::open_subsonic::OpenSubsonicScrobbleKind,
        track: crate::scrobble::ScrobbleTrack,
        confirmation: Option<crate::scrobble::OpenSubsonicSubmissionAck>,
    ) {
        let bridge_durable = confirmation
            .as_ref()
            .is_some_and(crate::scrobble::OpenSubsonicSubmissionAck::bridge_marker_is_durable);
        let _admitted = admit_pending_scrobble(
            &mut self.open_subsonic_pending_scrobbles,
            PendingOpenSubsonicScrobble {
                event_id,
                kind,
                track,
                confirmation,
                receipt: None,
                source_receipt: None,
                bridge_durable,
                source_ack_durable: false,
            },
        );
        self.drive_open_subsonic_scrobbles();
    }

    pub(in crate::daemon) fn maintain_open_subsonic_bridge(&mut self) {
        self.drive_open_subsonic_scrobbles();
        if let Some(pending) = self.open_subsonic_pending_rating.as_mut() {
            match pending.receipt.try_recv() {
                Ok(Ok(())) => {
                    let pending = self
                        .open_subsonic_pending_rating
                        .take()
                        .expect("rating receipt was present");
                    self.open_subsonic_rating_identity = Some(pending.identity);
                }
                Ok(Err(error)) => {
                    tracing::warn!(
                        reason = %error,
                        "music server ratings are waiting for local durability"
                    );
                    self.open_subsonic_pending_rating = None;
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => return,
                Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                    self.open_subsonic_pending_rating = None;
                }
            }
        }
        let Some(handle) = crate::open_subsonic::current_handle() else {
            return;
        };

        let Ok(identity) = self.personal_state.identity() else {
            return;
        };
        if self.open_subsonic_rating_identity.as_deref() == Some(identity.as_str()) {
            return;
        }
        let winners =
            match crate::personal_state::open_subsonic_rating_winners(&self.personal_state) {
                Ok(winners) => winners,
                Err(error) => {
                    tracing::warn!(%error, "music server rating projection could not be prepared");
                    return;
                }
            };
        if let Ok(receipt) = handle.reconcile_ratings(winners) {
            self.open_subsonic_pending_rating =
                Some(PendingOpenSubsonicRatingProjection { identity, receipt });
        }
    }

    /// Poll only receipts which are already ready. An unresolved Submission remains protected by
    /// the scrobble JSONL marker; NowPlaying is intentionally ephemeral.
    pub(in crate::daemon) fn pump_open_subsonic_scrobbles_for_shutdown(
        &mut self,
    ) -> Result<(), crate::open_subsonic::ServiceError> {
        loop {
            let Some(pending) = self.open_subsonic_pending_scrobbles.front_mut() else {
                return Ok(());
            };
            if pending
                .confirmation
                .as_ref()
                .is_some_and(crate::scrobble::OpenSubsonicSubmissionAck::account_scope_is_retired)
            {
                self.open_subsonic_pending_scrobbles.pop_front();
                continue;
            }
            if pending.confirmation.as_ref().is_some_and(
                crate::scrobble::OpenSubsonicSubmissionAck::account_scope_retirement_is_pending,
            ) {
                return Ok(());
            }
            if pending.bridge_durable {
                if pending.kind == crate::open_subsonic::OpenSubsonicScrobbleKind::Submission
                    && let Some(confirmation) = pending.confirmation.as_ref()
                {
                    if !confirmation.bridge_marker_is_durable() {
                        let _ = confirmation.confirm_bridge_durable();
                    } else if pending.source_ack_durable {
                        let _ = confirmation.confirm_source_acknowledged();
                    } else if let Some(receipt) = pending.source_receipt.as_mut() {
                        match receipt.try_recv() {
                            Ok(Ok(())) => {
                                let _ = confirmation.confirm_source_acknowledged();
                            }
                            Ok(Err(error)) if wrong_account_scope(&error) => {
                                pending.source_receipt = None;
                                request_scope_retirement(pending);
                                return Ok(());
                            }
                            Ok(Err(error)) => return Err(error),
                            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
                            | Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {}
                        }
                    } else if let Some(handle) = crate::open_subsonic::current_handle() {
                        let _ = handle.acknowledge_scrobble_source(
                            pending.event_id.clone(),
                            pending.track.clone(),
                        );
                    }
                }
                self.open_subsonic_pending_scrobbles.pop_front();
                continue;
            }
            if let Some(receipt) = pending.receipt.as_mut() {
                match receipt.try_recv() {
                    Ok(Ok(())) => {
                        pending.receipt = None;
                        pending.bridge_durable = true;
                        continue;
                    }
                    Ok(Err(error)) if wrong_account_scope(&error) => {
                        pending.receipt = None;
                        if pending.kind
                            == crate::open_subsonic::OpenSubsonicScrobbleKind::NowPlaying
                        {
                            self.open_subsonic_pending_scrobbles.pop_front();
                            continue;
                        }
                        request_scope_retirement(pending);
                        return Ok(());
                    }
                    Ok(Err(error))
                        if pending.kind
                            == crate::open_subsonic::OpenSubsonicScrobbleKind::NowPlaying =>
                    {
                        tracing::debug!(
                            reason = %error,
                            "ephemeral music server now-playing report retired during shutdown"
                        );
                        self.open_subsonic_pending_scrobbles.pop_front();
                        continue;
                    }
                    Ok(Err(error)) => return Err(error),
                    Err(tokio::sync::oneshot::error::TryRecvError::Empty) => return Ok(()),
                    Err(tokio::sync::oneshot::error::TryRecvError::Closed) => return Ok(()),
                }
            }
            let Some(handle) = crate::open_subsonic::current_handle() else {
                return Ok(());
            };
            match handle.queue_scrobble(
                pending.event_id.clone(),
                pending.kind,
                pending.track.clone(),
            ) {
                Ok(receipt) => {
                    pending.receipt = Some(receipt);
                }
                Err(error) => {
                    tracing::debug!(
                        reason = %error,
                        "music server playback report remains journaled for restart replay"
                    );
                    return Ok(());
                }
            }
        }
    }

    fn drive_open_subsonic_scrobbles(&mut self) {
        loop {
            let Some(pending) = self.open_subsonic_pending_scrobbles.front_mut() else {
                return;
            };
            if pending
                .confirmation
                .as_ref()
                .is_some_and(crate::scrobble::OpenSubsonicSubmissionAck::account_scope_is_retired)
            {
                self.open_subsonic_pending_scrobbles.pop_front();
                continue;
            }
            if pending.confirmation.as_ref().is_some_and(
                crate::scrobble::OpenSubsonicSubmissionAck::account_scope_retirement_is_pending,
            ) {
                return;
            }
            if pending.kind == crate::open_subsonic::OpenSubsonicScrobbleKind::NowPlaying
                && pending.bridge_durable
            {
                self.open_subsonic_pending_scrobbles.pop_front();
                continue;
            }
            if !pending.bridge_durable {
                if let Some(receipt) = pending.receipt.as_mut() {
                    match receipt.try_recv() {
                        Ok(Ok(())) => {
                            pending.receipt = None;
                            pending.bridge_durable = true;
                            continue;
                        }
                        Ok(Err(error)) if wrong_account_scope(&error) => {
                            pending.receipt = None;
                            if pending.kind
                                == crate::open_subsonic::OpenSubsonicScrobbleKind::NowPlaying
                            {
                                self.open_subsonic_pending_scrobbles.pop_front();
                                continue;
                            }
                            request_scope_retirement(pending);
                            return;
                        }
                        Ok(Err(error)) => {
                            tracing::warn!(
                                reason = %error,
                                "music server playback report is waiting for local durability"
                            );
                            pending.receipt = None;
                            return;
                        }
                        Err(tokio::sync::oneshot::error::TryRecvError::Empty) => return,
                        Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                            pending.receipt = None;
                            return;
                        }
                    }
                }
                let Some(handle) = crate::open_subsonic::current_handle() else {
                    return;
                };
                match handle.queue_scrobble(
                    pending.event_id.clone(),
                    pending.kind,
                    pending.track.clone(),
                ) {
                    Ok(receipt) => {
                        pending.receipt = Some(receipt);
                        return;
                    }
                    Err(_) => return,
                }
            }

            let Some(confirmation) = pending.confirmation.as_ref() else {
                self.open_subsonic_pending_scrobbles.pop_front();
                continue;
            };
            if !confirmation.bridge_marker_is_durable() {
                match confirmation.confirm_bridge_durable() {
                    Ok(()) | Err(crate::util::delivery::DeliveryError::Busy) => return,
                    Err(crate::util::delivery::DeliveryError::Closed) => {
                        self.open_subsonic_pending_scrobbles.pop_front();
                        continue;
                    }
                    Err(_) => return,
                }
            }
            if !pending.source_ack_durable {
                if let Some(receipt) = pending.source_receipt.as_mut() {
                    match receipt.try_recv() {
                        Ok(Ok(())) => {
                            pending.source_receipt = None;
                            pending.source_ack_durable = true;
                            continue;
                        }
                        Ok(Err(error)) if wrong_account_scope(&error) => {
                            pending.source_receipt = None;
                            request_scope_retirement(pending);
                            return;
                        }
                        Ok(Err(error)) => {
                            tracing::warn!(
                                reason = %error,
                                "music server source acknowledgement is waiting for durability"
                            );
                            pending.source_receipt = None;
                            return;
                        }
                        Err(tokio::sync::oneshot::error::TryRecvError::Empty) => return,
                        Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                            pending.source_receipt = None;
                            return;
                        }
                    }
                }
                let Some(handle) = crate::open_subsonic::current_handle() else {
                    return;
                };
                match handle
                    .acknowledge_scrobble_source(pending.event_id.clone(), pending.track.clone())
                {
                    Ok(receipt) => {
                        pending.source_receipt = Some(receipt);
                        return;
                    }
                    Err(_) => return,
                }
            }
            if confirmation.source_marker_is_removed() {
                self.open_subsonic_pending_scrobbles.pop_front();
                continue;
            }
            match confirmation.confirm_source_acknowledged() {
                Ok(()) | Err(crate::util::delivery::DeliveryError::Busy) => return,
                Err(crate::util::delivery::DeliveryError::Closed) => {
                    self.open_subsonic_pending_scrobbles.pop_front();
                    continue;
                }
                Err(_) => return,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::personal_state::{EngagementKind, PortableTrack, PortableTrackKey, Rating};

    fn scrobble_track() -> crate::scrobble::ScrobbleTrack {
        crate::scrobble::ScrobbleTrack {
            key: "server-song".to_owned(),
            open_subsonic_item: None,
            artist: "Artist".to_owned(),
            title: "Song".to_owned(),
            album: None,
            duration_secs: Some(180),
            origin_url: None,
            started_unix: 100,
        }
    }

    fn pending(
        event_id: impl Into<String>,
        kind: crate::open_subsonic::OpenSubsonicScrobbleKind,
        confirmation: Option<crate::scrobble::OpenSubsonicSubmissionAck>,
    ) -> super::PendingOpenSubsonicScrobble {
        super::PendingOpenSubsonicScrobble {
            event_id: event_id.into(),
            kind,
            track: scrobble_track(),
            confirmation,
            receipt: None,
            source_receipt: None,
            bridge_durable: false,
            source_ack_durable: false,
        }
    }

    fn track() -> PortableTrack {
        PortableTrack {
            key: PortableTrackKey::OpenSubsonic {
                backend_id: "backend".to_owned(),
                account_scope_id: "account".to_owned(),
                item_id: "song".to_owned(),
            },
            title: "Song".to_owned(),
            artist: "Artist".to_owned(),
            album: None,
            duration_secs: Some(180),
            isrc: None,
        }
    }

    #[test]
    fn daemon_commits_replayed_server_observation_once() {
        let mut engine = super::super::tests::engine_with_queue(&[]);
        let import = crate::open_subsonic::OpenSubsonicBridgeImport::Rating {
            operation_id: "daemon-server-rating".to_owned(),
            track: track(),
            rating: Rating::Liked,
            observed_at_unix: 100,
        };

        let envelope_id = engine.apply_open_subsonic_bridge_import(&import).unwrap();
        assert_eq!(
            engine.apply_open_subsonic_bridge_import(&import).unwrap(),
            envelope_id
        );

        assert_eq!(
            engine
                .personal_state
                .operations
                .iter()
                .filter(|operation| operation.operation_id == envelope_id)
                .count(),
            1
        );
        assert_ne!(envelope_id, import.operation_id());
        assert_eq!(engine.library.favorites.len(), 1);
    }

    #[test]
    fn app_and_daemon_reduce_server_rating_and_history_in_lockstep() {
        let mut app = crate::app::App::new(50);
        let initial = crate::personal_state::legacy_state(
            &app.library,
            &app.playlists,
            &app.signals,
            &app.station,
        )
        .unwrap();
        app.install_personal_state_runtime(initial).unwrap();
        let mut engine = super::super::tests::engine_with_queue(&[]);
        let imports = [
            crate::open_subsonic::OpenSubsonicBridgeImport::Rating {
                operation_id: "shared-rating".to_owned(),
                track: track(),
                rating: Rating::Disliked,
                observed_at_unix: 100,
            },
            crate::open_subsonic::OpenSubsonicBridgeImport::Engagement {
                operation_id: "shared-play".to_owned(),
                track: track(),
                engagement: EngagementKind::Play,
                played_duration_ms: None,
                total_duration_ms: Some(180_000),
                artist_key: "artist".to_owned(),
                observed_at_unix: 101,
            },
        ];
        for import in &imports {
            app.apply_open_subsonic_bridge_import(import).unwrap();
            engine.apply_open_subsonic_bridge_import(import).unwrap();
        }

        let app_projection = crate::personal_state::project(&app.personal_state.ledger).unwrap();
        let daemon_projection = crate::personal_state::project(&engine.personal_state).unwrap();
        assert_eq!(
            app_projection.legacy.favorites,
            daemon_projection.legacy.favorites
        );
        assert_eq!(
            app_projection.legacy.signals,
            daemon_projection.legacy.signals
        );
    }

    #[test]
    fn daemon_keeps_scrobble_buffered_until_durability_receipt() {
        let mut engine = super::super::tests::engine_with_queue(&[]);
        let (reply, receipt) = tokio::sync::oneshot::channel();
        let (confirmation_tx, mut confirmation_rx) = tokio::sync::mpsc::channel(2);
        engine
            .open_subsonic_pending_scrobbles
            .push_back(super::PendingOpenSubsonicScrobble {
                event_id: "server-play-1".to_owned(),
                kind: crate::open_subsonic::OpenSubsonicScrobbleKind::Submission,
                track: scrobble_track(),
                confirmation: Some(crate::scrobble::OpenSubsonicSubmissionAck::new(
                    "server-play-1".to_owned(),
                    confirmation_tx,
                )),
                receipt: Some(receipt),
                source_receipt: None,
                bridge_durable: false,
                source_ack_durable: false,
            });

        engine.drive_open_subsonic_scrobbles();
        assert_eq!(engine.open_subsonic_pending_scrobbles.len(), 1);
        assert!(matches!(
            confirmation_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));

        reply.send(Ok(())).unwrap();
        engine.drive_open_subsonic_scrobbles();
        assert_eq!(engine.open_subsonic_pending_scrobbles.len(), 1);
        assert!(
            confirmation_rx.try_recv().is_ok(),
            "the intermediate marker rewrite starts only after bridge-store durability"
        );
    }

    #[test]
    fn daemon_retires_only_wrong_scope_then_advances_the_valid_fifo_tail() {
        use crate::open_subsonic::{OpenSubsonicScrobbleKind as Kind, ServerError, ServiceError};

        let mut engine = super::super::tests::engine_with_queue(&[]);
        let (ack_tx, mut ack_rx) = tokio::sync::mpsc::channel(4);
        let old_confirmation =
            crate::scrobble::OpenSubsonicSubmissionAck::new("old-a".to_owned(), ack_tx.clone());
        let (old_reply, old_receipt) = tokio::sync::oneshot::channel();
        old_reply
            .send(Err(ServiceError::Server(ServerError::WrongAccountScope)))
            .unwrap();
        let mut old = pending("old-a", Kind::Submission, Some(old_confirmation.clone()));
        old.receipt = Some(old_receipt);

        let new_confirmation =
            crate::scrobble::OpenSubsonicSubmissionAck::new("valid-b".to_owned(), ack_tx);
        let (new_reply, new_receipt) = tokio::sync::oneshot::channel();
        new_reply.send(Ok(())).unwrap();
        let mut valid = pending("valid-b", Kind::Submission, Some(new_confirmation.clone()));
        valid.receipt = Some(new_receipt);
        engine.open_subsonic_pending_scrobbles.push_back(old);
        engine.open_subsonic_pending_scrobbles.push_back(valid);

        engine.drive_open_subsonic_scrobbles();
        assert!(old_confirmation.account_scope_retirement_is_pending());
        assert_eq!(
            engine
                .open_subsonic_pending_scrobbles
                .front()
                .unwrap()
                .event_id,
            "old-a"
        );
        assert!(ack_rx.try_recv().is_ok());

        old_confirmation.mark_account_scope_retired();
        engine.drive_open_subsonic_scrobbles();
        let current = engine.open_subsonic_pending_scrobbles.front().unwrap();
        assert_eq!(current.event_id, "valid-b");
        assert!(current.bridge_durable);
        assert!(
            ack_rx.try_recv().is_ok(),
            "the valid profile-B submission reaches its bridge acknowledgement in the same run"
        );
        assert!(!new_confirmation.account_scope_retirement_is_pending());
    }

    #[test]
    fn daemon_retires_awaiting_wrong_scope_and_never_retires_transient_setup_failure() {
        use crate::open_subsonic::{OpenSubsonicScrobbleKind as Kind, ServerError, ServiceError};

        let mut engine = super::super::tests::engine_with_queue(&[]);
        let (ack_tx, mut ack_rx) = tokio::sync::mpsc::channel(4);
        let awaiting_confirmation =
            crate::scrobble::OpenSubsonicSubmissionAck::new_with_bridge_marker(
                "awaiting-a".to_owned(),
                ack_tx.clone(),
            );
        let (source_reply, source_receipt) = tokio::sync::oneshot::channel();
        source_reply
            .send(Err(ServiceError::Server(ServerError::WrongAccountScope)))
            .unwrap();
        let mut awaiting = pending(
            "awaiting-a",
            Kind::Submission,
            Some(awaiting_confirmation.clone()),
        );
        awaiting.bridge_durable = true;
        awaiting.source_receipt = Some(source_receipt);
        engine.open_subsonic_pending_scrobbles.push_back(awaiting);

        engine.drive_open_subsonic_scrobbles();
        assert!(awaiting_confirmation.account_scope_retirement_is_pending());
        assert!(ack_rx.try_recv().is_ok());

        engine.open_subsonic_pending_scrobbles.clear();
        let transient_confirmation =
            crate::scrobble::OpenSubsonicSubmissionAck::new("offline".to_owned(), ack_tx);
        let (reply, receipt) = tokio::sync::oneshot::channel();
        reply.send(Err(ServiceError::InvalidSetup)).unwrap();
        let mut transient = pending(
            "offline",
            Kind::Submission,
            Some(transient_confirmation.clone()),
        );
        transient.receipt = Some(receipt);
        engine.open_subsonic_pending_scrobbles.push_back(transient);

        engine.drive_open_subsonic_scrobbles();
        assert!(!transient_confirmation.account_scope_retirement_is_pending());
        assert_eq!(engine.open_subsonic_pending_scrobbles.len(), 1);
    }

    #[test]
    fn daemon_wrong_scope_now_playing_never_blocks_a_valid_submission() {
        use crate::open_subsonic::{OpenSubsonicScrobbleKind as Kind, ServerError, ServiceError};

        let mut engine = super::super::tests::engine_with_queue(&[]);
        let (old_reply, old_receipt) = tokio::sync::oneshot::channel();
        old_reply
            .send(Err(ServiceError::Server(ServerError::WrongAccountScope)))
            .unwrap();
        let mut old = pending("now-a", Kind::NowPlaying, None);
        old.receipt = Some(old_receipt);
        let (valid_reply, valid_receipt) = tokio::sync::oneshot::channel();
        valid_reply.send(Ok(())).unwrap();
        let mut valid = pending("valid-b", Kind::NowPlaying, None);
        valid.receipt = Some(valid_receipt);
        engine.open_subsonic_pending_scrobbles.push_back(old);
        engine.open_subsonic_pending_scrobbles.push_back(valid);

        engine.drive_open_subsonic_scrobbles();
        assert!(
            engine.open_subsonic_pending_scrobbles.is_empty(),
            "the stale ephemeral report is removed and the valid tail progresses immediately"
        );
    }

    #[test]
    fn daemon_shutdown_pump_never_waits_for_an_unresolved_bridge_receipt() {
        let mut engine = super::super::tests::engine_with_queue(&[]);
        let (_reply, receipt) = tokio::sync::oneshot::channel();
        engine
            .open_subsonic_pending_scrobbles
            .push_back(super::PendingOpenSubsonicScrobble {
                event_id: "server-play-in-flight".to_owned(),
                kind: crate::open_subsonic::OpenSubsonicScrobbleKind::Submission,
                track: scrobble_track(),
                confirmation: None,
                receipt: Some(receipt),
                source_receipt: None,
                bridge_durable: false,
                source_ack_durable: false,
            });

        assert!(engine.pump_open_subsonic_scrobbles_for_shutdown().is_ok());
        assert_eq!(
            engine.open_subsonic_pending_scrobbles.len(),
            1,
            "the JSONL journal, not shutdown waiting, owns unresolved submission recovery"
        );
    }

    #[test]
    fn daemon_advances_rating_identity_only_after_durability_receipt() {
        let mut engine = super::super::tests::engine_with_queue(&[]);
        let (reply, receipt) = tokio::sync::oneshot::channel();
        engine.open_subsonic_pending_rating = Some(super::PendingOpenSubsonicRatingProjection {
            identity: "durable-ledger".to_owned(),
            receipt,
        });

        engine.maintain_open_subsonic_bridge();
        assert_eq!(engine.open_subsonic_rating_identity, None);
        assert!(engine.open_subsonic_pending_rating.is_some());

        reply.send(Ok(())).unwrap();
        engine.maintain_open_subsonic_bridge();
        assert_eq!(
            engine.open_subsonic_rating_identity.as_deref(),
            Some("durable-ledger")
        );
        assert!(engine.open_subsonic_pending_rating.is_none());
    }

    #[test]
    fn daemon_owner_scrobble_queue_is_bounded_and_ephemeral_reports_yield_first() {
        use crate::open_subsonic::{
            OWNER_PLAYBACK_REPORT_QUEUE_MAX, OpenSubsonicScrobbleKind as Kind,
        };

        let mut queue = std::collections::VecDeque::new();
        for index in 0..OWNER_PLAYBACK_REPORT_QUEUE_MAX {
            queue.push_back(pending(format!("now-{index}"), Kind::NowPlaying, None));
        }
        assert!(super::admit_pending_scrobble(
            &mut queue,
            pending("submission", Kind::Submission, None)
        ));
        assert_eq!(queue.len(), OWNER_PLAYBACK_REPORT_QUEUE_MAX);
        assert!(queue.iter().any(|queued| queued.kind == Kind::Submission));

        queue.clear();
        for index in 0..OWNER_PLAYBACK_REPORT_QUEUE_MAX {
            queue.push_back(pending(
                format!("submission-{index}"),
                Kind::Submission,
                None,
            ));
        }
        assert!(!super::admit_pending_scrobble(
            &mut queue,
            pending("now-at-cap", Kind::NowPlaying, None)
        ));
        assert_eq!(queue.len(), OWNER_PLAYBACK_REPORT_QUEUE_MAX);
    }

    #[test]
    fn daemon_submission_backpressure_retries_after_same_run_capacity_release() {
        use crate::open_subsonic::{
            OWNER_PLAYBACK_REPORT_QUEUE_MAX, OpenSubsonicScrobbleKind as Kind,
        };

        let (confirmation_tx, mut confirmation_rx) = tokio::sync::mpsc::channel(2);
        let _keepalive = confirmation_tx.clone();
        let oldest_confirmation =
            crate::scrobble::OpenSubsonicSubmissionAck::new("oldest".to_owned(), confirmation_tx);
        let mut queue = std::collections::VecDeque::new();
        queue.push_back(pending(
            "oldest",
            Kind::Submission,
            Some(oldest_confirmation),
        ));
        for index in 1..OWNER_PLAYBACK_REPORT_QUEUE_MAX {
            queue.push_back(pending(
                format!("submission-{index}"),
                Kind::Submission,
                None,
            ));
        }

        let newest_confirmation = crate::scrobble::OpenSubsonicSubmissionAck::new(
            "newest".to_owned(),
            _keepalive.clone(),
        );
        assert!(!super::admit_pending_scrobble(
            &mut queue,
            pending(
                "newest",
                Kind::Submission,
                Some(newest_confirmation.clone())
            )
        ));
        assert!(queue.iter().any(|queued| queued.event_id == "oldest"));
        assert!(!queue.iter().any(|queued| queued.event_id == "newest"));
        assert!(newest_confirmation.submission_is_deferred());
        assert!(matches!(
            confirmation_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));

        queue.pop_front();
        assert!(newest_confirmation.take_deferred_submission());
        assert!(super::admit_pending_scrobble(
            &mut queue,
            pending(
                "newest",
                Kind::Submission,
                Some(newest_confirmation.clone())
            )
        ));
        assert_eq!(queue.len(), OWNER_PLAYBACK_REPORT_QUEUE_MAX);
        assert_eq!(
            queue
                .iter()
                .filter(|queued| queued.event_id == "newest")
                .count(),
            1
        );

        let duplicate_confirmation =
            crate::scrobble::OpenSubsonicSubmissionAck::new("newest".to_owned(), _keepalive);
        assert!(!super::admit_pending_scrobble(
            &mut queue,
            pending(
                "newest",
                Kind::Submission,
                Some(duplicate_confirmation.clone())
            )
        ));
        assert!(!duplicate_confirmation.submission_is_deferred());
        assert_eq!(queue.len(), OWNER_PLAYBACK_REPORT_QUEUE_MAX);
    }
}
