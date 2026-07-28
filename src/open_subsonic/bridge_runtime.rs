//! Durable rating, history, and scrobble bridge driven by the credential-owning actor.

use std::collections::BTreeMap;
use std::ops::Bound::{Excluded, Unbounded};
use std::sync::Mutex;
use std::time::Duration;

use data_encoding::HEXLOWER;
use futures::StreamExt as _;
use sha2::{Digest as _, Sha256};

use super::actor::ServiceError;
use super::bridge_event::{
    OpenSubsonicBridgeSink, OpenSubsonicScrobbleKind, portable_server_track,
};
use super::bridge_store::{
    AggregatePlayShadow, BridgeMutationError, HistoryCursor, MAX_UNCERTAIN_SCROBBLE_READBACKS,
    NativeHistoryHealth, OutboundScrobbleDelivery, OutboundScrobbleEcho, OutboundScrobbleKind,
    PendingEngagementImport, PendingNativeMetadataRow, PendingOutboundScrobble,
    PendingPlaylistProjectionStage, PendingRatingImport, PendingRatingProjection,
    PendingRatingProjectionStage, RatingShadow,
};
use super::catalog::OpenSubsonicCatalog;
use super::client::MutationDeliveryError;
use super::client::OpenSubsonicClient;
use super::history::parse_rfc3339_unix;
use super::model::{
    AccountScopeId, BackendId, ItemId, OpenSubsonicItemRef, ServerLibraryDetail, ServerLibraryPage,
    ServerLibraryRow, ServerSong,
};
use super::native_history::{NativeHistoryError, NavidromeNativeClient};
use super::profile::{OpenSubsonicPaths, StoreError};
use super::rating::{RawServerRating, canonical_server_rating, map_server_rating};
use super::transaction::{
    OpenSubsonicStoreSet, commit_store_set, load_store_set, load_store_set_read_only,
};
use crate::personal_state::{
    EngagementKind, OpenSubsonicRatingWinner, OperationOrigin, PlaylistId, PortableTrack,
    PortableTrackKey,
};

const NATIVE_CURSOR: &str = "navidrome-native";
const MAX_NETWORK_FLUSH_PER_TURN: usize = 1;
const STANDARD_RECENT_ALBUMS: u32 = 20;
const STANDARD_RECENT_SONGS: usize = 2_000;
const STANDARD_ALBUM_CONCURRENCY: usize = 4;
const NATIVE_HISTORY_TOTAL_TIMEOUT: Duration = Duration::from_secs(60);
const STANDARD_HISTORY_TOTAL_TIMEOUT: Duration = Duration::from_secs(30);

mod emission;
mod history_metadata;
mod history_support;
mod outbound_resolution;
mod playlists;

pub(super) use history_support::completed_history_overlap;
use history_support::{
    history_continuation, native_cursor, native_scan_continuation, next_counter_epoch,
    observe_aggregate,
};

/// A credential-owning, read-only network worker. It never mutates the live bridge snapshot.
///
/// The worker is deliberately not `Debug` or `Clone`: one actor tick creates one single-flight
/// value and moves it into an owned, abortable task.
pub(crate) struct HistoryWorker {
    paths: OpenSubsonicPaths,
}

pub(crate) struct HistoryRefreshResult {
    pub(super) backend_id: BackendId,
    pub(super) account_scope_id: AccountScopeId,
    pub(super) base_cursor: Option<HistoryCursor>,
    pub(super) native: Result<Option<NativeHistoryBatch>, NativeHistoryError>,
    pub(super) standard: Result<Vec<ServerSong>, ServiceError>,
}

pub(super) struct NativeHistoryBatch {
    pub(super) rows: Vec<NativeHistoryObservation>,
    pub(super) aggregate_baselines: BTreeMap<ItemId, (u64, Option<String>)>,
    pub(super) next_cursor: Option<HistoryCursor>,
    pub(super) truncated: bool,
    pub(super) metadata_retry_pending: bool,
}

