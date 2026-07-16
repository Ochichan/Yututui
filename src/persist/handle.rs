use super::*;

impl PersistHandle {
    #[cfg(test)]
    pub(crate) fn detached_for_runtime_test() -> Self {
        let (tx, rx) = crate::util::backpressure::bounded_channel(
            crate::util::backpressure::PERSIST_CONTROL_QUEUE,
        );
        drop(rx);
        Self {
            tx,
            pending: Arc::new(Mutex::new(PendingQueue::new())),
            inflight: Arc::new(Mutex::new(HashMap::new())),
            dirty: Arc::new(Notify::new()),
            events: Arc::new(Mutex::new(None)),
            order_source: Arc::new(JournalOrderSource::for_test(1)),
            panic_shadow: Arc::new(PanicShadow::new()),
        }
    }

    #[cfg(test)]
    pub(crate) fn has_pending_for_test(&self, kind: StoreKind) -> bool {
        lock(&self.pending).contains_key(&kind)
    }

    pub fn save(&self, snapshot: Snapshot) -> crate::util::delivery::DeliveryResult {
        self.queue_snapshot(snapshot).map(|(receipt, _)| receipt)
    }

    /// Queue a snapshot and return its exact acceptance identity for targeted confirmation.
    pub(crate) fn save_tracked(
        &self,
        snapshot: Snapshot,
    ) -> Result<PersistTarget, crate::util::delivery::DeliveryError> {
        self.queue_snapshot(snapshot).map(|(_, target)| target)
    }

    fn queue_snapshot(
        &self,
        snapshot: Snapshot,
    ) -> Result<
        (crate::util::delivery::DeliveryReceipt, PersistTarget),
        crate::util::delivery::DeliveryError,
    > {
        if ensure_persistence_writes_allowed().is_err() {
            return Err(crate::util::delivery::DeliveryError::Closed);
        }
        let mut pending = lock(&self.pending);
        if pending.admission() == SnapshotAdmission::Sealed {
            return Err(crate::util::delivery::DeliveryError::Closed);
        }
        let accepted = self.order_source.accept();
        let operation = PendingOperation::save(snapshot, accepted);
        let target = PersistTarget {
            kind: operation.kind(),
            order: operation.order,
        };
        let operation = publish_pending_operation(&self.panic_shadow, operation)
            .map_err(|PanicShadowSealed| crate::util::delivery::DeliveryError::Closed)?;
        let replaced_existing = pending.insert_owned(operation).is_some();
        drop(pending);
        self.dirty.notify_one();
        let receipt = if replaced_existing {
            crate::util::delivery::DeliveryReceipt::Coalesced {
                replaced_existing: true,
                evicted_oldest: false,
            }
        } else {
            crate::util::delivery::DeliveryReceipt::Enqueued
        };
        Ok((receipt, target))
    }

    /// Atomically close ordinary admission and publish the owner's final snapshots.
    ///
    /// A concurrent save either lands before this boundary and is superseded by the final
    /// snapshot for its store, or observes the seal and cannot create undurable work after the
    /// quit barrier. Ordered fallback writes remain available after sealing.
    pub fn seal_with_snapshots<I>(
        &self,
        snapshots: I,
    ) -> Result<(), crate::util::delivery::DeliveryError>
    where
        I: IntoIterator<Item = Snapshot>,
    {
        let mutation_allowed = ensure_persistence_writes_allowed();
        self.seal_with_snapshots_after_check(snapshots, mutation_allowed)
    }

    pub(super) fn seal_with_snapshots_after_check<I>(
        &self,
        snapshots: I,
        mutation_allowed: std::io::Result<()>,
    ) -> Result<(), crate::util::delivery::DeliveryError>
    where
        I: IntoIterator<Item = Snapshot>,
    {
        self.seal_with_snapshots_after_check_and_hook(snapshots, mutation_allowed, || {})
    }

