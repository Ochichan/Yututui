//! Owner-only durable observations and work queues for server bridges.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use super::model::{AccountScopeId, BackendId, ItemId};
use super::profile::StoreError;
use super::rating::{RawServerRating, map_server_rating};
use crate::personal_state::{EngagementKind, PortableTrack, PortableTrackKey, Rating};

mod aggregate_ranges;
mod history_credits;
mod history_cursor;
mod outbound_lifecycle;

pub(crate) use aggregate_ranges::PendingAggregateRange;
pub use history_cursor::NativeHistoryHealth;
pub(crate) use history_cursor::{HistoryContinuation, HistoryCursor, PendingNativeMetadataRow};
use outbound_lifecycle::{
    completed_matches_pending, same_outbound_identity, validate_outbound_scrobble,
};

pub(crate) const BRIDGE_KIND: &str = "yututui_open_subsonic_bridge";
pub(crate) const BRIDGE_SCHEMA_VERSION: u32 = 2;
pub(crate) const MAX_BRIDGE_BYTES: u64 = 16 * 1024 * 1024;

const MAX_RATING_SHADOWS: usize = 20_000;
const MAX_PENDING_RATING_PROJECTIONS: usize = 20_000;
const MAX_PENDING_RATING_IMPORTS: usize = 20_000;
const MAX_PENDING_ENGAGEMENT_IMPORTS: usize = 20_000;
const MAX_PENDING_AGGREGATE_RANGES: usize = 20_000;
const MAX_AGGREGATE_PLAY_SHADOWS: usize = 20_000;
const MAX_HISTORY_DEDUPE_CREDITS: usize = 20_000;
const MAX_HISTORY_CURSORS: usize = 32;
const MAX_OUTBOUND_SCROBBLES: usize = 20_000;
const MAX_OUTBOUND_ECHOES: usize = 20_000;
const MAX_COMPLETED_OUTBOUND_SCROBBLES: usize = 20_000;
pub(crate) const MAX_UNCERTAIN_SCROBBLE_READBACKS: u8 = 3;
const MAX_EVENT_ID_BYTES: usize = 512;
const MAX_CURSOR_KEY_BYTES: usize = 128;
const MAX_PLAYED_AT_BYTES: usize = 256;
const MAX_PORTABLE_TEXT_CHARS: usize = 1_024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BridgeMutationError {
    InvalidEntry,
    CapacityExceeded,
    ConflictingEntry,
}

/// Exact last rating observation for one server item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RatingShadow {
    pub raw: RawServerRating,
    pub observed_at_unix: i64,
    /// The local operation whose server echo produced this observation, when known.
    pub confirmed_operation_id: Option<String>,
}

/// Durable two-call projection state. A projection is complete only after `Readback`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PendingRatingProjectionStage {
    SetRating,
    SetStarred,
    Readback,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PendingRatingProjection {
    pub operation_id: String,
    pub target: Rating,
    pub stage: PendingRatingProjectionStage,
    /// First non-canonical readback candidate. A consecutive identical readback is treated as a
    /// stable external observation; a changing value replaces this candidate without another write.
    pub last_readback: Option<RawServerRating>,
    pub queued_at_unix: i64,
}

/// A server observation waiting to become one external personal-state operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PendingRatingImport {
    pub item_id: ItemId,
    /// Sanitized portable metadata makes a committed observation replayable while offline.
    pub track: PortableTrack,
    pub raw: RawServerRating,
    pub mapped: Rating,
    pub observed_at_unix: i64,
}

/// Exact or aggregate server evidence waiting to become one engagement operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PendingEngagementImport {
    pub track: PortableTrack,
    pub engagement: EngagementKind,
    pub played_duration_ms: Option<u64>,
    pub total_duration_ms: Option<u64>,
    pub artist_key: String,
    pub observed_at_unix: i64,
}

/// Standard OpenSubsonic aggregate history evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AggregatePlayShadow {
    pub play_count: u64,
    pub played_at: Option<String>,
    pub observed_at_unix: i64,
    /// Local generation used to keep counter ordinals unique after a server reset.
    #[serde(default)]
    pub counter_epoch: u64,
}

/// Durable two-way evidence credits prevent exact and aggregate history from counting one play
/// twice regardless of which source arrives first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HistoryDedupeCredits {
    pub exact_unmatched: u64,
    pub aggregate_unmatched: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OutboundScrobbleKind {
    NowPlaying,
    Submission,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OutboundScrobbleDelivery {
    #[default]
    Queued,
    /// The request may have reached the server and must never be blindly resent.
    Uncertain,
    /// Bounded readback could not prove delivery. Only an exact echo or an explicit user
    /// resolution may release this report.
    NeedsAttention,
}

/// One exact local event waiting for standard OpenSubsonic scrobble acknowledgement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PendingOutboundScrobble {
    pub event_id: String,
    pub item_id: ItemId,
    pub played_at_unix: i64,
    pub kind: OutboundScrobbleKind,
    #[serde(default)]
    pub delivery: OutboundScrobbleDelivery,
    #[serde(default)]
    pub baseline_captured: bool,
    #[serde(default)]
    pub baseline_play_count: Option<u64>,
    #[serde(default)]
    pub baseline_played_at: Option<String>,
    #[serde(default)]
    pub exact_credit_recorded: bool,
    /// Counter generation in which `exact_credit_recorded` was registered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exact_credit_epoch: Option<u64>,
    /// Successful, unchanged aggregate readbacks after an ambiguous response.
    #[serde(default)]
    pub uncertain_readbacks: u8,
    /// The source journal durably entered `AwaitingSourceAck` and the bridge accepted that proof.
    ///
    /// The intermediate marker may still exist, but restart will replay only this acknowledgement,
    /// never the server submission. Until this bit crosses the bridge-store durability boundary, a
    /// successful submission must retain a completion receipt.
    #[serde(default)]
    pub source_marker_acknowledged: bool,
}

/// Successful local submission retained until the native-history importer observes its echo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OutboundScrobbleEcho {
    pub event_id: String,
    pub item_id: ItemId,
    pub played_at_unix: i64,
}