pub(super) struct NativeHistoryObservation {
    pub(super) row_id: u64,
    pub(super) item_id: ItemId,
    pub(super) track: PortableTrack,
    pub(super) observed_at_unix: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HistoryApplyOutcome {
    pub native_error: Option<NativeHistoryError>,
    pub standard_error: Option<ServiceError>,
    pub native_stale: bool,
    pub native_truncated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutboundScrobbleResolution {
    Retry,
    MarkSent,
}

pub(crate) struct BridgeRuntime {
    paths: Option<OpenSubsonicPaths>,
    sink: Option<OpenSubsonicBridgeSink>,
    retry_cursors: Mutex<RetryCursors>,
}

#[derive(Default)]
struct RetryCursors {
    lane_after: Option<RetryLane>,
    playlist_after: Option<PlaylistId>,
    rating_after: Option<ItemId>,
    outbound_after: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RetryLane {
    Playlist,
    Rating,
    Outbound,
}

impl HistoryWorker {
    /// Load a coherent owner-only credential snapshot, then fetch both history sources without
    /// holding the live actor. Each source has a whole-operation deadline; native failure never
    /// prevents the standard aggregate fallback from completing.
    pub(crate) async fn fetch(self) -> Result<HistoryRefreshResult, ServiceError> {
        let paths = self.paths;
        let store_set = tokio::task::spawn_blocking(move || load_store_set_read_only(&paths))
            .await
            .map_err(|_| ServiceError::Store(StoreError::StorageUnavailable))??
            .ok_or(ServiceError::InvalidSetup)?;
        let client = OpenSubsonicClient::connect(&store_set.profile).await?;
        fetch_history_sources(
            &store_set,
            &client,
            NATIVE_HISTORY_TOTAL_TIMEOUT,
            STANDARD_HISTORY_TOTAL_TIMEOUT,
        )
        .await
    }
}

impl BridgeRuntime {
    pub(crate) fn writable(paths: OpenSubsonicPaths, sink: Option<OpenSubsonicBridgeSink>) -> Self {
        Self {
            paths: Some(paths),
            sink,
            retry_cursors: Mutex::new(RetryCursors::default()),
        }
    }

    pub(crate) fn read_only() -> Self {
        Self {
            paths: None,
            sink: None,
            retry_cursors: Mutex::new(RetryCursors::default()),
        }
    }

    pub(crate) fn is_writable(&self) -> bool {
        self.paths.is_some()
    }

    pub(crate) fn history_worker(&self) -> Option<HistoryWorker> {
        self.paths
            .as_ref()
            .cloned()
            .map(|paths| HistoryWorker { paths })
    }

    /// Rebase a dormant candidate on the latest coherent owner snapshot immediately before the
    /// owner publishes it. The previous active actor may have advanced bridge state while this
    /// candidate probed the server; carrying that stale revision into activation would otherwise
    /// make its first durable bridge mutation fail. Ephemeral now-playing work never crosses an
    /// activation boundary, while exact submissions remain durable.
    pub(crate) fn refresh_snapshot_for_activation(
        &self,
        store_set: &mut OpenSubsonicStoreSet,
    ) -> Result<(), ServiceError> {
        let Some(paths) = &self.paths else {
            return Ok(());
        };
        let latest = load_store_set(paths)?.ok_or(ServiceError::InvalidSetup)?;
        if latest.profile.backend_id() != store_set.profile.backend_id()
            || latest.profile.account_scope_id() != store_set.profile.account_scope_id()
        {
            return Err(ServiceError::Store(StoreError::RevisionConflict));
        }
        *store_set = latest;
        let before = store_set.bridge_state.clone();
        if store_set.bridge_state.discard_stale_now_playing() > 0 {
            self.persist_or_restore(store_set, before)?;
        }
        Ok(())
    }

    pub(crate) fn set_native_history_health(
        &self,
        store_set: &mut OpenSubsonicStoreSet,
        health: NativeHistoryHealth,
    ) -> Result<(), ServiceError> {
        if !self.is_writable() || store_set.bridge_state.native_history_health() == health {
            return Ok(());
        }
        let before = store_set.bridge_state.clone();
        store_set.bridge_state.set_native_history_health(health);
        self.persist_or_restore(store_set, before)?;
        Ok(())
    }

    /// Merge a completed read-only fetch into the current owner snapshot.
    ///
    /// Identity and cursor checks are intentionally independent of the store revision: unrelated
    /// rating/scrobble work is expected to commit while the network job is running and must not be
    /// overwritten. Native rows are idempotent by row ID; a changed cursor rejects only that stale
    /// native batch, while still allowing current-account aggregate observations to merge.
    pub(crate) fn apply_history_refresh(
        &self,
        store_set: &mut OpenSubsonicStoreSet,
        result: HistoryRefreshResult,
    ) -> Result<HistoryApplyOutcome, ServiceError> {
        let HistoryRefreshResult {
            backend_id,
            account_scope_id,
            base_cursor,
            native,
            standard,
        } = result;
        if &backend_id != store_set.profile.backend_id()
            || &account_scope_id != store_set.profile.account_scope_id()
        {
            return Ok(HistoryApplyOutcome {
                native_error: None,
                standard_error: None,
                native_stale: true,
                native_truncated: false,
            });
        }

        let mut native_error = native.as_ref().err().copied();
        let standard_error = standard.as_ref().err().copied();
        let mut native_stale = false;
        let mut native_truncated = false;
        if let Ok(Some(batch)) = native {
            native_truncated = batch.truncated;
            if batch.metadata_retry_pending {
                native_error = Some(NativeHistoryError::TemporarilyUnavailable);
            }
            if native_cursor(store_set) == base_cursor.as_ref() {
                self.apply_native_history_batch(store_set, batch)?;
            } else {
                native_stale = true;
            }
        }
        if let Ok(songs) = standard {
            self.observe_songs(store_set, &songs)?;
        }
        Ok(HistoryApplyOutcome {
            native_error,
            standard_error,
            native_stale,
            native_truncated,
        })
    }

    pub(crate) fn observe_page(
        &self,
        store_set: &mut OpenSubsonicStoreSet,
        page: &ServerLibraryPage,
    ) -> Result<(), ServiceError> {
        let songs = page
            .rows
            .iter()
            .filter_map(|row| match row {
                ServerLibraryRow::Song(song) => Some(song.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        self.observe_songs(store_set, &songs)
    }

    pub(crate) fn observe_detail(
        &self,
        store_set: &mut OpenSubsonicStoreSet,
        detail: &ServerLibraryDetail,
    ) -> Result<(), ServiceError> {
        let songs = match detail {
            ServerLibraryDetail::AlbumSongs { songs, .. } => songs.as_slice(),
            ServerLibraryDetail::PlaylistEntries(playlist) => playlist.entries.as_slice(),
            ServerLibraryDetail::ArtistAlbums { .. } => &[],
        };
        self.observe_songs(store_set, songs)
    }

    pub(crate) fn observe_songs(
        &self,
        store_set: &mut OpenSubsonicStoreSet,
        songs: &[ServerSong],
    ) -> Result<(), ServiceError> {
        if !self.is_writable() || songs.is_empty() {
            return Ok(());
        }
        let before = store_set.bridge_state.clone();
        let observed_at_unix = crate::signals::unix_now();
        let nonce = store_set.bridge_state.revision().saturating_add(1);
        let result = songs.iter().enumerate().try_for_each(|(index, song)| {
            if song.item.backend_id() != store_set.profile.backend_id()
                || song.item.account_scope_id() != store_set.profile.account_scope_id()
            {
                return Err(BridgeMutationError::InvalidEntry);
            }
            observe_rating(
                store_set,
                song,
                observed_at_unix,
                nonce.saturating_add(index as u64),
            )?;
            observe_aggregate(store_set, song, observed_at_unix)
        });
        if let Err(error) = result {
            store_set.bridge_state = before;
            return Err(mutation_error(error));
        }
        self.persist_or_restore(store_set, before)?;
        self.emit_pending(store_set);
        Ok(())
    }

    pub(crate) fn reconcile_ratings(
        &self,
        store_set: &mut OpenSubsonicStoreSet,
        winners: Vec<OpenSubsonicRatingWinner>,
    ) -> Result<(), ServiceError> {
        if !self.is_writable() {
            return Ok(());
        }
        let before = store_set.bridge_state.clone();
        let queued_at_unix = crate::signals::unix_now();
        for winner in winners {
            let Some(item_id) = matching_item_id(store_set, &winner.track) else {
                continue;
            };
            if matches!(
                &winner.origin,
                OperationOrigin::OpenSubsonic { backend_id }
                    if backend_id == store_set.profile.backend_id().as_str()
            ) {
                // The canonical ledger has already accepted this server observation as the
                // effective winner. Any older local projection for the same exact item is now
                // superseded and must not keep rewriting the server behind the owner's back.
                store_set.bridge_state.remove_rating_projection(&item_id);
                continue;
            }
            if store_set
                .bridge_state
                .rating_shadow(&item_id)
                .and_then(|shadow| shadow.confirmed_operation_id.as_deref())
                == Some(winner.operation_id.as_str())
            {
                continue;
            }
            if store_set
                .bridge_state
                .pending_rating_projections()
                .get(&item_id)
                .is_some_and(|pending| {
                    pending.operation_id == winner.operation_id && pending.target == winner.rating
                })
            {
                continue;
            }
            if let Err(error) = store_set.bridge_state.queue_rating_projection(
                item_id,
                PendingRatingProjection {
                    operation_id: winner.operation_id,
                    target: winner.rating,
                    stage: PendingRatingProjectionStage::SetRating,
                    last_readback: None,
                    queued_at_unix,
                },
            ) {
                store_set.bridge_state = before;
                return Err(mutation_error(error));
            }
        }
        self.persist_or_restore(store_set, before)?;
        Ok(())
    }

    /// Persist one outbound playback report before the caller releases its in-memory ownership.
    ///
    /// Network delivery deliberately stays on [`Self::retry_network`]. A successful return is a
    /// local durability receipt and never waits for an unavailable server during owner shutdown.
    pub(crate) fn queue_scrobble(
        &self,
        store_set: &mut OpenSubsonicStoreSet,
        owner_event_id: &str,
        kind: OpenSubsonicScrobbleKind,
        track: crate::scrobble::service::ScrobbleTrack,
    ) -> Result<(), ServiceError> {
        if !self.is_writable() {
            return Err(ServiceError::InvalidSetup);
        }
        let item = track.open_subsonic_item.ok_or(ServiceError::InvalidSetup)?;
        if item.backend_id() != store_set.profile.backend_id()
            || item.account_scope_id() != store_set.profile.account_scope_id()
        {
            return Err(ServiceError::Server(super::ServerError::WrongAccountScope));
        }
        let before = store_set.bridge_state.clone();
        let baseline = store_set
            .bridge_state
            .aggregate_play_shadows()
            .get(item.item_id())
            .cloned();
        store_set
            .bridge_state
            .queue_outbound_scrobble(PendingOutboundScrobble {
                event_id: scrobble_event_id(&item, owner_event_id),
                item_id: item.item_id().clone(),
                played_at_unix: track.started_unix,
                kind: match kind {
                    OpenSubsonicScrobbleKind::NowPlaying => OutboundScrobbleKind::NowPlaying,
                    OpenSubsonicScrobbleKind::Submission => OutboundScrobbleKind::Submission,
                },
                delivery: OutboundScrobbleDelivery::Queued,
                baseline_captured: baseline.is_some(),
                baseline_play_count: baseline.as_ref().map(|shadow| shadow.play_count),
                baseline_played_at: baseline.and_then(|shadow| shadow.played_at),
                exact_credit_recorded: false,
                exact_credit_epoch: None,
                uncertain_readbacks: 0,
                source_marker_acknowledged: kind == OpenSubsonicScrobbleKind::NowPlaying,
            })?;
        self.persist_or_restore(store_set, before)?;
        Ok(())
    }

    /// Persist proof that the source journal entered its non-submitting acknowledgement state.
    pub(crate) fn acknowledge_scrobble_source(
        &self,
        store_set: &mut OpenSubsonicStoreSet,
        owner_event_id: &str,
        track: crate::scrobble::service::ScrobbleTrack,
    ) -> Result<(), ServiceError> {
        if !self.is_writable() {
            return Err(ServiceError::InvalidSetup);
        }
        let item = track.open_subsonic_item.ok_or(ServiceError::InvalidSetup)?;
        if item.backend_id() != store_set.profile.backend_id()
            || item.account_scope_id() != store_set.profile.account_scope_id()
        {
            return Err(ServiceError::Server(super::ServerError::WrongAccountScope));
        }
        let before = store_set.bridge_state.clone();
        store_set
            .bridge_state
            .acknowledge_outbound_source(&scrobble_event_id(&item, owner_event_id))?;
        self.persist_or_restore(store_set, before)?;
        Ok(())
    }

    pub(crate) fn acknowledge_import(
        &self,
        store_set: &mut OpenSubsonicStoreSet,
        operation_id: &str,
    ) -> Result<(), ServiceError> {
        if !self.is_writable() {
            return Ok(());
        }
        let before = store_set.bridge_state.clone();
        store_set.bridge_state.remove_rating_import(operation_id);
        store_set
            .bridge_state
            .remove_engagement_import(operation_id);
        store_set.bridge_state.remove_playlist_import(operation_id);
        if let Err(error) = store_set
            .bridge_state
            .materialize_pending_aggregate_ranges()
        {
            store_set.bridge_state = before;
            return Err(mutation_error(error));
        }
        self.persist_or_restore(store_set, before)?;
        self.emit_pending(store_set);
        Ok(())
    }

    pub(crate) async fn retry_network(
        &self,
        store_set: &mut OpenSubsonicStoreSet,
        client: &OpenSubsonicClient,
    ) -> Result<(), ServiceError> {
        if !self.is_writable() {
            return Ok(());
        }
        self.emit_pending(store_set);
        match self.next_retry_lane(store_set) {
            Some(RetryLane::Playlist) => {
                self.flush_one_playlist_projection(store_set, client).await
            }
            Some(RetryLane::Rating) => self.flush_rating_projections(store_set, client).await,
            Some(RetryLane::Outbound) => self.flush_outbound_scrobbles(store_set, client).await,
            None => Ok(()),
        }
    }

    fn apply_native_history_batch(
        &self,
        store_set: &mut OpenSubsonicStoreSet,
        batch: NativeHistoryBatch,
    ) -> Result<(), ServiceError> {
        let before = store_set.bridge_state.clone();
        let mutation = (|| {
            let mut exact_rows_by_item = BTreeMap::<ItemId, u64>::new();
            let mut baseline_covered_rows = BTreeMap::<ItemId, u64>::new();
            for row in batch.rows.iter().rev() {
                let count = exact_rows_by_item.entry(row.item_id.clone()).or_default();
                *count = count.saturating_add(1);
                let aggregate_shadow = store_set
                    .bridge_state
                    .aggregate_play_shadows()
                    .get(&row.item_id);
                let counter_epoch = aggregate_shadow.map_or(0, |shadow| shadow.counter_epoch);
                let covered_by_previous_baseline = aggregate_shadow
                    .and_then(|shadow| shadow.played_at.as_deref())
                    .and_then(parse_rfc3339_unix)
                    .is_some_and(|covered_through| row.observed_at_unix <= covered_through);
                if let Some(pending) = store_set
                    .bridge_state
                    .outbound_scrobbles()
                    .iter()
                    .find(|pending| {
                        matches!(
                            pending.delivery,
                            OutboundScrobbleDelivery::Uncertain
                                | OutboundScrobbleDelivery::NeedsAttention
                        ) && pending.kind == OutboundScrobbleKind::Submission
                            && pending.item_id == row.item_id
                            && pending.played_at_unix == row.observed_at_unix
                    })
                    .cloned()
                {
                    if !pending.exact_credit_recorded {
                        store_set
                            .bridge_state
                            .reserve_outbound_exact_history_credit(
                                row.item_id.clone(),
                                counter_epoch,
                            )?;
                    }
                    if covered_by_previous_baseline {
                        let covered = baseline_covered_rows
                            .entry(row.item_id.clone())
                            .or_default();
                        *covered = covered.saturating_add(1);
                    }
                    store_set
                        .bridge_state
                        .complete_outbound_scrobble(&pending.event_id)?;
                    continue;
                }
                if store_set
                    .bridge_state
                    .consume_outbound_echo(&row.item_id, row.observed_at_unix)
                    .is_some()
                {
                    continue;
                }
                let should_import = store_set
                    .bridge_state
                    .record_exact_history_evidence(row.item_id.clone(), counter_epoch)?;
                if !should_import {
                    continue;
                }
                if covered_by_previous_baseline {
                    let covered = baseline_covered_rows
                        .entry(row.item_id.clone())
                        .or_default();
                    *covered = covered.saturating_add(1);
                }
                let operation_id = native_event_id(store_set, row.row_id);
                store_set.bridge_state.queue_engagement_import(
                    operation_id,
                    PendingEngagementImport {
                        track: row.track.clone(),
                        engagement: EngagementKind::Play,
                        played_duration_ms: None,
                        total_duration_ms: row
                            .track
                            .duration_secs
                            .map(|seconds| u64::from(seconds).saturating_mul(1_000)),
                        artist_key: crate::signals::normalize_artist(&row.track.artist),
                        observed_at_unix: row.observed_at_unix,
                    },
                )?;
            }
            // Rows at or before the previous server `played` watermark were already represented by
            // that aggregate baseline. Retaining their exact credits would make a later, genuinely
            // new playCount increment disappear. A row newer than the watermark deliberately keeps
            // its credit until the count catches up, preventing an aggregate echo from duplicating
            // the exact event.
            for (item_id, covered) in &baseline_covered_rows {
                let counter_epoch = store_set
                    .bridge_state
                    .aggregate_play_shadows()
                    .get(item_id)
                    .map_or(0, |shadow| shadow.counter_epoch);
                store_set.bridge_state.reconcile_native_aggregate_baseline(
                    item_id.clone(),
                    counter_epoch,
                    *covered,
                )?;
            }
            let aggregate_observed_at = crate::signals::unix_now();
            for (item_id, (play_count, played_at)) in &batch.aggregate_baselines {
                if store_set
                    .bridge_state
                    .has_unresolved_outbound_submission(item_id)
                {
                    // Exact rows above may resolve an ambiguous submission in this transaction.
                    // If any remain, preserve the old raw shadow so a later refresh can reconsider
                    // every aggregate increment without consuming speculative exact evidence.
                    continue;
                }
                let previous = store_set
                    .bridge_state
                    .aggregate_play_shadows()
                    .get(item_id)
                    .cloned();
                let counter_epoch = next_counter_epoch(previous.as_ref(), *play_count);
                let exact_rows = exact_rows_by_item.get(item_id).copied().unwrap_or(0);
                let already_covered = baseline_covered_rows.get(item_id).copied().unwrap_or(0);
                let newly_exact = exact_rows.saturating_sub(already_covered);
                let covered_delta = previous.as_ref().map_or_else(
                    || newly_exact.min(*play_count),
                    |shadow| {
                        if counter_epoch == shadow.counter_epoch {
                            newly_exact.min(play_count.saturating_sub(shadow.play_count))
                        } else {
                            newly_exact.min(*play_count)
                        }
                    },
                );
                store_set.bridge_state.reconcile_native_aggregate_baseline(
                    item_id.clone(),
                    counter_epoch,
                    covered_delta,
                )?;
                store_set.bridge_state.upsert_aggregate_play_shadow(
                    item_id.clone(),
                    AggregatePlayShadow {
                        play_count: *play_count,
                        played_at: played_at.clone(),
                        observed_at_unix: aggregate_observed_at,
                        counter_epoch,
                    },
                )?;
            }
            store_set
                .bridge_state
                .materialize_pending_aggregate_ranges()?;
            if let Some(cursor) = batch.next_cursor {
                store_set
                    .bridge_state
                    .set_history_cursor(NATIVE_CURSOR.to_owned(), cursor)?;
            }
            Ok::<(), BridgeMutationError>(())
        })();
        if let Err(error) = mutation {
            store_set.bridge_state = before;
            return Err(mutation_error(error));
        }
        self.persist_or_restore(store_set, before)?;
        self.emit_pending(store_set);
        Ok(())
    }

    fn next_retry_lane(&self, store_set: &OpenSubsonicStoreSet) -> Option<RetryLane> {
        let has_rating = !store_set
            .bridge_state
            .pending_rating_projections()
            .is_empty();
        let has_outbound = store_set
            .bridge_state
            .outbound_scrobbles()
            .iter()
            .any(|pending| pending.delivery != OutboundScrobbleDelivery::NeedsAttention);
        // Linked playlists are also periodic read work: a mobile/server edit must reach the
        // local ledger even when YuTuTui has no pending write.
        let has_playlist =
            store_set
                .bridge_state
                .playlist_links()
                .iter()
                .any(|(playlist_id, link)| {
                    let pending = store_set
                        .bridge_state
                        .pending_playlist_projections()
                        .get(playlist_id);
                    let in_flight = pending.is_some_and(|pending| {
                        matches!(
                            pending.stage,
                            PendingPlaylistProjectionStage::Ambiguous
                                | PendingPlaylistProjectionStage::Readback
                        )
                    });
                    link.state == super::bridge_store::PlaylistLinkState::Linked
                        && (!link.content_needs_attention || in_flight)
                        && store_set
                            .bridge_state
                            .pending_playlist_import(playlist_id)
                            .is_none()
                        && pending.is_none_or(|pending| {
                            pending.stage != PendingPlaylistProjectionStage::NeedsAttention
                        })
                });
        let mut cursors = self
            .retry_cursors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let order = match cursors.lane_after {
            Some(RetryLane::Playlist) => {
                [RetryLane::Rating, RetryLane::Outbound, RetryLane::Playlist]
            }
            Some(RetryLane::Rating) => {
                [RetryLane::Outbound, RetryLane::Playlist, RetryLane::Rating]
            }
            Some(RetryLane::Outbound) | None => {
                [RetryLane::Playlist, RetryLane::Rating, RetryLane::Outbound]
            }
        };
        let selected = order.into_iter().find(|lane| match lane {
            RetryLane::Playlist => has_playlist,
            RetryLane::Rating => has_rating,
            RetryLane::Outbound => has_outbound,
        });
        if let Some(lane) = selected {
            cursors.lane_after = Some(lane);
        }
        selected
    }

    fn next_rating_projection(
        &self,
        store_set: &OpenSubsonicStoreSet,
    ) -> Option<(ItemId, PendingRatingProjection)> {
        let pending = store_set.bridge_state.pending_rating_projections();
        let mut cursors = self
            .retry_cursors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let selected = cursors
            .rating_after
            .as_ref()
            .and_then(|last| pending.range((Excluded(last.clone()), Unbounded)).next())
            .or_else(|| pending.iter().next())
            .map(|(item_id, projection)| (item_id.clone(), projection.clone()));
        if let Some((item_id, _)) = &selected {
            cursors.rating_after = Some(item_id.clone());
        }
        selected
    }

    fn next_outbound_scrobble(
        &self,
        store_set: &OpenSubsonicStoreSet,
    ) -> Option<PendingOutboundScrobble> {
        let pending = store_set.bridge_state.outbound_scrobbles();
        let mut cursors = self
            .retry_cursors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let after = cursors.outbound_after.as_deref();
        let selected = pending
            .iter()
            .filter(|entry| entry.delivery != OutboundScrobbleDelivery::NeedsAttention)
            .filter(|entry| after.is_none_or(|last| entry.event_id.as_str() > last))
            .min_by(|left, right| left.event_id.cmp(&right.event_id))
            .or_else(|| {
                pending
                    .iter()
                    .filter(|entry| entry.delivery != OutboundScrobbleDelivery::NeedsAttention)
                    .min_by(|left, right| left.event_id.cmp(&right.event_id))
            })
            .cloned();
        if let Some(entry) = &selected {
            cursors.outbound_after = Some(entry.event_id.clone());
        }
        selected
    }

    async fn flush_rating_projections(
        &self,
        store_set: &mut OpenSubsonicStoreSet,
        client: &OpenSubsonicClient,
    ) -> Result<(), ServiceError> {
        for _ in 0..MAX_NETWORK_FLUSH_PER_TURN {
            let Some((item_id, pending)) = self.next_rating_projection(store_set) else {
                break;
            };
            let item = scoped_item(store_set, item_id.clone());
            let target = canonical_server_rating(pending.target);
            match pending.stage {
                PendingRatingProjectionStage::SetRating => {
                    let rating = target
                        .user_rating
                        .and_then(|value| u8::try_from(value).ok())
                        .ok_or(ServiceError::InvalidSetup)?;
                    catalog(store_set, client).set_rating(&item, rating).await?;
                    self.update_projection_stage(
                        store_set,
                        item_id,
                        PendingRatingProjectionStage::SetStarred,
                        None,
                    )?;
                }
                PendingRatingProjectionStage::SetStarred => {
                    if target.starred {
                        catalog(store_set, client).star(&item).await?;
                    } else {
                        catalog(store_set, client).unstar(&item).await?;
                    }
                    self.update_projection_stage(
                        store_set,
                        item_id,
                        PendingRatingProjectionStage::Readback,
                        None,
                    )?;
                }
                PendingRatingProjectionStage::Readback => {
                    let song = catalog(store_set, client).get_song(&item).await?;
                    let readback = raw_rating(&song);
                    if readback != target {
                        if pending.last_readback == Some(readback) {
                            self.accept_stable_rating_mismatch(
                                store_set, item_id, pending, &song, readback,
                            )?;
                        } else {
                            // A first (or changing) mismatch can be a delayed read-after-write.
                            // Persist it and re-read once without issuing another mutation. Only a
                            // stable repeat becomes an external causal observation.
                            self.update_projection_stage(
                                store_set,
                                item_id,
                                PendingRatingProjectionStage::Readback,
                                Some(readback),
                            )?;
                        }
                        break;
                    }
                    let before = store_set.bridge_state.clone();
                    store_set
                        .bridge_state
                        .remove_rating_projection(item.item_id());
                    store_set.bridge_state.upsert_rating_shadow(
                        item.item_id().clone(),
                        RatingShadow {
                            raw: readback,
                            observed_at_unix: crate::signals::unix_now(),
                            confirmed_operation_id: Some(pending.operation_id),
                        },
                    )?;
                    self.persist_or_restore(store_set, before)?;
                }
            }
        }
        Ok(())
    }

    fn update_projection_stage(
        &self,
        store_set: &mut OpenSubsonicStoreSet,
        item_id: ItemId,
        stage: PendingRatingProjectionStage,
        last_readback: Option<RawServerRating>,
    ) -> Result<(), ServiceError> {
        let before = store_set.bridge_state.clone();
        let mut pending = store_set
            .bridge_state
            .pending_rating_projections()
            .get(&item_id)
            .cloned()
            .ok_or(ServiceError::InvalidSetup)?;
        pending.stage = stage;
        pending.last_readback = last_readback;
        store_set
            .bridge_state
            .queue_rating_projection(item_id, pending)?;
        self.persist_or_restore(store_set, before)?;
        Ok(())
    }

    fn accept_stable_rating_mismatch(
        &self,
        store_set: &mut OpenSubsonicStoreSet,
        item_id: ItemId,
        pending: PendingRatingProjection,
        song: &ServerSong,
        readback: RawServerRating,
    ) -> Result<(), ServiceError> {
        let before = store_set.bridge_state.clone();
        let observed_at_unix = crate::signals::unix_now();
        let operation_id =
            rating_projection_mismatch_id(store_set, &item_id, &pending.operation_id, readback);
        let mutation = (|| {
            store_set.bridge_state.queue_rating_import(
                operation_id,
                PendingRatingImport {
                    item_id: item_id.clone(),
                    track: portable_server_track(song),
                    raw: readback,
                    mapped: map_server_rating(readback),
                    observed_at_unix,
                },
            )?;
            store_set.bridge_state.upsert_rating_shadow(
                item_id.clone(),
                RatingShadow {
                    raw: readback,
                    observed_at_unix,
                    confirmed_operation_id: None,
                },
            )?;
            store_set.bridge_state.remove_rating_projection(&item_id);
            Ok::<(), BridgeMutationError>(())
        })();
        if let Err(error) = mutation {
            store_set.bridge_state = before;
            return Err(mutation_error(error));
        }
        self.persist_or_restore(store_set, before)?;
        self.emit_pending(store_set);
        Ok(())
    }

    async fn flush_outbound_scrobbles(
        &self,
        store_set: &mut OpenSubsonicStoreSet,
        client: &OpenSubsonicClient,
    ) -> Result<(), ServiceError> {
        for _ in 0..MAX_NETWORK_FLUSH_PER_TURN {
            let Some(mut pending) = self.next_outbound_scrobble(store_set) else {
                break;
            };
            if pending.delivery == OutboundScrobbleDelivery::Uncertain {
                if pending.kind == OutboundScrobbleKind::NowPlaying {
                    self.complete_outbound(store_set, &pending, false)?;
                    continue;
                }
                if !pending.exact_credit_recorded {
                    let before = store_set.bridge_state.clone();
                    let mut credited = pending.clone();
                    record_pending_exact_credit(store_set, &mut credited)?;
                    store_set.bridge_state.replace_outbound_scrobble(credited)?;
                    self.persist_or_restore(store_set, before)?;
                    continue;
                }
                let item = scoped_item(store_set, pending.item_id.clone());
                let song = catalog(store_set, client).get_song(&item).await?;
                let confirmed = outbound_readback_confirms(&pending, &song);
                let before = store_set.bridge_state.clone();
                let mutation = (|| {
                    if confirmed {
                        store_set
                            .bridge_state
                            .complete_outbound_scrobble(&pending.event_id)?;
                        store_set
                            .bridge_state
                            .record_outbound_echo(outbound_echo(&pending))?;
                        observe_aggregate(store_set, &song, crate::signals::unix_now())?;
                    } else {
                        observe_aggregate(store_set, &song, crate::signals::unix_now())?;
                        pending.uncertain_readbacks = pending.uncertain_readbacks.saturating_add(1);
                        if pending.uncertain_readbacks >= MAX_UNCERTAIN_SCROBBLE_READBACKS {
                            pending.uncertain_readbacks = MAX_UNCERTAIN_SCROBBLE_READBACKS;
                            pending.delivery = OutboundScrobbleDelivery::NeedsAttention;
                            tracing::warn!(
                                "music server playback report needs an explicit retry or sent decision"
                            );
                        }
                        store_set.bridge_state.replace_outbound_scrobble(pending)?;
                    }
                    Ok::<(), BridgeMutationError>(())
                })();
                if let Err(error) = mutation {
                    store_set.bridge_state = before;
                    return Err(mutation_error(error));
                }
                self.persist_or_restore(store_set, before)?;
                self.emit_pending(store_set);
                continue;
            }

            let item = scoped_item(store_set, pending.item_id.clone());
            let submission = pending.kind == OutboundScrobbleKind::Submission;
            if submission && !pending.baseline_captured {
                let song = catalog(store_set, client).get_song(&item).await?;
                let before = store_set.bridge_state.clone();
                if let Err(error) = observe_aggregate(store_set, &song, crate::signals::unix_now())
                {
                    store_set.bridge_state = before;
                    return Err(mutation_error(error));
                }
                pending.baseline_captured = true;
                pending.baseline_play_count = song.play_count;
                pending.baseline_played_at = song.played_at;
                store_set.bridge_state.replace_outbound_scrobble(pending)?;
                self.persist_or_restore(store_set, before)?;
                self.emit_pending(store_set);
                continue;
            }

            let queued_before_attempt = store_set.bridge_state.clone();
            pending.delivery = OutboundScrobbleDelivery::Uncertain;
            pending.uncertain_readbacks = 0;
            if submission {
                record_pending_exact_credit(store_set, &mut pending)?;
            }
            store_set
                .bridge_state
                .replace_outbound_scrobble(pending.clone())?;
            self.persist_or_restore(store_set, queued_before_attempt.clone())?;

            let time = u64::try_from(pending.played_at_unix)
                .ok()
                .and_then(|seconds| seconds.checked_mul(1_000));
            let delivery = catalog(store_set, client)
                .scrobble(&item, submission, time)
                .await;
            match delivery {
                Ok(()) => self.complete_outbound(store_set, &pending, submission)?,
                Err(MutationDeliveryError::DefinitelyNotApplied(error)) => {
                    let uncertain = store_set.bridge_state.clone();
                    let mut queued = queued_before_attempt;
                    queued.set_revision(uncertain.revision());
                    store_set.bridge_state = queued;
                    self.persist_or_restore(store_set, uncertain)?;
                    return Err(error.into());
                }
                Err(MutationDeliveryError::Ambiguous(error)) => return Err(error.into()),
            }
        }
        Ok(())
    }

    fn complete_outbound(
        &self,
        store_set: &mut OpenSubsonicStoreSet,
        pending: &PendingOutboundScrobble,
        retain_echo: bool,
    ) -> Result<(), ServiceError> {
        let before = store_set.bridge_state.clone();
        let mutation = (|| {
            store_set
                .bridge_state
                .complete_outbound_scrobble(&pending.event_id)?;
            if retain_echo {
                store_set
                    .bridge_state
                    .record_outbound_echo(outbound_echo(pending))?;
            }
            Ok::<(), BridgeMutationError>(())
        })();
        if let Err(error) = mutation {
            store_set.bridge_state = before;
            return Err(mutation_error(error));
        }
        self.persist_or_restore(store_set, before)?;
        Ok(())
    }

    fn persist_or_restore(
        &self,
        store_set: &mut OpenSubsonicStoreSet,
        before: super::OpenSubsonicBridgeState,
    ) -> Result<bool, ServiceError> {
        if before == store_set.bridge_state {
            return Ok(false);
        }
        let Some(paths) = self.paths.as_ref() else {
            store_set.bridge_state = before;
            return Err(ServiceError::InvalidSetup);
        };
        let expected = store_set.revisions();
        let desired = store_set.bridge_state.clone();
        let Err(error) = commit_store_set(paths, expected, store_set) else {
            return Ok(true);
        };

        // A store-set install may report an error after its commit marker became durable. Reloading
        // both rolls that transaction forward and distinguishes "committed, response lost" from a
        // genuine rejected write. Revision conflicts also rebase this actor onto the current owner
        // snapshot so a later replay can make progress instead of repeatedly presenting stale
        // revisions.
        if let Ok(Some(recovered)) = load_store_set(paths)
            && recovered.profile.backend_id() == store_set.profile.backend_id()
            && recovered.profile.account_scope_id() == store_set.profile.account_scope_id()
        {
            let mut desired_at_recovered_revision = desired;
            desired_at_recovered_revision.set_revision(recovered.bridge_state.revision());
            let desired_is_durable = recovered.bridge_state == desired_at_recovered_revision;
            *store_set = recovered;
            return if desired_is_durable {
                Ok(true)
            } else {
                Err(error.into())
            };
        }

        store_set.bridge_state = before;
        Err(error.into())
    }
}

async fn fetch_history_sources(
    store_set: &OpenSubsonicStoreSet,
    client: &OpenSubsonicClient,
    native_budget: Duration,
    standard_budget: Duration,
) -> Result<HistoryRefreshResult, ServiceError> {
    let base_cursor = native_cursor(store_set).cloned();
    let native = tokio::time::timeout(native_budget, fetch_native_history_batch(store_set, client));
    let standard = tokio::time::timeout(standard_budget, fetch_standard_history(store_set, client));
    let (native, standard) = tokio::join!(native, standard);
    Ok(HistoryRefreshResult {
        backend_id: store_set.profile.backend_id().clone(),
        account_scope_id: store_set.profile.account_scope_id().clone(),
        base_cursor,
        native: native.unwrap_or(Err(NativeHistoryError::TemporarilyUnavailable)),
        standard: standard.unwrap_or(Err(ServiceError::Server(
            super::ServerError::TemporarilyUnavailable,
        ))),
    })
}

async fn fetch_native_history_batch(
    store_set: &OpenSubsonicStoreSet,
    client: &OpenSubsonicClient,
) -> Result<Option<NativeHistoryBatch>, NativeHistoryError> {
    let Some(credential) = store_set
        .private_state
        .native_history_credential()
        .map_err(|_| NativeHistoryError::InvalidCredential)?
    else {
        return Ok(None);
    };
    let cursor = native_cursor(store_set);
    if let Some(cursor) = cursor.filter(|cursor| !cursor.pending_metadata_rows.is_empty()) {
        let previous_len = cursor.pending_metadata_rows.len();
        let resolution =
            history_metadata::resolve(store_set, client, cursor.pending_metadata_rows.clone())
                .await;
        let mut next_cursor = cursor.clone();
        next_cursor.pending_metadata_rows = resolution.pending;
        if next_cursor.pending_metadata_rows.len() != previous_len {
            next_cursor.updated_at_unix = crate::signals::unix_now();
        }
        let truncated =
            !next_cursor.pending_metadata_rows.is_empty() || next_cursor.continuation.is_some();
        return Ok(Some(NativeHistoryBatch {
            rows: resolution.observations,
            aggregate_baselines: resolution.aggregate_baselines,
            next_cursor: Some(next_cursor),
            truncated,
            metadata_retry_pending: resolution.transient_failure,
        }));
    }
    let high_water = cursor
        .and_then(|cursor| cursor.high_water_id.as_deref())
        .map(|value| {
            value
                .parse::<u64>()
                .map_err(|_| NativeHistoryError::InvalidResponse)
        })
        .transpose()?;
    let continuation = cursor
        .and_then(|cursor| cursor.continuation.as_ref())
        .map(native_scan_continuation)
        .transpose()?;
    let overlap_from_unix = cursor
        .and_then(|cursor| cursor.overlap_started_at_unix)
        .and_then(|value| u64::try_from(value).ok());
    let native = NavidromeNativeClient::connect(&store_set.profile).await?;
    let mut session = native.login(&credential).await?;
    native.probe(&mut session).await?;
    let scan = native
        .scan_recent_from_window(&mut session, high_water, overlap_from_unix, continuation)
        .await?;

    let pending = scan
        .rows
        .iter()
        .map(|row| {
            Ok(PendingNativeMetadataRow {
                row_id: row.id,
                item_id: row.media_file_id.clone(),
                observed_at_unix: i64::try_from(row.submission_time_unix)
                    .map_err(|_| NativeHistoryError::InvalidResponse)?,
            })
        })
        .collect::<Result<Vec<_>, NativeHistoryError>>()?;
    let resolution = history_metadata::resolve(store_set, client, pending).await;
    let mut next_cursor = if let Some(continuation) = scan.continuation.as_ref() {
        Some(HistoryCursor {
            high_water_id: high_water.map(|id| id.to_string()),
            overlap_started_at_unix: continuation
                .through_unix
                .and_then(|value| i64::try_from(value).ok()),
            updated_at_unix: crate::signals::unix_now(),
            continuation: Some(history_continuation(continuation)?),
            pending_metadata_rows: Vec::new(),
        })
    } else {
        scan.next_high_water_id.map(|high_water_id| HistoryCursor {
            high_water_id: Some(high_water_id.to_string()),
            overlap_started_at_unix: completed_history_overlap(cursor, &scan.rows),
            updated_at_unix: crate::signals::unix_now(),
            continuation: None,
            pending_metadata_rows: Vec::new(),
        })
    };
    if !resolution.pending.is_empty() {
        next_cursor
            .as_mut()
            .ok_or(NativeHistoryError::InvalidResponse)?
            .pending_metadata_rows = resolution.pending;
    }
    let truncated = scan.truncated
        || next_cursor
            .as_ref()
            .is_some_and(|cursor| !cursor.pending_metadata_rows.is_empty());
    Ok(Some(NativeHistoryBatch {
        rows: resolution.observations,
        aggregate_baselines: resolution.aggregate_baselines,
        next_cursor,
        truncated,
        metadata_retry_pending: resolution.transient_failure,
    }))
}

async fn fetch_standard_history(
    store_set: &OpenSubsonicStoreSet,
    client: &OpenSubsonicClient,
) -> Result<Vec<ServerSong>, ServiceError> {
    let page = catalog(store_set, client)
        .library_page(
            super::model::ServerLibrarySection::RecentlyPlayed,
            0,
            STANDARD_RECENT_ALBUMS,
        )
        .await?;
    let mut songs = page
        .rows
        .iter()
        .filter_map(|row| match row {
            ServerLibraryRow::Song(song) => Some(song.clone()),
            _ => None,
        })
        .take(STANDARD_RECENT_SONGS)
        .collect::<Vec<_>>();
    let album_ids = page
        .rows
        .iter()
        .filter_map(|row| match row {
            ServerLibraryRow::Album(album) => Some(album.id.clone()),
            _ => None,
        })
        .take(STANDARD_RECENT_ALBUMS as usize)
        .collect::<Vec<_>>();
    let details =
        futures::stream::iter(album_ids.into_iter().map(|album_id| async move {
            catalog(store_set, client).album_detail(&album_id).await
        }))
        .buffered(STANDARD_ALBUM_CONCURRENCY)
        .collect::<Vec<_>>()
        .await;
    for detail in details {
        if songs.len() >= STANDARD_RECENT_SONGS {
            break;
        }
        let Ok(ServerLibraryDetail::AlbumSongs {
            songs: album_songs, ..
        }) = detail
        else {
            continue;
        };
        songs.extend(
            album_songs
                .into_iter()
                .take(STANDARD_RECENT_SONGS.saturating_sub(songs.len())),
        );
    }
    Ok(songs)
}

fn observe_rating(
    store_set: &mut OpenSubsonicStoreSet,
    song: &ServerSong,
    observed_at_unix: i64,
    nonce: u64,
) -> Result<(), BridgeMutationError> {
    let item_id = song.item.item_id();
    if store_set
        .bridge_state
        .pending_rating_projections()
        .contains_key(item_id)
    {
        return Ok(());
    }
    let raw = raw_rating(song);
    if store_set
        .bridge_state
        .rating_shadow(item_id)
        .is_some_and(|shadow| shadow.raw == raw)
    {
        return Ok(());
    }
    let operation_id = rating_observation_id(store_set, item_id, raw, nonce);
    store_set.bridge_state.queue_rating_import(
        operation_id,
        PendingRatingImport {
            item_id: item_id.clone(),
            track: portable_server_track(song),
            raw,
            mapped: map_server_rating(raw),
            observed_at_unix,
        },
    )?;
    store_set.bridge_state.upsert_rating_shadow(
        item_id.clone(),
        RatingShadow {
            raw,
            observed_at_unix,
            confirmed_operation_id: None,
        },
    )
}

fn record_pending_exact_credit(
    store_set: &mut OpenSubsonicStoreSet,
    pending: &mut PendingOutboundScrobble,
) -> Result<(), BridgeMutationError> {
    if pending.exact_credit_recorded {
        return Ok(());
    }
    let counter_epoch = store_set
        .bridge_state
        .aggregate_play_shadows()
        .get(&pending.item_id)
        .map_or(0, |shadow| shadow.counter_epoch);
    store_set
        .bridge_state
        .reserve_outbound_exact_history_credit(pending.item_id.clone(), counter_epoch)?;
    pending.exact_credit_recorded = true;
    pending.exact_credit_epoch = Some(counter_epoch);
    Ok(())
}

fn outbound_echo(pending: &PendingOutboundScrobble) -> OutboundScrobbleEcho {
    OutboundScrobbleEcho {
        event_id: pending.event_id.clone(),
        item_id: pending.item_id.clone(),
        played_at_unix: pending.played_at_unix,
    }
}

fn outbound_readback_confirms(pending: &PendingOutboundScrobble, song: &ServerSong) -> bool {
    if !pending.baseline_captured {
        return false;
    }
    song.played_at != pending.baseline_played_at
        && song.played_at.as_deref().and_then(parse_rfc3339_unix) == Some(pending.played_at_unix)
}

fn matching_item_id(store_set: &OpenSubsonicStoreSet, track: &PortableTrack) -> Option<ItemId> {
    match &track.key {
        PortableTrackKey::OpenSubsonic {
            backend_id,
            account_scope_id,
            item_id,
        } if backend_id == store_set.profile.backend_id().as_str()
            && account_scope_id == store_set.profile.account_scope_id().as_str() =>
        {
            ItemId::new(item_id.clone()).ok()
        }
        _ => None,
    }
}

fn scoped_item(store_set: &OpenSubsonicStoreSet, item_id: ItemId) -> OpenSubsonicItemRef {
    OpenSubsonicItemRef::new(
        store_set.profile.backend_id().clone(),
        store_set.profile.account_scope_id().clone(),
        item_id,
    )
}

fn catalog<'a>(
    store_set: &'a OpenSubsonicStoreSet,
    client: &'a OpenSubsonicClient,
) -> OpenSubsonicCatalog<'a> {
    OpenSubsonicCatalog::new(
        client,
        store_set.private_state.credential(),
        store_set.profile.backend_id(),
        store_set.profile.account_scope_id(),
    )
}

fn raw_rating(song: &ServerSong) -> RawServerRating {
    RawServerRating {
        user_rating: song.user_rating,
        starred: song.starred,
    }
}

fn rating_observation_id(
    store_set: &OpenSubsonicStoreSet,
    item_id: &ItemId,
    raw: RawServerRating,
    nonce: u64,
) -> String {
    let mut digest = Sha256::new();
    digest.update(b"yututui-open-subsonic-rating-observation-v1\0");
    hash_text(&mut digest, store_set.profile.backend_id().as_str());
    hash_text(&mut digest, store_set.profile.account_scope_id().as_str());
    hash_text(&mut digest, item_id.as_str());
    digest.update(nonce.to_be_bytes());
    hash_raw_rating(&mut digest, raw);
    format!("sub-rating-{}", HEXLOWER.encode(&digest.finalize()))
}

fn rating_projection_mismatch_id(
    store_set: &OpenSubsonicStoreSet,
    item_id: &ItemId,
    projected_operation_id: &str,
    raw: RawServerRating,
) -> String {
    let mut digest = Sha256::new();
    digest.update(b"yututui-open-subsonic-rating-projection-mismatch-v1\0");
    hash_text(&mut digest, store_set.profile.backend_id().as_str());
    hash_text(&mut digest, store_set.profile.account_scope_id().as_str());
    hash_text(&mut digest, item_id.as_str());
    hash_text(&mut digest, projected_operation_id);
    hash_raw_rating(&mut digest, raw);
    format!(
        "sub-rating-mismatch-{}",
        HEXLOWER.encode(&digest.finalize())
    )
}

fn hash_raw_rating(digest: &mut Sha256, raw: RawServerRating) {
    match raw.user_rating {
        Some(value) => {
            digest.update([1]);
            digest.update(value.to_be_bytes());
        }
        None => digest.update([0]),
    }
    digest.update([u8::from(raw.starred)]);
}

fn native_event_id(store_set: &OpenSubsonicStoreSet, row_id: u64) -> String {
    let mut digest = Sha256::new();
    digest.update(b"yututui-open-subsonic-native-history-v1\0");
    hash_text(&mut digest, store_set.profile.backend_id().as_str());
    hash_text(&mut digest, store_set.profile.account_scope_id().as_str());
    digest.update(row_id.to_be_bytes());
    format!("sub-native-{}", HEXLOWER.encode(&digest.finalize()))
}

fn scrobble_event_id(item: &OpenSubsonicItemRef, owner_event_id: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"yututui-open-subsonic-outbound-scrobble-v2\0");
    hash_text(&mut digest, item.backend_id().as_str());
    hash_text(&mut digest, item.account_scope_id().as_str());
    hash_text(&mut digest, owner_event_id);
    format!("sub-scrobble-{}", HEXLOWER.encode(&digest.finalize()))
}

fn hash_text(digest: &mut Sha256, value: &str) {
    digest.update((value.len() as u64).to_be_bytes());
    digest.update(value.as_bytes());
}

fn mutation_error(_error: BridgeMutationError) -> ServiceError {
    ServiceError::Store(StoreError::InvalidState)
}

impl From<BridgeMutationError> for ServiceError {
    fn from(error: BridgeMutationError) -> Self {
        mutation_error(error)
    }
}
