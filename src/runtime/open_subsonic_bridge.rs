//! Owner-lane handoff between the credential owner and canonical personal state.

use super::{RuntimeHandles, persist_delivery};
use crate::app::{App, PersistCmd};

mod playlists;
pub(super) use playlists::PendingOpenSubsonicPlaylistProjection;

/// The durable bridge store can expose at most 20,000 rating plus 20,000 engagement imports.
///
/// This owner-side set is only a hand-off ledger. Refusing new hand-offs at the same aggregate
/// bound is lossless: the bridge keeps an unacknowledged import durable and re-emits it later.
const OWNER_BRIDGE_IMPORT_TRACKING_MAX: usize = 40_000;

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
            "retired music-server account marker waits for source-journal durability"
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
                "ephemeral music server now-playing report coalesced at owner queue capacity"
            );
            return false;
        }
        // All retained submissions may be between two durable acknowledgement boundaries. Do not
        // evict one: defer the newly announced marker to restart replay instead.
        tracing::warn!(
            capacity = OWNER_PLAYBACK_REPORT_QUEUE_MAX,
            "music server submission owner queue full; newest journaled report deferred"
        );
        if let Some(confirmation) = pending.confirmation.as_ref() {
            confirmation.defer_submission();
        }
        return false;
    }
    queue.push_back(pending);
    true
}

enum RatingProjectionReceipt {
    Idle,
    Waiting,
    Durable(String),
    Retry(Option<crate::open_subsonic::ServiceError>),
}

fn poll_rating_projection_receipt(
    pending: &mut Option<PendingOpenSubsonicRatingProjection>,
) -> RatingProjectionReceipt {
    let Some(receipt) = pending.as_mut().map(|pending| &mut pending.receipt) else {
        return RatingProjectionReceipt::Idle;
    };
    match receipt.try_recv() {
        Ok(Ok(())) => RatingProjectionReceipt::Durable(
            pending.take().expect("rating receipt was present").identity,
        ),
        Ok(Err(error)) => {
            *pending = None;
            RatingProjectionReceipt::Retry(Some(error))
        }
        Err(tokio::sync::oneshot::error::TryRecvError::Empty) => RatingProjectionReceipt::Waiting,
        Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
            *pending = None;
            RatingProjectionReceipt::Retry(None)
        }
    }
}

fn bridge_import_tracking_has_capacity(
    pending: &std::collections::BTreeMap<String, Vec<String>>,
    committed: &std::collections::BTreeSet<String>,
) -> bool {
    pending.len().saturating_add(committed.len()) < OWNER_BRIDGE_IMPORT_TRACKING_MAX
}

fn bridge_import_is_covered(app: &App, envelope_ids: &[String]) -> bool {
    // Empty means the current durable playlist deletion superseded an older remote observation.
    // The enclosing personal-state commit identity is still checked by the caller.
    envelope_ids.is_empty()
        || envelope_ids.iter().all(|envelope_id| {
            app.personal_state
                .ledger
                .operations
                .iter()
                .any(|operation| operation.operation_id == envelope_id.as_str())
        })
}

impl RuntimeHandles {
    /// Apply one durable server observation and request the personal-state transaction that makes
    /// it acknowledgeable. Re-delivery is expected after crashes and is idempotent by operation ID.
    pub(crate) fn apply_open_subsonic_bridge_import(
        &mut self,
        app: &mut App,
        import: crate::open_subsonic::OpenSubsonicBridgeImport,
    ) {
        let operation_id = import.operation_id().to_owned();
        if self.open_subsonic_committed_imports.contains(&operation_id)
            || self
                .open_subsonic_pending_imports
                .contains_key(&operation_id)
        {
            self.maintain_open_subsonic_bridge(app);
            return;
        }
        if !bridge_import_tracking_has_capacity(
            &self.open_subsonic_pending_imports,
            &self.open_subsonic_committed_imports,
        ) {
            // First try to release locally committed hand-offs. If the credential owner is busy
            // or unavailable, leave this import unacknowledged in its durable source journal.
            self.maintain_open_subsonic_bridge(app);
            if !bridge_import_tracking_has_capacity(
                &self.open_subsonic_pending_imports,
                &self.open_subsonic_committed_imports,
            ) {
                tracing::warn!(
                    capacity = OWNER_BRIDGE_IMPORT_TRACKING_MAX,
                    "music server observation hand-off is full; durable source will retry"
                );
                return;
            }
        }
        let envelope_ids = match app.apply_open_subsonic_bridge_import(&import) {
            Ok(envelope_ids) => envelope_ids,
            Err(error) => {
                tracing::warn!(%error, "music server observation could not be merged");
                return;
            }
        };
        match persist_delivery::admit(&self.persist, app, PersistCmd::Library) {
            Ok(_) => {
                self.open_subsonic_pending_imports
                    .insert(operation_id, envelope_ids);
                debug_assert!(
                    self.open_subsonic_pending_imports
                        .len()
                        .saturating_add(self.open_subsonic_committed_imports.len())
                        <= OWNER_BRIDGE_IMPORT_TRACKING_MAX
                );
            }
            Err(error) => {
                tracing::warn!(
                    %error,
                    "music server observation is waiting for personal-state persistence"
                );
            }
        }
    }