    fn seal_with_snapshots_after_check_and_hook<I>(
        &self,
        snapshots: I,
        mutation_allowed: std::io::Result<()>,
        after_seal: impl FnOnce(),
    ) -> Result<(), crate::util::delivery::DeliveryError>
    where
        I: IntoIterator<Item = Snapshot>,
    {
        let mut pending = lock(&self.pending);
        pending.seal();
        after_seal();
        if mutation_allowed.is_err() {
            return Err(crate::util::delivery::DeliveryError::Closed);
        }
        let operations: Vec<_> = snapshots
            .into_iter()
            .map(|snapshot| PendingOperation::save(snapshot, self.order_source.accept()))
            .collect();
        let operations = publish_pending_batch(&self.panic_shadow, operations)
            .map_err(|PanicShadowSealed| crate::util::delivery::DeliveryError::Closed)?;
        for operation in operations {
            pending.insert_owned(operation);
        }
        drop(pending);
        self.dirty.notify_one();
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn seal_with_snapshots_after_check_and_hook_for_test<I>(
        &self,
        snapshots: I,
        mutation_allowed: std::io::Result<()>,
        after_seal: impl FnOnce(),
    ) -> Result<(), crate::util::delivery::DeliveryError>
    where
        I: IntoIterator<Item = Snapshot>,
    {
        self.seal_with_snapshots_after_check_and_hook(snapshots, mutation_allowed, after_seal)
    }

    /// Make deletion the latest pending romanize-cache operation. The shared slot is
    /// independent of the bounded control queue, so a saturated queue cannot lose the
    /// intent; a later [`Self::flush`] only acknowledges after the actor applies it.
    pub fn delete_romanized_titles(&self) -> crate::util::delivery::DeliveryResult {
        self.queue_romanized_delete(PendingAction::DeleteRomanizedTitles)
    }

    fn queue_romanized_delete(
        &self,
        action: PendingAction,
    ) -> crate::util::delivery::DeliveryResult {
        if ensure_persistence_writes_allowed().is_err() {
            return Err(crate::util::delivery::DeliveryError::Closed);
        }
        let mut pending = lock(&self.pending);
        if pending.admission() == SnapshotAdmission::Sealed {
            return Err(crate::util::delivery::DeliveryError::Closed);
        }
        let accepted = self.order_source.accept();
        let operation = PendingOperation::new(action, accepted);
        let operation = publish_pending_operation(&self.panic_shadow, operation)
            .map_err(|PanicShadowSealed| crate::util::delivery::DeliveryError::Closed)?;
        let replaced_existing = pending.insert_owned(operation).is_some();
        drop(pending);
        self.dirty.notify_one();
        if replaced_existing {
            Ok(crate::util::delivery::DeliveryReceipt::Coalesced {
                replaced_existing: true,
                evicted_oldest: false,
            })
        } else {
            Ok(crate::util::delivery::DeliveryReceipt::Enqueued)
        }
    }

    #[cfg(test)]
    pub(super) fn delete_romanized_titles_with<F>(&self, deleter: F)
    where
        F: Fn() -> std::io::Result<()> + Send + Sync + 'static,
    {
        let _ = self
            .queue_romanized_delete(PendingAction::TestDeleteRomanizedTitles {
                deleter: Arc::new(deleter),
            })
            .expect("test persistence admission is open");
    }

    /// Drain every pending write, bounded by `budget`. Returns `false` on timeout or when a
    /// failed write remains dirty for retry.
    pub async fn flush(&self, budget: Duration) -> bool {
        let (ack_tx, ack_rx) = oneshot::channel();
        let deadline = tokio::time::Instant::now() + budget;
        match tokio::time::timeout_at(deadline, self.tx.send(PersistMsg::Flush(ack_tx))).await {
            Ok(Ok(())) => {}
            _ => return false,
        }
        match tokio::time::timeout_at(deadline, ack_rx).await {
            Ok(Ok(clean)) => clean,
            _ => false,
        }
    }

    /// Confirm only `target` within `budget`, without letting an unrelated store failure decide
    /// the result. A newer same-store write is reported as supersession, never exact success.
    pub(crate) async fn flush_target(
        &self,
        target: PersistTarget,
        budget: Duration,
    ) -> TargetFlushOutcome {
        let (ack_tx, ack_rx) = oneshot::channel();
        let deadline = tokio::time::Instant::now() + budget;
        let message = PersistMsg::FlushTarget {
            target,
            ack: ack_tx,
        };
        match tokio::time::timeout_at(deadline, self.tx.send(message)).await {
            Ok(Ok(())) => {}
            _ => return TargetFlushOutcome::Unconfirmed,
        }
        match tokio::time::timeout_at(deadline, ack_rx).await {
            Ok(Ok(outcome)) => outcome,
            _ => TargetFlushOutcome::Unconfirmed,
        }
    }

    /// Opaque access to pending save/delete operations for [`install_panic_flush`].
    pub fn pending(&self) -> PanicPending {
        PanicPending {
            #[cfg(test)]
            inner: Arc::clone(&self.pending),
            shadow: Arc::clone(&self.panic_shadow),
        }
    }

    pub fn set_event_sink<F>(&self, emit: F)
    where
        F: Fn(PersistEvent) + Send + Sync + 'static,
    {
        *lock_event_sink(&self.events) = Some(Arc::new(emit));
    }
}