/// A bounded completion receipt makes owner-event replay a durable no-op after acknowledgement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CompletedOutboundScrobble {
    pub event_id: String,
    pub item_id: ItemId,
    pub played_at_unix: i64,
    pub kind: OutboundScrobbleKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenSubsonicBridgeState {
    revision: u64,
    backend_id: BackendId,
    account_scope_id: AccountScopeId,
    rating_shadows: BTreeMap<ItemId, RatingShadow>,
    pending_rating_projections: BTreeMap<ItemId, PendingRatingProjection>,
    pending_rating_imports: BTreeMap<String, PendingRatingImport>,
    pending_engagement_imports: BTreeMap<String, PendingEngagementImport>,
    pending_aggregate_ranges: VecDeque<PendingAggregateRange>,
    aggregate_play_shadows: BTreeMap<ItemId, AggregatePlayShadow>,
    history_dedupe_credits: BTreeMap<ItemId, BTreeMap<u64, HistoryDedupeCredits>>,
    history_cursors: BTreeMap<String, HistoryCursor>,
    native_history_health: NativeHistoryHealth,
    outbound_scrobbles: VecDeque<PendingOutboundScrobble>,
    outbound_echoes: VecDeque<OutboundScrobbleEcho>,
    completed_outbound_scrobbles: VecDeque<CompletedOutboundScrobble>,
}