    /// Record which bridge imports are covered by a completed personal-state transaction.
    pub(crate) fn note_open_subsonic_personal_state_commit(
        &mut self,
        app: &App,
        state_identity: &str,
    ) {
        if !app
            .personal_state
            .ledger
            .identity()
            .is_ok_and(|identity| identity == state_identity)
        {
            return;
        }
        let committed = self
            .open_subsonic_pending_imports
            .iter()
            .filter(|(_, envelope_ids)| bridge_import_is_covered(app, envelope_ids))
            .map(|(acknowledgement_id, _)| acknowledgement_id.clone())
            .collect::<Vec<_>>();
        for acknowledgement_id in committed {
            self.open_subsonic_pending_imports
                .remove(&acknowledgement_id);
            self.open_subsonic_committed_imports
                .insert(acknowledgement_id);
        }
    }

    /// Queue one exact playback action into the credential owner's durable bridge store.
    pub(crate) fn queue_open_subsonic_scrobble(
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

    /// Retry acknowledgements, playback reports, and rating projection from the current ledger.
    /// The identity guard makes the common owner turn allocation-free.
    pub(crate) fn maintain_open_subsonic_bridge(&mut self, app: &App) {
        self.drive_open_subsonic_scrobbles();
        let Some(runtime) = self.open_subsonic_runtime.as_ref() else {
            return;
        };
        let handle = runtime.handle();

        let acknowledged = self
            .open_subsonic_committed_imports
            .iter()
            .filter(|operation_id| {
                handle
                    .acknowledge_bridge_import((*operation_id).clone())
                    .is_ok()
            })
            .cloned()
            .collect::<Vec<_>>();
        for operation_id in acknowledged {
            self.open_subsonic_committed_imports.remove(&operation_id);
        }

        let rating_waiting =
            match poll_rating_projection_receipt(&mut self.open_subsonic_pending_rating) {
                RatingProjectionReceipt::Idle => false,
                RatingProjectionReceipt::Waiting => true,
                RatingProjectionReceipt::Durable(identity) => {
                    self.open_subsonic_rating_identity = Some(identity);
                    false
                }
                RatingProjectionReceipt::Retry(Some(error)) => {
                    tracing::warn!(
                        reason = %error,
                        "music server ratings are waiting for local durability"
                    );
                    false
                }
                RatingProjectionReceipt::Retry(None) => false,
            };
        let playlist_waiting = self.poll_open_subsonic_playlist_projection();

        let Ok(identity) = app.personal_state.ledger.identity() else {
            return;
        };
        if !rating_waiting
            && self.open_subsonic_rating_identity.as_deref() != Some(identity.as_str())
        {
            match crate::personal_state::open_subsonic_rating_winners(&app.personal_state.ledger) {
                Ok(winners) => {
                    if let Ok(receipt) = handle.reconcile_ratings(winners) {
                        self.open_subsonic_pending_rating =
                            Some(PendingOpenSubsonicRatingProjection {
                                identity: identity.clone(),
                                receipt,
                            });
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, "music server rating projection could not be prepared");
                }
            }
        }
        self.reconcile_open_subsonic_playlists(app, &handle, &identity, playlist_waiting);
    }

    pub(crate) fn reset_open_subsonic_rating_projection(&mut self) {
        self.open_subsonic_rating_identity = None;
        self.open_subsonic_pending_rating = None;
        self.reset_open_subsonic_playlist_projection();
    }

    /// Give already-ready bridge receipts one final owner-lane pump without waiting for the
    /// credential actor. A Submission that is still queued or in flight remains protected by the
    /// scrobble JSONL marker; ratings remain derivable from the durable personal-state ledger.
    pub(crate) fn pump_open_subsonic_for_shutdown(
        &mut self,
        _app: &App,
    ) -> Result<(), crate::open_subsonic::ServiceError> {
        let mut projection_error = None;
        match poll_rating_projection_receipt(&mut self.open_subsonic_pending_rating) {
            RatingProjectionReceipt::Durable(identity) => {
                self.open_subsonic_rating_identity = Some(identity);
            }
            RatingProjectionReceipt::Retry(Some(error)) => projection_error = Some(error),
            RatingProjectionReceipt::Idle
            | RatingProjectionReceipt::Waiting
            | RatingProjectionReceipt::Retry(None) => {}
        }
        if let Some(error) = self.poll_open_subsonic_playlist_for_shutdown() {
            projection_error.get_or_insert(error);
        }
        if let Some(error) = projection_error {
            return Err(error);
        }

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
                        // This may or may not finish before teardown. Pending and intermediate
                        // journal states are both restart-safe.
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
                    } else if let Some(runtime) = self.open_subsonic_runtime.as_ref() {
                        let _ = runtime.handle().acknowledge_scrobble_source(
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
            if let Some(runtime) = self.open_subsonic_runtime.as_ref() {
                let handle = runtime.handle();
                match handle.queue_scrobble(
                    pending.event_id.clone(),
                    pending.kind,
                    pending.track.clone(),
                ) {
                    Ok(receipt) => {
                        pending.receipt = Some(receipt);
                        continue;
                    }
                    Err(error) => {
                        tracing::debug!(
                            reason = %error,
                            "music server playback report remains journaled for restart replay"
                        );
                    }
                }
            }
            return Ok(());
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
                let Some(runtime) = self.open_subsonic_runtime.as_ref() else {
                    return;
                };
                let handle = runtime.handle();
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
                        // The marker remains in its Pending state and will replay against the
                        // bridge's durable row on restart.
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
                let Some(runtime) = self.open_subsonic_runtime.as_ref() else {
                    return;
                };
                match runtime
                    .handle()
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
                    // The bridge already knows the source is acknowledged. The intermediate
                    // journal marker will replay only this idempotent finalization after restart.
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
    use super::{
        OWNER_BRIDGE_IMPORT_TRACKING_MAX, PendingOpenSubsonicRatingProjection,
        PendingOpenSubsonicScrobble, RatingProjectionReceipt, admit_pending_scrobble,
        bridge_import_is_covered, bridge_import_tracking_has_capacity,
        poll_rating_projection_receipt,
    };

    fn pending(
        event_id: impl Into<String>,
        kind: crate::open_subsonic::OpenSubsonicScrobbleKind,
        confirmation: Option<crate::scrobble::OpenSubsonicSubmissionAck>,
    ) -> PendingOpenSubsonicScrobble {
        PendingOpenSubsonicScrobble {
            event_id: event_id.into(),
            kind,
            track: crate::scrobble::ScrobbleTrack {
                key: "server-song".to_owned(),
                open_subsonic_item: None,
                artist: "Artist".to_owned(),
                title: "Song".to_owned(),
                album: None,
                duration_secs: Some(180),
                origin_url: None,
                started_unix: 100,
            },
            confirmation,
            receipt: None,
            source_receipt: None,
            bridge_durable: false,
            source_ack_durable: false,
        }
    }

    #[test]
    fn tui_rating_projection_waits_for_durability_receipt() {
        let (reply, receipt) = tokio::sync::oneshot::channel();
        let mut pending = Some(PendingOpenSubsonicRatingProjection {
            identity: "durable-ledger".to_owned(),
            receipt,
        });

        assert!(matches!(
            poll_rating_projection_receipt(&mut pending),
            RatingProjectionReceipt::Waiting
        ));
        assert!(pending.is_some());

        reply.send(Ok(())).unwrap();
        assert!(matches!(
            poll_rating_projection_receipt(&mut pending),
            RatingProjectionReceipt::Durable(identity) if identity == "durable-ledger"
        ));
        assert!(pending.is_none());
    }

    #[test]
    fn tui_owner_queue_coalesces_ephemeral_reports_and_bounds_submission_backlog() {
        use crate::open_subsonic::{
            OWNER_PLAYBACK_REPORT_QUEUE_MAX, OpenSubsonicScrobbleKind as Kind,
        };

        let mut queue = std::collections::VecDeque::new();
        assert!(admit_pending_scrobble(
            &mut queue,
            pending("now-1", Kind::NowPlaying, None)
        ));
        assert!(admit_pending_scrobble(
            &mut queue,
            pending("now-2", Kind::NowPlaying, None)
        ));
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.front().unwrap().event_id, "now-2");

        queue.clear();
        for index in 0..OWNER_PLAYBACK_REPORT_QUEUE_MAX {
            queue.push_back(pending(
                format!("submission-{index}"),
                Kind::Submission,
                None,
            ));
        }
        assert!(!admit_pending_scrobble(
            &mut queue,
            pending("now-at-cap", Kind::NowPlaying, None)
        ));
        assert_eq!(queue.len(), OWNER_PLAYBACK_REPORT_QUEUE_MAX);
        assert!(queue.iter().all(|queued| queued.kind == Kind::Submission));
    }

    #[test]
    fn tui_owner_queue_retries_a_deferred_submission_after_same_run_capacity_release() {
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
        assert!(!admit_pending_scrobble(
            &mut queue,
            pending(
                "newest",
                Kind::Submission,
                Some(newest_confirmation.clone())
            )
        ));
        assert_eq!(queue.len(), OWNER_PLAYBACK_REPORT_QUEUE_MAX);
        assert!(queue.iter().any(|queued| queued.event_id == "oldest"));
        assert!(!queue.iter().any(|queued| queued.event_id == "newest"));
        assert!(newest_confirmation.submission_is_deferred());
        assert!(matches!(
            confirmation_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));

        queue.pop_front();
        assert!(newest_confirmation.take_deferred_submission());
        assert!(admit_pending_scrobble(
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
        assert!(!admit_pending_scrobble(
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

    #[test]
    fn tui_scope_retirement_is_distinct_and_only_matches_wrong_account_scope() {
        use crate::open_subsonic::{OpenSubsonicScrobbleKind as Kind, ServerError, ServiceError};

        let (ack_tx, mut ack_rx) = tokio::sync::mpsc::channel(2);
        let confirmation =
            crate::scrobble::OpenSubsonicSubmissionAck::new("old-a".to_owned(), ack_tx);
        let stale = pending("old-a", Kind::Submission, Some(confirmation.clone()));
        let mismatch = ServiceError::Server(ServerError::WrongAccountScope);

        assert!(super::wrong_account_scope(&mismatch));
        assert!(!super::wrong_account_scope(&ServiceError::InvalidSetup));
        super::request_scope_retirement(&stale);
        assert!(confirmation.account_scope_retirement_is_pending());
        assert!(!confirmation.bridge_marker_is_durable());
        assert!(ack_rx.try_recv().is_ok());

        confirmation.mark_account_scope_retired();
        assert!(confirmation.account_scope_is_retired());

        let ephemeral = pending("now-a", Kind::NowPlaying, None);
        super::request_scope_retirement(&ephemeral);
        assert!(matches!(
            ack_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
    }

    #[test]
    fn owner_import_handoff_refuses_overflow_without_inventing_an_acknowledgement() {
        let pending = std::collections::BTreeMap::new();
        let mut committed = std::collections::BTreeSet::new();
        for index in 0..OWNER_BRIDGE_IMPORT_TRACKING_MAX {
            committed.insert(format!("committed-{index}"));
        }

        assert!(!bridge_import_tracking_has_capacity(&pending, &committed));
        assert_eq!(committed.len(), OWNER_BRIDGE_IMPORT_TRACKING_MAX);
        assert!(pending.is_empty());
        assert!(!committed.contains("overflow-import"));
    }

    #[test]
    fn owner_commit_covers_stale_playlist_retirement_without_an_envelope() {
        let app = crate::app::App::new(50);

        assert!(bridge_import_is_covered(&app, &[]));
        assert!(!bridge_import_is_covered(
            &app,
            &["not-in-the-ledger".to_owned()]
        ));
    }
}
