//! Explicit recovery for outbound reports whose delivery cannot be proven.

use super::{BridgeRuntime, OutboundScrobbleResolution, mutation_error, outbound_echo};
use crate::open_subsonic::actor::ServiceError;
use crate::open_subsonic::bridge_store::{BridgeMutationError, OutboundScrobbleDelivery};
use crate::open_subsonic::transaction::OpenSubsonicStoreSet;

impl BridgeRuntime {
    pub(crate) fn outbound_scrobble_attention_ids(
        &self,
        store_set: &OpenSubsonicStoreSet,
    ) -> Vec<String> {
        store_set.bridge_state.outbound_scrobble_attention_ids()
    }

    pub(crate) fn resolve_outbound_scrobble(
        &self,
        store_set: &mut OpenSubsonicStoreSet,
        event_id: &str,
        resolution: OutboundScrobbleResolution,
    ) -> Result<(), ServiceError> {
        if !self.is_writable() {
            return Err(ServiceError::InvalidSetup);
        }
        let Some(mut pending) = store_set
            .bridge_state
            .outbound_scrobbles()
            .iter()
            .find(|pending| pending.event_id == event_id)
            .cloned()
        else {
            return Err(ServiceError::InvalidSetup);
        };
        if pending.delivery != OutboundScrobbleDelivery::NeedsAttention {
            return Err(ServiceError::InvalidSetup);
        }

        let before = store_set.bridge_state.clone();
        let mutation = (|| {
            match resolution {
                OutboundScrobbleResolution::Retry => {
                    if pending.exact_credit_recorded {
                        store_set.bridge_state.discard_exact_history_evidence(
                            &pending.item_id,
                            pending
                                .exact_credit_epoch
                                .ok_or(BridgeMutationError::InvalidEntry)?,
                        );
                    }
                    pending.delivery = OutboundScrobbleDelivery::Queued;
                    pending.baseline_captured = false;
                    pending.baseline_play_count = None;
                    pending.baseline_played_at = None;
                    pending.exact_credit_recorded = false;
                    pending.exact_credit_epoch = None;
                    pending.uncertain_readbacks = 0;
                    store_set.bridge_state.replace_outbound_scrobble(pending)?;
                }
                OutboundScrobbleResolution::MarkSent => {
                    store_set
                        .bridge_state
                        .record_outbound_echo(outbound_echo(&pending))?;
                    store_set
                        .bridge_state
                        .complete_outbound_scrobble(&pending.event_id)?;
                }
            }
            store_set
                .bridge_state
                .materialize_pending_aggregate_ranges()?;
            Ok::<(), BridgeMutationError>(())
        })();
        if let Err(error) = mutation {
            store_set.bridge_state = before;
            return Err(mutation_error(error));
        }
        self.persist_or_restore(store_set, before)?;
        Ok(())
    }
}
