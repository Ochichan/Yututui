//! Durable reconciliation between exact scrobble rows and aggregate play counters.

use super::{
    BridgeMutationError, HistoryDedupeCredits, ItemId, MAX_HISTORY_DEDUPE_CREDITS,
    OpenSubsonicBridgeState,
};

impl OpenSubsonicBridgeState {
    #[cfg(test)]
    pub(crate) fn history_dedupe_credits(
        &self,
    ) -> &std::collections::BTreeMap<ItemId, std::collections::BTreeMap<u64, HistoryDedupeCredits>>
    {
        &self.history_dedupe_credits
    }

    /// Record one exact event. `true` means it is new evidence that should be imported; `false`
    /// means an earlier aggregate fallback in the same counter generation already represented it.
    pub(crate) fn record_exact_history_evidence(
        &mut self,
        item_id: ItemId,
        counter_epoch: u64,
    ) -> Result<bool, BridgeMutationError> {
        self.ensure_credit_slot(&item_id, counter_epoch)?;
        let credits = self
            .history_dedupe_credits
            .entry(item_id.clone())
            .or_default()
            .entry(counter_epoch)
            .or_insert(HistoryDedupeCredits {
                exact_unmatched: 0,
                aggregate_unmatched: 0,
            });
        let should_import = if credits.aggregate_unmatched > 0 {
            credits.aggregate_unmatched -= 1;
            false
        } else {
            credits.exact_unmatched = credits
                .exact_unmatched
                .checked_add(1)
                .ok_or(BridgeMutationError::CapacityExceeded)?;
            true
        };
        self.remove_empty_credits(&item_id, counter_epoch);
        Ok(should_import)
    }

    /// Reserve one future aggregate increment for an outbound local event already present in the
    /// personal-state ledger.
    ///
    /// Unlike an imported exact history row, this must never pair with older aggregate evidence:
    /// doing so would silently relabel an unrelated mobile play as the local outbound report.
    pub(crate) fn reserve_outbound_exact_history_credit(
        &mut self,
        item_id: ItemId,
        counter_epoch: u64,
    ) -> Result<(), BridgeMutationError> {
        self.ensure_credit_slot(&item_id, counter_epoch)?;
        let credits = self
            .history_dedupe_credits
            .entry(item_id)
            .or_default()
            .entry(counter_epoch)
            .or_insert(HistoryDedupeCredits {
                exact_unmatched: 0,
                aggregate_unmatched: 0,
            });
        credits.exact_unmatched = credits
            .exact_unmatched
            .checked_add(1)
            .ok_or(BridgeMutationError::CapacityExceeded)?;
        Ok(())
    }

    /// Discard one speculative exact credit in the generation where it was registered.
    pub(crate) fn discard_exact_history_evidence(&mut self, item_id: &ItemId, counter_epoch: u64) {
        if let Some(credits) = self
            .history_dedupe_credits
            .get_mut(item_id)
            .and_then(|epochs| epochs.get_mut(&counter_epoch))
        {
            credits.exact_unmatched = credits.exact_unmatched.saturating_sub(1);
        }
        self.remove_empty_credits(item_id, counter_epoch);
    }

    pub(super) fn exact_history_credit_count(&self, item_id: &ItemId, counter_epoch: u64) -> u64 {
        self.history_dedupe_credits
            .get(item_id)
            .and_then(|epochs| epochs.get(&counter_epoch))
            .map_or(0, |credits| credits.exact_unmatched)
    }

    /// Reconcile aggregate counter growth against exact events. The returned suffix length is the
    /// number of aggregate operations that still need importing.
    pub(crate) fn record_aggregate_history_evidence(
        &mut self,
        item_id: ItemId,
        counter_epoch: u64,
        delta: u64,
    ) -> Result<u64, BridgeMutationError> {
        if delta == 0 {
            return Ok(0);
        }
        self.ensure_credit_slot(&item_id, counter_epoch)?;
        let credits = self
            .history_dedupe_credits
            .entry(item_id.clone())
            .or_default()
            .entry(counter_epoch)
            .or_insert(HistoryDedupeCredits {
                exact_unmatched: 0,
                aggregate_unmatched: 0,
            });
        let consumed = delta.min(credits.exact_unmatched);
        credits.exact_unmatched -= consumed;
        let remaining = delta - consumed;
        credits.aggregate_unmatched = credits
            .aggregate_unmatched
            .checked_add(remaining)
            .ok_or(BridgeMutationError::CapacityExceeded)?;
        self.remove_empty_credits(&item_id, counter_epoch);
        Ok(remaining)
    }

    /// Advance an aggregate baseline observed alongside exact rows without creating fallback
    /// operations. This consumes only exact credits from the matching counter generation.
    pub(crate) fn reconcile_native_aggregate_baseline(
        &mut self,
        item_id: ItemId,
        counter_epoch: u64,
        delta: u64,
    ) -> Result<(), BridgeMutationError> {
        if let Some(credits) = self
            .history_dedupe_credits
            .get_mut(&item_id)
            .and_then(|epochs| epochs.get_mut(&counter_epoch))
        {
            credits.exact_unmatched -= delta.min(credits.exact_unmatched);
        }
        self.remove_empty_credits(&item_id, counter_epoch);
        Ok(())
    }

    pub(super) fn history_credit_entry_count(&self) -> Result<usize, BridgeMutationError> {
        self.history_dedupe_credits
            .values()
            .try_fold(0_usize, |total, epochs| {
                total
                    .checked_add(epochs.len())
                    .ok_or(BridgeMutationError::CapacityExceeded)
            })
    }

    fn ensure_credit_slot(
        &self,
        item_id: &ItemId,
        counter_epoch: u64,
    ) -> Result<(), BridgeMutationError> {
        if self
            .history_dedupe_credits
            .get(item_id)
            .is_some_and(|epochs| epochs.contains_key(&counter_epoch))
        {
            return Ok(());
        }
        if self.history_credit_entry_count()? >= MAX_HISTORY_DEDUPE_CREDITS {
            return Err(BridgeMutationError::CapacityExceeded);
        }
        Ok(())
    }

    fn remove_empty_credits(&mut self, item_id: &ItemId, counter_epoch: u64) {
        if let Some(epochs) = self.history_dedupe_credits.get_mut(item_id) {
            if epochs.get(&counter_epoch).is_some_and(|credits| {
                credits.exact_unmatched == 0 && credits.aggregate_unmatched == 0
            }) {
                epochs.remove(&counter_epoch);
            }
            if epochs.is_empty() {
                self.history_dedupe_credits.remove(item_id);
            }
        }
    }
}