#[allow(
    dead_code,
    reason = "bounded durable bridge APIs are consumed incrementally by the PR 7 owner runtime"
)]
impl OpenSubsonicBridgeState {
    pub fn new(backend_id: BackendId, account_scope_id: AccountScopeId) -> Self {
        Self {
            revision: 0,
            backend_id,
            account_scope_id,
            rating_shadows: BTreeMap::new(),
            pending_rating_projections: BTreeMap::new(),
            pending_rating_imports: BTreeMap::new(),
            pending_engagement_imports: BTreeMap::new(),
            pending_aggregate_ranges: VecDeque::new(),
            aggregate_play_shadows: BTreeMap::new(),
            history_dedupe_credits: BTreeMap::new(),
            history_cursors: BTreeMap::new(),
            native_history_health: NativeHistoryHealth::Off,
            outbound_scrobbles: VecDeque::new(),
            outbound_echoes: VecDeque::new(),
            completed_outbound_scrobbles: VecDeque::new(),
        }
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn backend_id(&self) -> &BackendId {
        &self.backend_id
    }

    pub fn account_scope_id(&self) -> &AccountScopeId {
        &self.account_scope_id
    }

    pub(crate) fn rating_shadows(&self) -> &BTreeMap<ItemId, RatingShadow> {
        &self.rating_shadows
    }

    pub(crate) fn rating_shadow(&self, item_id: &ItemId) -> Option<&RatingShadow> {
        self.rating_shadows.get(item_id)
    }

    pub(crate) fn upsert_rating_shadow(
        &mut self,
        item_id: ItemId,
        shadow: RatingShadow,
    ) -> Result<(), BridgeMutationError> {
        self.upsert_rating_shadow_with_limit(item_id, shadow, MAX_RATING_SHADOWS)
    }

    fn upsert_rating_shadow_with_limit(
        &mut self,
        item_id: ItemId,
        shadow: RatingShadow,
        limit: usize,
    ) -> Result<(), BridgeMutationError> {
        validate_rating_shadow(&shadow)?;
        if self.rating_shadows.contains_key(&item_id) || self.rating_shadows.len() < limit {
            self.rating_shadows.insert(item_id, shadow);
            return Ok(());
        }

        let incoming_is_protected = self.pending_rating_projections.contains_key(&item_id)
            || self
                .pending_rating_imports
                .values()
                .any(|pending| pending.item_id == item_id);
        let mut victim = (!incoming_is_protected).then(|| {
            (
                shadow.observed_at_unix,
                item_id.as_str().to_owned(),
                item_id.clone(),
            )
        });
        for (candidate_id, candidate) in &self.rating_shadows {
            let protected = self.pending_rating_projections.contains_key(candidate_id)
                || self
                    .pending_rating_imports
                    .values()
                    .any(|pending| pending.item_id == *candidate_id);
            if protected {
                continue;
            }
            let key = (
                candidate.observed_at_unix,
                candidate_id.as_str().to_owned(),
                candidate_id.clone(),
            );
            if victim.as_ref().is_none_or(|current| key < *current) {
                victim = Some(key);
            }
        }
        let Some((_, _, victim_id)) = victim else {
            return Err(BridgeMutationError::CapacityExceeded);
        };
        if victim_id != item_id {
            self.rating_shadows.remove(&victim_id);
            self.rating_shadows.insert(item_id, shadow);
        }
        Ok(())
    }

    pub(crate) fn pending_rating_projections(&self) -> &BTreeMap<ItemId, PendingRatingProjection> {
        &self.pending_rating_projections
    }

    pub(crate) fn queue_rating_projection(
        &mut self,
        item_id: ItemId,
        pending: PendingRatingProjection,
    ) -> Result<(), BridgeMutationError> {
        validate_pending_projection(&pending)?;
        if !self.pending_rating_projections.contains_key(&item_id)
            && self.pending_rating_projections.len() >= MAX_PENDING_RATING_PROJECTIONS
        {
            return Err(BridgeMutationError::CapacityExceeded);
        }
        self.pending_rating_projections.insert(item_id, pending);
        Ok(())
    }

    pub(crate) fn remove_rating_projection(
        &mut self,
        item_id: &ItemId,
    ) -> Option<PendingRatingProjection> {
        self.pending_rating_projections.remove(item_id)
    }

    pub(crate) fn pending_rating_imports(&self) -> &BTreeMap<String, PendingRatingImport> {
        &self.pending_rating_imports
    }

    pub(crate) fn queue_rating_import(
        &mut self,
        observation_id: String,
        pending: PendingRatingImport,
    ) -> Result<(), BridgeMutationError> {
        validate_identifier(&observation_id, MAX_EVENT_ID_BYTES)?;
        self.validate_pending_import(&pending)?;
        if let Some(existing) = self.pending_rating_imports.get(&observation_id) {
            return if existing == &pending {
                Ok(())
            } else {
                Err(BridgeMutationError::ConflictingEntry)
            };
        }
        let replaces_same_item = self
            .pending_rating_imports
            .values()
            .any(|existing| existing.item_id == pending.item_id);
        if !replaces_same_item && self.pending_rating_imports.len() >= MAX_PENDING_RATING_IMPORTS {
            return Err(BridgeMutationError::CapacityExceeded);
        }
        // A rating is a register, not an event stream. Only the most recently observed value for
        // one exact server item needs to cross the owner boundary. Coalescing here also preserves
        // that order across a crash: replay must not depend on the lexical order of hashed
        // observation IDs in the map.
        self.pending_rating_imports
            .retain(|_, existing| existing.item_id != pending.item_id);
        self.pending_rating_imports.insert(observation_id, pending);
        Ok(())
    }

    pub(crate) fn remove_rating_import(
        &mut self,
        observation_id: &str,
    ) -> Option<PendingRatingImport> {
        self.pending_rating_imports.remove(observation_id)
    }

    pub(crate) fn pending_engagement_imports(&self) -> &BTreeMap<String, PendingEngagementImport> {
        &self.pending_engagement_imports
    }

    pub(crate) fn queue_engagement_import(
        &mut self,
        observation_id: String,
        pending: PendingEngagementImport,
    ) -> Result<(), BridgeMutationError> {
        self.queue_engagement_import_with_limit(
            observation_id,
            pending,
            MAX_PENDING_ENGAGEMENT_IMPORTS,
        )
    }

    fn queue_engagement_import_with_limit(
        &mut self,
        observation_id: String,
        pending: PendingEngagementImport,
        limit: usize,
    ) -> Result<(), BridgeMutationError> {
        validate_identifier(&observation_id, MAX_EVENT_ID_BYTES)?;
        self.validate_pending_engagement_import(&pending)?;
        if let Some(existing) = self.pending_engagement_imports.get(&observation_id) {
            return if existing == &pending {
                Ok(())
            } else {
                Err(BridgeMutationError::ConflictingEntry)
            };
        }
        if self.pending_engagement_imports.len() >= limit {
            return Err(BridgeMutationError::CapacityExceeded);
        }
        self.pending_engagement_imports
            .insert(observation_id, pending);
        Ok(())
    }

    pub(crate) fn remove_engagement_import(
        &mut self,
        observation_id: &str,
    ) -> Option<PendingEngagementImport> {
        self.pending_engagement_imports.remove(observation_id)
    }

    pub(crate) fn aggregate_play_shadows(&self) -> &BTreeMap<ItemId, AggregatePlayShadow> {
        &self.aggregate_play_shadows
    }

    pub(crate) fn upsert_aggregate_play_shadow(
        &mut self,
        item_id: ItemId,
        shadow: AggregatePlayShadow,
    ) -> Result<(), BridgeMutationError> {
        validate_aggregate_shadow(&shadow)?;
        if !self.aggregate_play_shadows.contains_key(&item_id)
            && self.aggregate_play_shadows.len() >= MAX_AGGREGATE_PLAY_SHADOWS
        {
            let victim = self
                .aggregate_play_shadows
                .iter()
                .filter(|(candidate_id, _)| !self.aggregate_item_is_protected(candidate_id))
                .map(|(candidate_id, candidate)| {
                    (
                        candidate.observed_at_unix,
                        candidate_id.as_str(),
                        candidate_id,
                    )
                })
                .min_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(right.1)))
                .map(|(_, _, candidate_id)| candidate_id.clone())
                .ok_or(BridgeMutationError::CapacityExceeded)?;
            self.aggregate_play_shadows.remove(&victim);
        }
        self.aggregate_play_shadows.insert(item_id, shadow);
        Ok(())
    }

    pub fn native_history_health(&self) -> NativeHistoryHealth {
        self.native_history_health
    }

    pub fn set_native_history_health(&mut self, health: NativeHistoryHealth) {
        self.native_history_health = health;
    }

    pub(crate) fn queue_outbound_scrobble(
        &mut self,
        pending: PendingOutboundScrobble,
    ) -> Result<(), BridgeMutationError> {
        validate_outbound_scrobble(&pending)?;
        if let Some(existing) = self
            .outbound_scrobbles
            .iter()
            .find(|existing| existing.event_id == pending.event_id)
        {
            return if same_outbound_identity(existing, &pending) {
                Ok(())
            } else {
                Err(BridgeMutationError::ConflictingEntry)
            };
        }
        if let Some(existing) = self
            .outbound_echoes
            .iter()
            .find(|existing| existing.event_id == pending.event_id)
        {
            return if pending.kind == OutboundScrobbleKind::Submission
                && existing.item_id == pending.item_id
                && existing.played_at_unix == pending.played_at_unix
            {
                Ok(())
            } else {
                Err(BridgeMutationError::ConflictingEntry)
            };
        }
        if let Some(existing) = self
            .completed_outbound_scrobbles
            .iter()
            .find(|existing| existing.event_id == pending.event_id)
        {
            return if completed_matches_pending(existing, &pending) {
                Ok(())
            } else {
                Err(BridgeMutationError::ConflictingEntry)
            };
        }
        if self.outbound_scrobbles.len() >= MAX_OUTBOUND_SCROBBLES {
            return Err(BridgeMutationError::CapacityExceeded);
        }
        self.outbound_scrobbles.push_back(pending);
        Ok(())
    }

    /// Accept a durable `AwaitingSourceAck` marker from the source JSONL journal.
    ///
    /// A pending report carries the acknowledgement into completion. A completed report no longer
    /// needs its replay tombstone and can release that bounded slot immediately. Missing entries
    /// are an idempotent success: either the acknowledgement was already applied or the source had
    /// no durable marker (for example, an ephemeral now-playing report).
    pub(crate) fn acknowledge_outbound_source(
        &mut self,
        event_id: &str,
    ) -> Result<(), BridgeMutationError> {
        validate_identifier(event_id, MAX_EVENT_ID_BYTES)?;
        if let Some(pending) = self
            .outbound_scrobbles
            .iter_mut()
            .find(|pending| pending.event_id == event_id)
        {
            pending.source_marker_acknowledged = true;
            return Ok(());
        }
        if let Some(position) = self
            .completed_outbound_scrobbles
            .iter()
            .position(|completed| completed.event_id == event_id)
        {
            self.completed_outbound_scrobbles.remove(position);
        }
        Ok(())
    }

    pub(crate) fn replace_outbound_scrobble(
        &mut self,
        pending: PendingOutboundScrobble,
    ) -> Result<(), BridgeMutationError> {
        validate_outbound_scrobble(&pending)?;
        let Some(position) = self
            .outbound_scrobbles
            .iter()
            .position(|existing| existing.event_id == pending.event_id)
        else {
            return Err(BridgeMutationError::ConflictingEntry);
        };
        if !same_outbound_identity(&self.outbound_scrobbles[position], &pending) {
            return Err(BridgeMutationError::ConflictingEntry);
        }
        self.outbound_scrobbles[position] = pending;
        Ok(())
    }

    pub(crate) fn acknowledge_outbound_scrobble(
        &mut self,
        event_id: &str,
    ) -> Result<Option<PendingOutboundScrobble>, BridgeMutationError> {
        validate_identifier(event_id, MAX_EVENT_ID_BYTES)?;
        if self
            .outbound_scrobbles
            .front()
            .is_some_and(|pending| pending.event_id == event_id)
        {
            return Ok(self.outbound_scrobbles.pop_front());
        }
        if self
            .outbound_scrobbles
            .iter()
            .any(|pending| pending.event_id == event_id)
        {
            return Err(BridgeMutationError::ConflictingEntry);
        }
        Ok(None)
    }

    pub(crate) fn complete_outbound_scrobble(
        &mut self,
        event_id: &str,
    ) -> Result<Option<PendingOutboundScrobble>, BridgeMutationError> {
        validate_identifier(event_id, MAX_EVENT_ID_BYTES)?;
        let Some(position) = self
            .outbound_scrobbles
            .iter()
            .position(|pending| pending.event_id == event_id)
        else {
            return Ok(None);
        };
        let retain_completion = !self.outbound_scrobbles[position].source_marker_acknowledged;
        if retain_completion
            && self.completed_outbound_scrobbles.len() >= MAX_COMPLETED_OUTBOUND_SCROBBLES
        {
            // Every retained receipt protects a source marker whose durable removal has not been
            // observed. Evicting one would permit a duplicate after restart, so apply backpressure
            // and leave the uncertain/pending report intact.
            return Err(BridgeMutationError::CapacityExceeded);
        }
        let pending = self
            .outbound_scrobbles
            .remove(position)
            .ok_or(BridgeMutationError::ConflictingEntry)?;
        if !retain_completion {
            return Ok(Some(pending));
        }
        let completed = CompletedOutboundScrobble {
            event_id: pending.event_id.clone(),
            item_id: pending.item_id.clone(),
            played_at_unix: pending.played_at_unix,
            kind: pending.kind,
        };
        self.completed_outbound_scrobbles.push_back(completed);
        Ok(Some(pending))
    }

    pub(crate) fn record_outbound_echo(
        &mut self,
        echo: OutboundScrobbleEcho,
    ) -> Result<(), BridgeMutationError> {
        validate_identifier(&echo.event_id, MAX_EVENT_ID_BYTES)?;
        if let Some(existing) = self
            .outbound_echoes
            .iter()
            .find(|existing| existing.event_id == echo.event_id)
        {
            return if existing == &echo {
                Ok(())
            } else {
                Err(BridgeMutationError::ConflictingEntry)
            };
        }
        if self.outbound_echoes.len() == MAX_OUTBOUND_ECHOES {
            self.outbound_echoes.pop_front();
        }
        self.outbound_echoes.push_back(echo);
        Ok(())
    }

    pub(crate) fn consume_outbound_echo(
        &mut self,
        item_id: &ItemId,
        played_at_unix: i64,
    ) -> Option<OutboundScrobbleEcho> {
        let position = self
            .outbound_echoes
            .iter()
            .position(|echo| echo.item_id == *item_id && echo.played_at_unix == played_at_unix)?;
        self.outbound_echoes.remove(position)
    }

    #[cfg(test)]
    pub(crate) fn outbound_echoes(&self) -> &VecDeque<OutboundScrobbleEcho> {
        &self.outbound_echoes
    }

    #[cfg(test)]
    pub(crate) fn completed_outbound_scrobbles(&self) -> &VecDeque<CompletedOutboundScrobble> {
        &self.completed_outbound_scrobbles
    }

    pub(crate) fn set_revision(&mut self, revision: u64) {
        self.revision = revision;
    }

    fn validate(&self) -> Result<(), BridgeMutationError> {
        if self.rating_shadows.len() > MAX_RATING_SHADOWS
            || self.pending_rating_projections.len() > MAX_PENDING_RATING_PROJECTIONS
            || self.pending_rating_imports.len() > MAX_PENDING_RATING_IMPORTS
            || self.pending_engagement_imports.len() > MAX_PENDING_ENGAGEMENT_IMPORTS
            || self.pending_aggregate_ranges.len() > MAX_PENDING_AGGREGATE_RANGES
            || self.aggregate_play_shadows.len() > MAX_AGGREGATE_PLAY_SHADOWS
            || self.history_credit_entry_count()? > MAX_HISTORY_DEDUPE_CREDITS
            || self.history_cursors.len() > MAX_HISTORY_CURSORS
            || self.outbound_scrobbles.len() > MAX_OUTBOUND_SCROBBLES
            || self.outbound_echoes.len() > MAX_OUTBOUND_ECHOES
            || self.completed_outbound_scrobbles.len() > MAX_COMPLETED_OUTBOUND_SCROBBLES
        {
            return Err(BridgeMutationError::CapacityExceeded);
        }
        for shadow in self.rating_shadows.values() {
            validate_rating_shadow(shadow)?;
        }
        for pending in self.pending_rating_projections.values() {
            validate_pending_projection(pending)?;
        }
        for (observation_id, pending) in &self.pending_rating_imports {
            validate_identifier(observation_id, MAX_EVENT_ID_BYTES)?;
            self.validate_pending_import(pending)?;
        }
        let mut pending_rating_items = BTreeSet::new();
        if self
            .pending_rating_imports
            .values()
            .any(|pending| !pending_rating_items.insert(&pending.item_id))
        {
            return Err(BridgeMutationError::ConflictingEntry);
        }
        for (observation_id, pending) in &self.pending_engagement_imports {
            validate_identifier(observation_id, MAX_EVENT_ID_BYTES)?;
            self.validate_pending_engagement_import(pending)?;
        }
        self.validate_pending_aggregate_ranges()?;
        for shadow in self.aggregate_play_shadows.values() {
            validate_aggregate_shadow(shadow)?;
        }
        for (item_id, credits_by_epoch) in &self.history_dedupe_credits {
            if credits_by_epoch.is_empty() {
                return Err(BridgeMutationError::InvalidEntry);
            }
            for (epoch, credits) in credits_by_epoch {
                let epoch_is_valid = self
                    .aggregate_play_shadows
                    .get(item_id)
                    .map_or(*epoch == 0, |shadow| *epoch <= shadow.counter_epoch);
                if !epoch_is_valid
                    || (credits.exact_unmatched == 0 && credits.aggregate_unmatched == 0)
                {
                    return Err(BridgeMutationError::InvalidEntry);
                }
            }
        }
        for (source, cursor) in &self.history_cursors {
            validate_identifier(source, MAX_CURSOR_KEY_BYTES)?;
            history_cursor::validate(cursor)?;
        }
        let mut event_ids = BTreeSet::new();
        for pending in &self.outbound_scrobbles {
            validate_outbound_scrobble(pending)?;
            if !event_ids.insert(pending.event_id.as_str()) {
                return Err(BridgeMutationError::ConflictingEntry);
            }
        }
        for echo in &self.outbound_echoes {
            validate_identifier(&echo.event_id, MAX_EVENT_ID_BYTES)?;
            if !event_ids.insert(echo.event_id.as_str()) {
                return Err(BridgeMutationError::ConflictingEntry);
            }
        }
        let mut completed_ids = BTreeSet::new();
        for completed in &self.completed_outbound_scrobbles {
            validate_identifier(&completed.event_id, MAX_EVENT_ID_BYTES)?;
            if !completed_ids.insert(completed.event_id.as_str())
                || self
                    .outbound_scrobbles
                    .iter()
                    .any(|pending| pending.event_id == completed.event_id)
            {
                return Err(BridgeMutationError::ConflictingEntry);
            }
        }
        Ok(())
    }

    fn validate_pending_import(
        &self,
        pending: &PendingRatingImport,
    ) -> Result<(), BridgeMutationError> {
        pending
            .track
            .validate()
            .map_err(|_| BridgeMutationError::InvalidEntry)?;
        if pending.mapped != map_server_rating(pending.raw) {
            return Err(BridgeMutationError::InvalidEntry);
        }
        match &pending.track.key {
            PortableTrackKey::OpenSubsonic {
                backend_id,
                account_scope_id,
                item_id,
            } if backend_id == self.backend_id.as_str()
                && account_scope_id == self.account_scope_id.as_str()
                && item_id == pending.item_id.as_str() =>
            {
                Ok(())
            }
            _ => Err(BridgeMutationError::InvalidEntry),
        }
    }

    fn validate_pending_engagement_import(
        &self,
        pending: &PendingEngagementImport,
    ) -> Result<(), BridgeMutationError> {
        pending
            .track
            .validate()
            .map_err(|_| BridgeMutationError::InvalidEntry)?;
        validate_portable_text(&pending.artist_key)?;
        if let (Some(played), Some(total)) = (pending.played_duration_ms, pending.total_duration_ms)
            && total > 0
            && played > total.saturating_mul(4)
        {
            return Err(BridgeMutationError::InvalidEntry);
        }
        match &pending.track.key {
            PortableTrackKey::OpenSubsonic {
                backend_id,
                account_scope_id,
                ..
            } if backend_id == self.backend_id.as_str()
                && account_scope_id == self.account_scope_id.as_str() =>
            {
                Ok(())
            }
            _ => Err(BridgeMutationError::InvalidEntry),
        }
    }
}

