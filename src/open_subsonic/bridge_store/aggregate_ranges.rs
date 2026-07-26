//! Compact, durable continuation for standard aggregate play-count growth.

use serde::{Deserialize, Serialize};

use super::{
    BridgeMutationError, MAX_PENDING_AGGREGATE_RANGES, MAX_PENDING_ENGAGEMENT_IMPORTS,
    OpenSubsonicBridgeState, PendingEngagementImport,
};
use crate::open_subsonic::history::AggregatePlayRange;
use crate::open_subsonic::model::ItemId;
use crate::personal_state::{EngagementKind, PortableTrack, PortableTrackKey};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PendingAggregateRange {
    pub track: PortableTrack,
    pub artist_key: String,
    pub counter_epoch: u64,
    pub range: AggregatePlayRange,
}

impl OpenSubsonicBridgeState {
    #[cfg(test)]
    pub(crate) fn pending_aggregate_ranges(
        &self,
    ) -> &std::collections::VecDeque<PendingAggregateRange> {
        &self.pending_aggregate_ranges
    }

    pub(crate) fn queue_aggregate_range(
        &mut self,
        pending: PendingAggregateRange,
    ) -> Result<(), BridgeMutationError> {
        self.validate_pending_aggregate_range(&pending)?;
        if self
            .pending_aggregate_ranges
            .iter()
            .any(|existing| existing == &pending)
        {
            return Ok(());
        }
        if self.pending_aggregate_ranges.len() >= MAX_PENDING_AGGREGATE_RANGES {
            return Err(BridgeMutationError::CapacityExceeded);
        }
        let overlaps = self.pending_aggregate_ranges.iter().any(|existing| {
            existing.range.item == pending.range.item
                && existing.counter_epoch == pending.counter_epoch
                && existing.range.first_ordinal <= pending.range.last_ordinal
                && pending.range.first_ordinal <= existing.range.last_ordinal
        });
        if overlaps {
            return Err(BridgeMutationError::ConflictingEntry);
        }
        self.pending_aggregate_ranges.push_back(pending);
        Ok(())
    }

    /// Fill the bounded import queue without expanding the unprocessed ordinal tail.
    ///
    /// Exact-first evidence consumes a prefix arithmetically, so even an enormous suppressed range
    /// does not allocate or loop once per server play.
    pub(crate) fn materialize_pending_aggregate_ranges(
        &mut self,
    ) -> Result<(), BridgeMutationError> {
        loop {
            let available = MAX_PENDING_ENGAGEMENT_IMPORTS
                .saturating_sub(self.pending_engagement_imports.len());
            let Some((position, pending)) = self
                .pending_aggregate_ranges
                .iter()
                .enumerate()
                .find(|(_, pending)| {
                    let item_id = pending.range.item.item_id();
                    !self.has_unresolved_outbound_submission(item_id)
                        && (available > 0
                            || self.exact_history_credit_count(item_id, pending.counter_epoch) > 0)
                })
                .map(|(position, pending)| (position, pending.clone()))
            else {
                return Ok(());
            };
            let item_id = pending.range.item.item_id().clone();
            let exact = self.exact_history_credit_count(&item_id, pending.counter_epoch);
            let logical_count = pending
                .range
                .logical_len()
                .min(exact.saturating_add(available as u64));
            let import_count = self.record_aggregate_history_evidence(
                item_id,
                pending.counter_epoch,
                logical_count,
            )?;
            let suppressed = logical_count.saturating_sub(import_count);
            let first_import = pending
                .range
                .first_ordinal
                .checked_add(suppressed)
                .ok_or(BridgeMutationError::CapacityExceeded)?;
            let consumed_through = pending
                .range
                .first_ordinal
                .checked_add(logical_count.saturating_sub(1))
                .ok_or(BridgeMutationError::CapacityExceeded)?;
            for ordinal in first_import..=consumed_through {
                let play = pending
                    .range
                    .play(ordinal)
                    .ok_or(BridgeMutationError::InvalidEntry)?;
                self.queue_engagement_import(
                    play.event_id,
                    PendingEngagementImport {
                        track: pending.track.clone(),
                        engagement: EngagementKind::Play,
                        played_duration_ms: None,
                        total_duration_ms: pending
                            .track
                            .duration_secs
                            .map(|seconds| u64::from(seconds).saturating_mul(1_000)),
                        artist_key: pending.artist_key.clone(),
                        observed_at_unix: play.played_at_unix,
                    },
                )?;
            }
            if consumed_through == pending.range.last_ordinal {
                self.pending_aggregate_ranges.remove(position);
            } else {
                self.pending_aggregate_ranges
                    .get_mut(position)
                    .ok_or(BridgeMutationError::ConflictingEntry)?
                    .range
                    .first_ordinal = consumed_through
                    .checked_add(1)
                    .ok_or(BridgeMutationError::CapacityExceeded)?;
            }
        }
    }

    pub(super) fn aggregate_item_is_protected(&self, item_id: &ItemId) -> bool {
        self.history_dedupe_credits.contains_key(item_id)
            || self
                .pending_aggregate_ranges
                .iter()
                .any(|pending| pending.range.item.item_id() == item_id)
    }

    pub(super) fn validate_pending_aggregate_range(
        &self,
        pending: &PendingAggregateRange,
    ) -> Result<(), BridgeMutationError> {
        pending
            .track
            .validate()
            .map_err(|_| BridgeMutationError::InvalidEntry)?;
        super::validate_portable_text(&pending.artist_key)?;
        if pending.range.first_ordinal == 0
            || pending.range.first_ordinal > pending.range.last_ordinal
            || (!pending.range.has_server_time && !pending.range.has_server_ordinal)
            || (!pending.range.has_server_ordinal && pending.range.logical_len() != 1)
        {
            return Err(BridgeMutationError::InvalidEntry);
        }
        let PortableTrackKey::OpenSubsonic {
            backend_id,
            account_scope_id,
            item_id,
        } = &pending.track.key
        else {
            return Err(BridgeMutationError::InvalidEntry);
        };
        if backend_id != self.backend_id.as_str()
            || account_scope_id != self.account_scope_id.as_str()
            || item_id != pending.range.item.item_id().as_str()
            || pending.range.item.backend_id() != &self.backend_id
            || pending.range.item.account_scope_id() != &self.account_scope_id
        {
            return Err(BridgeMutationError::InvalidEntry);
        }
        let shadow = self
            .aggregate_play_shadows
            .get(pending.range.item.item_id())
            .ok_or(BridgeMutationError::InvalidEntry)?;
        if pending.counter_epoch > shadow.counter_epoch
            || (pending.counter_epoch == shadow.counter_epoch
                && pending.range.last_ordinal > shadow.play_count)
        {
            return Err(BridgeMutationError::InvalidEntry);
        }
        Ok(())
    }

    pub(super) fn validate_pending_aggregate_ranges(&self) -> Result<(), BridgeMutationError> {
        let mut last_by_generation = std::collections::BTreeMap::<(&ItemId, u64), u64>::new();
        for pending in &self.pending_aggregate_ranges {
            self.validate_pending_aggregate_range(pending)?;
            let key = (pending.range.item.item_id(), pending.counter_epoch);
            if last_by_generation
                .insert(key, pending.range.last_ordinal)
                .is_some_and(|last| last >= pending.range.first_ordinal)
            {
                return Err(BridgeMutationError::ConflictingEntry);
            }
        }
        Ok(())
    }
}
