//! Standard/native history bookkeeping kept out of the credential-owning actor core.

use super::NATIVE_CURSOR;
use crate::open_subsonic::bridge_event::portable_server_track;
use crate::open_subsonic::bridge_store::{
    AggregatePlayShadow, BridgeMutationError, HistoryContinuation, HistoryCursor,
    PendingAggregateRange,
};
use crate::open_subsonic::history::{
    AggregateHistoryShadow, parse_rfc3339_unix, plan_aggregate_history,
};
use crate::open_subsonic::model::ServerSong;
use crate::open_subsonic::native_history::{
    NativeHistoryError, NativeScrobbleRow, NativeScrobbleScanContinuation,
};
use crate::open_subsonic::transaction::OpenSubsonicStoreSet;

pub(super) fn native_cursor(store_set: &OpenSubsonicStoreSet) -> Option<&HistoryCursor> {
    store_set.bridge_state.history_cursors().get(NATIVE_CURSOR)
}

pub(in crate::open_subsonic) fn completed_history_overlap(
    prior: Option<&HistoryCursor>,
    rows: &[NativeScrobbleRow],
) -> Option<i64> {
    prior
        .filter(|cursor| cursor.continuation.is_some())
        .and_then(|cursor| cursor.overlap_started_at_unix)
        .or_else(|| {
            rows.last()
                .and_then(|row| i64::try_from(row.submission_time_unix).ok())
        })
}

pub(super) fn native_scan_continuation(
    continuation: &HistoryContinuation,
) -> Result<NativeScrobbleScanContinuation, NativeHistoryError> {
    Ok(NativeScrobbleScanContinuation {
        candidate_high_water_id: continuation
            .candidate_high_water_id
            .as_deref()
            .map(str::parse)
            .transpose()
            .map_err(|_| NativeHistoryError::InvalidResponse)?,
        next_start: usize::try_from(continuation.next_start)
            .map_err(|_| NativeHistoryError::InvalidResponse)?,
        through_unix: continuation.through_unix,
        reached_high_water: continuation.reached_high_water,
        overlap_row_ids: continuation.overlap_row_ids.clone(),
        backlog_complete: continuation.backlog_complete,
        head_anchor_high_water_id: continuation
            .head_anchor_high_water_id
            .as_deref()
            .map(str::parse)
            .transpose()
            .map_err(|_| NativeHistoryError::InvalidResponse)?,
        head_next_start: continuation
            .head_next_start
            .map(usize::try_from)
            .transpose()
            .map_err(|_| NativeHistoryError::InvalidResponse)?,
        head_from_unix: continuation.head_from_unix,
        head_through_unix: continuation.head_through_unix,
        head_overlap_row_ids: continuation.head_overlap_row_ids.clone(),
    })
}

pub(super) fn history_continuation(
    continuation: &NativeScrobbleScanContinuation,
) -> Result<HistoryContinuation, NativeHistoryError> {
    Ok(HistoryContinuation {
        candidate_high_water_id: continuation
            .candidate_high_water_id
            .map(|id| id.to_string()),
        next_start: u32::try_from(continuation.next_start)
            .map_err(|_| NativeHistoryError::InvalidResponse)?,
        through_unix: continuation.through_unix,
        reached_high_water: continuation.reached_high_water,
        overlap_row_ids: continuation.overlap_row_ids.clone(),
        backlog_complete: continuation.backlog_complete,
        head_anchor_high_water_id: continuation
            .head_anchor_high_water_id
            .map(|id| id.to_string()),
        head_next_start: continuation
            .head_next_start
            .map(u32::try_from)
            .transpose()
            .map_err(|_| NativeHistoryError::InvalidResponse)?,
        head_from_unix: continuation.head_from_unix,
        head_through_unix: continuation.head_through_unix,
        head_overlap_row_ids: continuation.head_overlap_row_ids.clone(),
    })
}

pub(super) fn observe_aggregate(
    store_set: &mut OpenSubsonicStoreSet,
    song: &ServerSong,
    observed_at_unix: i64,
) -> Result<(), BridgeMutationError> {
    if song.play_count.is_none() && song.played_at.is_none() {
        return Ok(());
    }
    if store_set
        .bridge_state
        .has_unresolved_outbound_submission(song.item.item_id())
    {
        // An ambiguous local submission owns speculative exact evidence. Advancing the raw
        // aggregate shadow here could consume an unrelated mobile increment and make it
        // unrecoverable. Keep the prior shadow so the same server value is retried after explicit
        // resolution or an exact native echo.
        return Ok(());
    }
    let previous_shadow = store_set
        .bridge_state
        .aggregate_play_shadows()
        .get(song.item.item_id())
        .cloned();
    let previous = previous_shadow
        .as_ref()
        .map(|shadow| AggregateHistoryShadow {
            play_count: shadow.play_count,
            played_at_unix: shadow.played_at.as_deref().and_then(parse_rfc3339_unix),
            observed_at_unix: shadow.observed_at_unix,
            counter_epoch: shadow.counter_epoch,
        });
    let plan = plan_aggregate_history(previous.as_ref(), song, observed_at_unix);
    let track = portable_server_track(song);
    let artist_key = crate::signals::normalize_artist(&track.artist);
    let played_at = song
        .played_at
        .as_ref()
        .filter(|value| parse_rfc3339_unix(value).is_some())
        .cloned()
        .or_else(|| {
            previous_shadow
                .as_ref()
                .and_then(|shadow| shadow.played_at.clone())
                .filter(|value| parse_rfc3339_unix(value).is_some())
        });
    store_set.bridge_state.upsert_aggregate_play_shadow(
        song.item.item_id().clone(),
        AggregatePlayShadow {
            play_count: plan.next_shadow.play_count,
            // Missing/malformed values create count-only evidence but never erase a stronger time.
            played_at,
            observed_at_unix,
            counter_epoch: plan.next_shadow.counter_epoch,
        },
    )?;
    if plan.baseline_only_delta > 0 {
        store_set.bridge_state.reconcile_native_aggregate_baseline(
            song.item.item_id().clone(),
            plan.next_shadow.counter_epoch,
            plan.baseline_only_delta,
        )?;
    }
    if let Some(range) = plan.range {
        store_set
            .bridge_state
            .queue_aggregate_range(PendingAggregateRange {
                track,
                artist_key,
                counter_epoch: plan.next_shadow.counter_epoch,
                range,
            })?;
        store_set
            .bridge_state
            .materialize_pending_aggregate_ranges()?;
    }
    Ok(())
}

pub(super) fn next_counter_epoch(previous: Option<&AggregatePlayShadow>, play_count: u64) -> u64 {
    previous.map_or(0, |shadow| {
        shadow
            .counter_epoch
            .saturating_add(u64::from(play_count < shadow.play_count))
    })
}