fn validate_rating_shadow(shadow: &RatingShadow) -> Result<(), BridgeMutationError> {
    if let Some(operation_id) = &shadow.confirmed_operation_id {
        validate_identifier(operation_id, MAX_EVENT_ID_BYTES)?;
    }
    Ok(())
}

fn validate_pending_projection(
    pending: &PendingRatingProjection,
) -> Result<(), BridgeMutationError> {
    validate_identifier(&pending.operation_id, MAX_EVENT_ID_BYTES)
}

fn validate_aggregate_shadow(shadow: &AggregatePlayShadow) -> Result<(), BridgeMutationError> {
    if let Some(played_at) = &shadow.played_at {
        validate_text(played_at, MAX_PLAYED_AT_BYTES)?;
    }
    Ok(())
}

fn validate_identifier(value: &str, max_bytes: usize) -> Result<(), BridgeMutationError> {
    if value.is_empty()
        || value.len() > max_bytes
        || value.chars().any(|character| character.is_control())
    {
        return Err(BridgeMutationError::InvalidEntry);
    }
    Ok(())
}

fn validate_text(value: &str, max_bytes: usize) -> Result<(), BridgeMutationError> {
    if value.len() > max_bytes || value.chars().any(|character| character.is_control()) {
        return Err(BridgeMutationError::InvalidEntry);
    }
    Ok(())
}

