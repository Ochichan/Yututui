//! Small read and startup helpers for the durable outbound report lifecycle.

use super::{
    BridgeMutationError, CompletedOutboundScrobble, MAX_EVENT_ID_BYTES, MAX_PLAYED_AT_BYTES,
    MAX_UNCERTAIN_SCROBBLE_READBACKS, OpenSubsonicBridgeState, OutboundScrobbleDelivery,
    OutboundScrobbleKind, PendingOutboundScrobble, validate_identifier, validate_text,
};

pub(super) fn validate_outbound_scrobble(
    pending: &PendingOutboundScrobble,
) -> Result<(), BridgeMutationError> {
    validate_identifier(&pending.event_id, MAX_EVENT_ID_BYTES)?;
    if let Some(played_at) = &pending.baseline_played_at {
        validate_text(played_at, MAX_PLAYED_AT_BYTES)?;
    }
    if (pending.delivery == OutboundScrobbleDelivery::Queued && pending.exact_credit_recorded)
        || pending.exact_credit_recorded != pending.exact_credit_epoch.is_some()
        || pending.uncertain_readbacks > MAX_UNCERTAIN_SCROBBLE_READBACKS
        || (pending.delivery == OutboundScrobbleDelivery::Queued
            && pending.uncertain_readbacks != 0)
        || (pending.delivery == OutboundScrobbleDelivery::NeedsAttention
            && (pending.kind != OutboundScrobbleKind::Submission
                || !pending.baseline_captured
                || !pending.exact_credit_recorded
                || pending.uncertain_readbacks != MAX_UNCERTAIN_SCROBBLE_READBACKS))
    {
        return Err(BridgeMutationError::InvalidEntry);
    }
    Ok(())
}

pub(super) fn same_outbound_identity(
    left: &PendingOutboundScrobble,
    right: &PendingOutboundScrobble,
) -> bool {
    left.event_id == right.event_id
        && left.item_id == right.item_id
        && left.played_at_unix == right.played_at_unix
        && left.kind == right.kind
}

pub(super) fn completed_matches_pending(
    completed: &CompletedOutboundScrobble,
    pending: &PendingOutboundScrobble,
) -> bool {
    completed.event_id == pending.event_id
        && completed.item_id == pending.item_id
        && completed.played_at_unix == pending.played_at_unix
        && completed.kind == pending.kind
}

impl OpenSubsonicBridgeState {
    pub(crate) fn outbound_scrobbles(
        &self,
    ) -> &std::collections::VecDeque<PendingOutboundScrobble> {
        &self.outbound_scrobbles
    }

    pub(crate) fn outbound_scrobble_attention_ids(&self) -> Vec<String> {
        self.outbound_scrobbles
            .iter()
            .filter(|pending| pending.delivery == OutboundScrobbleDelivery::NeedsAttention)
            .map(|pending| pending.event_id.clone())
            .collect()
    }

    pub(crate) fn has_unresolved_outbound_submission(&self, item_id: &super::ItemId) -> bool {
        self.outbound_scrobbles.iter().any(|pending| {
            pending.item_id == *item_id
                && pending.kind == OutboundScrobbleKind::Submission
                && matches!(
                    pending.delivery,
                    OutboundScrobbleDelivery::Uncertain | OutboundScrobbleDelivery::NeedsAttention
                )
        })
    }

    pub(crate) fn discard_stale_now_playing(&mut self) -> usize {
        let before = self.outbound_scrobbles.len();
        self.outbound_scrobbles
            .retain(|pending| pending.kind != OutboundScrobbleKind::NowPlaying);
        before.saturating_sub(self.outbound_scrobbles.len())
    }
}