fn validate_portable_text(value: &str) -> Result<(), BridgeMutationError> {
    let forbidden = |character: char| {
        character.is_control()
            || matches!(
                character,
                '\u{061c}'
                    | '\u{200b}'
                    | '\u{200c}'
                    | '\u{200d}'
                    | '\u{200e}'
                    | '\u{200f}'
                    | '\u{202a}'..='\u{202e}'
                    | '\u{2066}'..='\u{2069}'
                    | '\u{feff}'
            )
    };
    if value.chars().count() > MAX_PORTABLE_TEXT_CHARS || value.chars().any(forbidden) {
        return Err(BridgeMutationError::InvalidEntry);
    }
    Ok(())
}

#[derive(Deserialize)]
struct DiskBridgeHeader {
    kind: String,
    schema_version: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DiskBridgeStateV1 {
    kind: String,
    schema_version: u32,
    revision: u64,
    backend_id: BackendId,
    account_scope_id: AccountScopeId,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DiskBridgeStateV2 {
    kind: String,
    schema_version: u32,
    revision: u64,
    backend_id: BackendId,
    account_scope_id: AccountScopeId,
    rating_shadows: BTreeMap<ItemId, RatingShadow>,
    pending_rating_projections: BTreeMap<ItemId, PendingRatingProjection>,
    pending_rating_imports: BTreeMap<String, PendingRatingImport>,
    pending_engagement_imports: BTreeMap<String, PendingEngagementImport>,
    #[serde(default)]
    pending_aggregate_ranges: VecDeque<PendingAggregateRange>,
    aggregate_play_shadows: BTreeMap<ItemId, AggregatePlayShadow>,
    #[serde(default)]
    history_dedupe_credits: BTreeMap<ItemId, BTreeMap<u64, HistoryDedupeCredits>>,
    history_cursors: BTreeMap<String, HistoryCursor>,
    #[serde(default)]
    native_history_health: NativeHistoryHealth,
    outbound_scrobbles: VecDeque<PendingOutboundScrobble>,
    #[serde(default)]
    outbound_echoes: VecDeque<OutboundScrobbleEcho>,
    #[serde(default)]
    completed_outbound_scrobbles: VecDeque<CompletedOutboundScrobble>,
}

pub(crate) fn encode_bridge(
    state: &OpenSubsonicBridgeState,
) -> Result<Zeroizing<Vec<u8>>, StoreError> {
    state.validate().map_err(|_| StoreError::InvalidState)?;
    let disk = DiskBridgeStateV2 {
        kind: BRIDGE_KIND.to_owned(),
        schema_version: BRIDGE_SCHEMA_VERSION,
        revision: state.revision,
        backend_id: state.backend_id.clone(),
        account_scope_id: state.account_scope_id.clone(),
        rating_shadows: state.rating_shadows.clone(),
        pending_rating_projections: state.pending_rating_projections.clone(),
        pending_rating_imports: state.pending_rating_imports.clone(),
        pending_engagement_imports: state.pending_engagement_imports.clone(),
        pending_aggregate_ranges: state.pending_aggregate_ranges.clone(),
        aggregate_play_shadows: state.aggregate_play_shadows.clone(),
        history_dedupe_credits: state.history_dedupe_credits.clone(),
        history_cursors: state.history_cursors.clone(),
        native_history_health: state.native_history_health,
        outbound_scrobbles: state.outbound_scrobbles.clone(),
        outbound_echoes: state.outbound_echoes.clone(),
        completed_outbound_scrobbles: state.completed_outbound_scrobbles.clone(),
    };
    let bytes =
        Zeroizing::new(serde_json::to_vec(&disk).map_err(|_| StoreError::SerializationFailed)?);
    if bytes.len() as u64 > MAX_BRIDGE_BYTES {
        return Err(StoreError::PayloadTooLarge);
    }
    Ok(bytes)
}

pub(crate) fn decode_bridge(bytes: &[u8]) -> Result<OpenSubsonicBridgeState, StoreError> {
    if bytes.len() as u64 > MAX_BRIDGE_BYTES {
        return Err(StoreError::PayloadTooLarge);
    }
    let header: DiskBridgeHeader =
        serde_json::from_slice(bytes).map_err(|_| StoreError::InvalidState)?;
    if header.kind != BRIDGE_KIND {
        return Err(StoreError::InvalidState);
    }
    let state = match header.schema_version {
        1 => {
            let disk: DiskBridgeStateV1 =
                serde_json::from_slice(bytes).map_err(|_| StoreError::InvalidState)?;
            if disk.kind != BRIDGE_KIND || disk.schema_version != 1 {
                return Err(StoreError::InvalidState);
            }
            let mut state = OpenSubsonicBridgeState::new(disk.backend_id, disk.account_scope_id);
            state.revision = disk.revision;
            state
        }
        BRIDGE_SCHEMA_VERSION => {
            let disk: DiskBridgeStateV2 =
                serde_json::from_slice(bytes).map_err(|_| StoreError::InvalidState)?;
            if disk.kind != BRIDGE_KIND || disk.schema_version != BRIDGE_SCHEMA_VERSION {
                return Err(StoreError::InvalidState);
            }
            OpenSubsonicBridgeState {
                revision: disk.revision,
                backend_id: disk.backend_id,
                account_scope_id: disk.account_scope_id,
                rating_shadows: disk.rating_shadows,
                pending_rating_projections: disk.pending_rating_projections,
                pending_rating_imports: disk.pending_rating_imports,
                pending_engagement_imports: disk.pending_engagement_imports,
                pending_aggregate_ranges: disk.pending_aggregate_ranges,
                aggregate_play_shadows: disk.aggregate_play_shadows,
                history_dedupe_credits: disk.history_dedupe_credits,
                history_cursors: disk.history_cursors,
                native_history_health: disk.native_history_health,
                outbound_scrobbles: disk.outbound_scrobbles,
                outbound_echoes: disk.outbound_echoes,
                completed_outbound_scrobbles: disk.completed_outbound_scrobbles,
            }
        }
        _ => return Err(StoreError::InvalidState),
    };
    state.validate().map_err(|_| StoreError::InvalidState)?;
    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    mod aggregate_continuation;
    mod outbound_scrobble;
    mod rating_coalescing;

    fn bridge() -> OpenSubsonicBridgeState {
        OpenSubsonicBridgeState::new(
            BackendId::new("backend").unwrap(),
            AccountScopeId::new("account").unwrap(),
        )
    }

    fn shadow(rating: Option<i64>, starred: bool, observed_at_unix: i64) -> RatingShadow {
        RatingShadow {
            raw: RawServerRating {
                user_rating: rating,
                starred,
            },
            observed_at_unix,
            confirmed_operation_id: None,
        }
    }

    fn portable(item_id: &str) -> PortableTrack {
        PortableTrack {
            key: PortableTrackKey::OpenSubsonic {
                backend_id: "backend".to_owned(),
                account_scope_id: "account".to_owned(),
                item_id: item_id.to_owned(),
            },
            title: "Title".to_owned(),
            artist: "Artist".to_owned(),
            album: Some("Album".to_owned()),
            duration_secs: Some(180),
            isrc: None,
        }
    }

    fn engagement(item_id: &str) -> PendingEngagementImport {
        PendingEngagementImport {
            track: portable(item_id),
            engagement: EngagementKind::Play,
            played_duration_ms: None,
            total_duration_ms: None,
            artist_key: "artist".to_owned(),
            observed_at_unix: 12,
        }
    }

    #[test]
    fn bridge_round_trip_preserves_every_queue_and_invalid_raw_rating() {
        let mut bridge = bridge();
        bridge.set_native_history_health(NativeHistoryHealth::Detailed);
        let item = ItemId::new("song").unwrap();
        bridge
            .upsert_rating_shadow(item.clone(), shadow(Some(99), true, 10))
            .unwrap();
        bridge
            .queue_rating_projection(
                item.clone(),
                PendingRatingProjection {
                    operation_id: "local-op".to_owned(),
                    target: Rating::Disliked,
                    stage: PendingRatingProjectionStage::SetStarred,
                    last_readback: Some(RawServerRating {
                        user_rating: Some(99),
                        starred: true,
                    }),
                    queued_at_unix: 11,
                },
            )
            .unwrap();
        bridge
            .queue_rating_import(
                "server-observation".to_owned(),
                PendingRatingImport {
                    item_id: item.clone(),
                    track: portable("song"),
                    raw: RawServerRating {
                        user_rating: Some(-9),
                        starred: false,
                    },
                    mapped: Rating::Neutral,
                    observed_at_unix: 12,
                },
            )
            .unwrap();
        bridge
            .queue_engagement_import("history-row".to_owned(), engagement("song"))
            .unwrap();
        bridge
            .upsert_aggregate_play_shadow(
                item.clone(),
                AggregatePlayShadow {
                    play_count: 7,
                    played_at: Some("2026-07-26T10:00:00Z".to_owned()),
                    observed_at_unix: 13,
                    counter_epoch: 2,
                },
            )
            .unwrap();
        bridge
            .set_history_cursor(
                "navidrome-native".to_owned(),
                HistoryCursor {
                    high_water_id: Some("row-7".to_owned()),
                    overlap_started_at_unix: Some(14),
                    updated_at_unix: 15,
                    continuation: None,
                    pending_metadata_rows: Vec::new(),
                },
            )
            .unwrap();
        bridge
            .queue_outbound_scrobble(PendingOutboundScrobble {
                event_id: "engagement-1".to_owned(),
                item_id: item,
                played_at_unix: 16,
                kind: OutboundScrobbleKind::Submission,
                delivery: OutboundScrobbleDelivery::Queued,
                baseline_captured: false,
                baseline_play_count: None,
                baseline_played_at: None,
                exact_credit_recorded: false,
                exact_credit_epoch: None,
                uncertain_readbacks: 0,
                source_marker_acknowledged: false,
            })
            .unwrap();
        bridge
            .record_outbound_echo(OutboundScrobbleEcho {
                event_id: "echo-1".to_owned(),
                item_id: ItemId::new("song").unwrap(),
                played_at_unix: 17,
            })
            .unwrap();

        let decoded = decode_bridge(&encode_bridge(&bridge).unwrap()).unwrap();
        assert_eq!(decoded, bridge);
        assert_eq!(
            decoded
                .rating_shadow(&ItemId::new("song").unwrap())
                .unwrap()
                .raw
                .user_rating,
            Some(99)
        );
    }

    #[test]
    fn schema_one_empty_bridge_migrates_explicitly_to_schema_two() {
        let legacy = br#"{
            "kind":"yututui_open_subsonic_bridge",
            "schema_version":1,
            "revision":8,
            "backend_id":"backend",
            "account_scope_id":"account"
        }"#;
        let migrated = decode_bridge(legacy).unwrap();
        assert_eq!(migrated.revision(), 8);
        assert!(migrated.rating_shadows().is_empty());
        assert!(migrated.pending_rating_projections().is_empty());
        assert!(migrated.pending_rating_imports().is_empty());
        assert!(migrated.pending_engagement_imports().is_empty());
        assert!(migrated.aggregate_play_shadows().is_empty());
        assert!(migrated.history_dedupe_credits().is_empty());
        assert!(migrated.history_cursors().is_empty());
        assert!(migrated.outbound_scrobbles().is_empty());
        assert!(migrated.outbound_echoes().is_empty());
        assert!(migrated.completed_outbound_scrobbles().is_empty());
        assert_eq!(migrated.native_history_health(), NativeHistoryHealth::Off);
        let encoded: serde_json::Value =
            serde_json::from_slice(&encode_bridge(&migrated).unwrap()).unwrap();
        assert_eq!(
            encoded["schema_version"],
            serde_json::Value::from(BRIDGE_SCHEMA_VERSION)
        );
    }

    #[test]
    fn schema_two_aggregate_shadow_without_counter_epoch_defaults_to_legacy_ids() {
        let mut state = bridge();
        state
            .upsert_aggregate_play_shadow(
                ItemId::new("song").unwrap(),
                AggregatePlayShadow {
                    play_count: 4,
                    played_at: None,
                    observed_at_unix: 12,
                    counter_epoch: 3,
                },
            )
            .unwrap();
        let mut encoded: serde_json::Value =
            serde_json::from_slice(&encode_bridge(&state).unwrap()).unwrap();
        encoded["aggregate_play_shadows"]["song"]
            .as_object_mut()
            .unwrap()
            .remove("counter_epoch");
        encoded
            .as_object_mut()
            .unwrap()
            .remove("native_history_health");

        let decoded = decode_bridge(&serde_json::to_vec(&encoded).unwrap()).unwrap();
        assert_eq!(
            decoded
                .aggregate_play_shadows()
                .get(&ItemId::new("song").unwrap())
                .unwrap()
                .counter_epoch,
            0
        );
        assert_eq!(decoded.native_history_health(), NativeHistoryHealth::Off);
    }

    #[test]
    fn ordinary_shadows_have_deterministic_eviction_and_pending_are_protected() {
        let mut bridge = bridge();
        let protected = ItemId::new("protected").unwrap();
        bridge
            .upsert_rating_shadow_with_limit(protected.clone(), shadow(Some(5), true, 1), 2)
            .unwrap();
        bridge
            .queue_rating_projection(
                protected.clone(),
                PendingRatingProjection {
                    operation_id: "op".to_owned(),
                    target: Rating::Liked,
                    stage: PendingRatingProjectionStage::Readback,
                    last_readback: None,
                    queued_at_unix: 1,
                },
            )
            .unwrap();
        let tie_loser = ItemId::new("a").unwrap();
        bridge
            .upsert_rating_shadow_with_limit(tie_loser.clone(), shadow(None, false, 2), 2)
            .unwrap();
        let kept = ItemId::new("z").unwrap();
        bridge
            .upsert_rating_shadow_with_limit(kept.clone(), shadow(None, false, 2), 2)
            .unwrap();

        assert!(bridge.rating_shadow(&protected).is_some());
        assert!(bridge.rating_shadow(&tie_loser).is_none());
        assert!(bridge.rating_shadow(&kept).is_some());
    }

    #[test]
    fn pending_work_is_idempotent_but_never_evicted_or_reordered() {
        let mut bridge = bridge();
        let item = ItemId::new("song").unwrap();
        let import = PendingRatingImport {
            item_id: item.clone(),
            track: portable("song"),
            raw: RawServerRating {
                user_rating: Some(1),
                starred: true,
            },
            mapped: Rating::Disliked,
            observed_at_unix: 1,
        };
        bridge
            .queue_rating_import("observation".to_owned(), import.clone())
            .unwrap();
        bridge
            .queue_rating_import("observation".to_owned(), import)
            .unwrap();
        assert_eq!(bridge.pending_rating_imports().len(), 1);

        let pending_engagement = engagement("song");
        bridge
            .queue_engagement_import("history".to_owned(), pending_engagement.clone())
            .unwrap();
        bridge
            .queue_engagement_import("history".to_owned(), pending_engagement.clone())
            .unwrap();
        assert_eq!(bridge.pending_engagement_imports().len(), 1);
        let mut conflicting_engagement = pending_engagement;
        conflicting_engagement.engagement = EngagementKind::Completion;
        assert_eq!(
            bridge.queue_engagement_import("history".to_owned(), conflicting_engagement),
            Err(BridgeMutationError::ConflictingEntry)
        );
        assert_eq!(
            bridge.queue_engagement_import_with_limit(
                "history-2".to_owned(),
                engagement("song"),
                1,
            ),
            Err(BridgeMutationError::CapacityExceeded)
        );

        let first = PendingOutboundScrobble {
            event_id: "event-1".to_owned(),
            item_id: item.clone(),
            played_at_unix: 2,
            kind: OutboundScrobbleKind::NowPlaying,
            delivery: OutboundScrobbleDelivery::Queued,
            baseline_captured: false,
            baseline_play_count: None,
            baseline_played_at: None,
            exact_credit_recorded: false,
            exact_credit_epoch: None,
            uncertain_readbacks: 0,
            source_marker_acknowledged: false,
        };
        let second = PendingOutboundScrobble {
            event_id: "event-2".to_owned(),
            item_id: item,
            played_at_unix: 3,
            kind: OutboundScrobbleKind::Submission,
            delivery: OutboundScrobbleDelivery::Queued,
            baseline_captured: false,
            baseline_play_count: None,
            baseline_played_at: None,
            exact_credit_recorded: false,
            exact_credit_epoch: None,
            uncertain_readbacks: 0,
            source_marker_acknowledged: false,
        };
        bridge.queue_outbound_scrobble(first.clone()).unwrap();
        bridge.queue_outbound_scrobble(first.clone()).unwrap();
        bridge.queue_outbound_scrobble(second).unwrap();
        assert_eq!(bridge.outbound_scrobbles().len(), 2);
        assert_eq!(
            bridge.acknowledge_outbound_scrobble("event-2"),
            Err(BridgeMutationError::ConflictingEntry)
        );
        assert_eq!(
            bridge.acknowledge_outbound_scrobble("event-1").unwrap(),
            Some(first)
        );
    }

    #[test]
    fn native_echo_marker_is_exact_idempotent_and_consumed_once() {
        let mut bridge = bridge();
        let item = ItemId::new("song").unwrap();
        let echo = OutboundScrobbleEcho {
            event_id: "local-submission".to_owned(),
            item_id: item.clone(),
            played_at_unix: 42,
        };
        bridge.record_outbound_echo(echo.clone()).unwrap();
        bridge.record_outbound_echo(echo.clone()).unwrap();
        assert!(bridge.consume_outbound_echo(&item, 41).is_none());
        assert_eq!(bridge.consume_outbound_echo(&item, 42), Some(echo));
        assert!(bridge.consume_outbound_echo(&item, 42).is_none());
    }

    #[test]
    fn exact_and_aggregate_history_credits_cancel_in_either_arrival_order() {
        let item = ItemId::new("song").unwrap();

        let mut zero_baseline = bridge();
        assert_eq!(
            zero_baseline
                .record_aggregate_history_evidence(item.clone(), 0, 0)
                .unwrap(),
            0
        );
        zero_baseline
            .reconcile_native_aggregate_baseline(item.clone(), 0, 0)
            .unwrap();
        assert!(zero_baseline.history_dedupe_credits().is_empty());

        let mut exact_first = bridge();
        assert!(
            exact_first
                .record_exact_history_evidence(item.clone(), 0)
                .unwrap()
        );
        assert_eq!(
            exact_first
                .record_aggregate_history_evidence(item.clone(), 0, 1)
                .unwrap(),
            0
        );
        assert!(exact_first.history_dedupe_credits().is_empty());

        let mut aggregate_first = bridge();
        assert_eq!(
            aggregate_first
                .record_aggregate_history_evidence(item.clone(), 0, 1)
                .unwrap(),
            1
        );
        assert!(
            !aggregate_first
                .record_exact_history_evidence(item, 0)
                .unwrap()
        );
        assert!(aggregate_first.history_dedupe_credits().is_empty());
    }

    #[test]
    fn counter_epochs_keep_exact_and_aggregate_credits_isolated() {
        let mut bridge = bridge();
        let item = ItemId::new("song").unwrap();
        assert_eq!(
            bridge
                .record_aggregate_history_evidence(item.clone(), 0, 1)
                .unwrap(),
            1
        );
        assert!(
            bridge
                .record_exact_history_evidence(item.clone(), 1)
                .unwrap()
        );
        let epochs = bridge.history_dedupe_credits().get(&item).unwrap();
        assert_eq!(epochs.get(&0).unwrap().aggregate_unmatched, 1);
        assert_eq!(epochs.get(&0).unwrap().exact_unmatched, 0);
        assert_eq!(epochs.get(&1).unwrap().aggregate_unmatched, 0);
        assert_eq!(epochs.get(&1).unwrap().exact_unmatched, 1);

        bridge
            .reconcile_native_aggregate_baseline(item.clone(), 1, 1)
            .unwrap();
        let epochs = bridge.history_dedupe_credits().get(&item).unwrap();
        assert!(epochs.get(&1).is_none());
        assert_eq!(epochs.get(&0).unwrap().aggregate_unmatched, 1);
        assert!(
            !bridge
                .record_exact_history_evidence(item.clone(), 0)
                .unwrap()
        );
        assert!(bridge.history_dedupe_credits().get(&item).is_none());
    }

    #[test]
    fn pending_engagement_rejects_wrong_scope_and_impossible_duration() {
        let mut bridge = bridge();
        let mut pending = engagement("song");
        pending.track.key = PortableTrackKey::OpenSubsonic {
            backend_id: "different-backend".to_owned(),
            account_scope_id: "account".to_owned(),
            item_id: "song".to_owned(),
        };
        assert_eq!(
            bridge.queue_engagement_import("history".to_owned(), pending),
            Err(BridgeMutationError::InvalidEntry)
        );

        let mut pending = engagement("song");
        pending.played_duration_ms = Some(401);
        pending.total_duration_ms = Some(100);
        assert_eq!(
            bridge.queue_engagement_import("history".to_owned(), pending),
            Err(BridgeMutationError::InvalidEntry)
        );
    }

    #[test]
    fn pending_import_requires_matching_scope_item_and_fixed_mapping() {
        let mut bridge = bridge();
        let item = ItemId::new("song").unwrap();
        let mut pending = PendingRatingImport {
            item_id: item,
            track: portable("song"),
            raw: RawServerRating {
                user_rating: Some(1),
                starred: true,
            },
            mapped: Rating::Liked,
            observed_at_unix: 1,
        };
        assert_eq!(
            bridge.queue_rating_import("observation".to_owned(), pending.clone()),
            Err(BridgeMutationError::InvalidEntry)
        );

        pending.mapped = Rating::Disliked;
        pending.track.key = PortableTrackKey::OpenSubsonic {
            backend_id: "different-backend".to_owned(),
            account_scope_id: "account".to_owned(),
            item_id: "song".to_owned(),
        };
        assert_eq!(
            bridge.queue_rating_import("observation".to_owned(), pending),
            Err(BridgeMutationError::InvalidEntry)
        );
    }

    #[test]
    fn corrupt_or_oversized_v2_collections_are_rejected() {
        let mut value: serde_json::Value =
            serde_json::from_slice(&encode_bridge(&bridge()).unwrap()).unwrap();
        value["pending_rating_imports"] = serde_json::json!({
            "": {
                "item_id": "song",
                "raw": {"user_rating": null, "starred": false},
                "observed_at_unix": 0
            }
        });
        assert_eq!(
            decode_bridge(&serde_json::to_vec(&value).unwrap()),
            Err(StoreError::InvalidState)
        );
    }
}
