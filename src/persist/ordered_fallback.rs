use super::*;

#[derive(Debug)]
pub struct PersistenceFallbackError {
    failures: Vec<(StoreKind, String)>,
}

impl PersistenceFallbackError {
    pub fn failures(&self) -> impl Iterator<Item = (StoreKind, &str)> {
        self.failures
            .iter()
            .map(|(store, error)| (*store, error.as_str()))
    }
}

impl std::fmt::Display for PersistenceFallbackError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} persistence fallback(s) failed",
            self.failures.len()
        )?;
        for (store, error) in &self.failures {
            write!(formatter, "; {}: {error}", store.label())?;
        }
        Ok(())
    }
}

impl std::error::Error for PersistenceFallbackError {}

impl PersistHandle {
    /// Retry every newest operation still owned by the panic-time frontier.
    ///
    /// All writes are launched together so a contended store cannot prevent an unrelated store
    /// from reaching its fallback. Successful operations are retired only at the exact journal
    /// order this method observed. Failed or cancelled operations remain in the shadow and in the
    /// actor maps for panic/recovery, preserving ownership when shutdown must report failure.
    pub async fn fallback_newest_owned(&self) -> Result<(), PersistenceFallbackError> {
        ensure_persistence_writes_allowed().map_err(|error| PersistenceFallbackError {
            failures: vec![(StoreKind::Config, error.to_string())],
        })?;
        let operations: Vec<_> = self.panic_shadow.snapshot().into_iter().flatten().collect();
        let writes = operations.into_iter().map(|operation| async move {
            let kind = operation.kind();
            let order = operation.order();
            let result = crate::util::blocking::spawn_io(move || operation.write()).await;
            let result = match result {
                Ok(result) => result,
                Err(error) => Err(std::io::Error::other(format!(
                    "ordered persistence fallback task failed: {error}"
                ))),
            };
            (kind, order, result)
        });

        let mut failures = Vec::new();
        for (kind, order, result) in futures::future::join_all(writes).await {
            match result {
                Ok(()) => {
                    let mut pending = lock(&self.pending);
                    if pending
                        .get(&kind)
                        .is_some_and(|current| current.order == order)
                    {
                        pending.remove(&kind);
                    }
                    drop(pending);
                    remove_inflight_if_order(&self.inflight, kind, order);
                    self.panic_shadow.clear_through(kind, order);
                }
                Err(error) => {
                    failures.push((kind, error.to_string()));
                    self.dirty.notify_one();
                }
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(PersistenceFallbackError { failures })
        }
    }

    /// Persist one fresh fallback snapshot under a newly accepted journal order.
    ///
    /// This is the bounded-flush escape hatch for shutdown paths. Ownership is published to the
    /// same pending/in-flight maps used by the actor before blocking I/O starts, and the write
    /// takes the normal intent lock and journal supersession path. An older actor write therefore
    /// either finishes before this snapshot or observes this snapshot's newer committed frontier
    /// and becomes a no-op. On I/O failure the operation remains actor/panic owned; a concurrent
    /// supersession leaves the strictly newer operation owned instead. The result stays truthful:
    /// `Ok` means this order was written or durably superseded; `Err` means durability was not
    /// confirmed by this call.
    pub async fn save_ordered_fallback(&self, snapshot: Snapshot) -> std::io::Result<()> {
        ensure_persistence_writes_allowed()?;
        let operation = PendingOperation::save(snapshot, self.order_source.accept());
        let kind = operation.kind();
        let order = operation.order;
        let operation = publish_pending_operation(&self.panic_shadow, operation).map_err(
            |PanicShadowSealed| {
                std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "panic-time persistence frontier is sealed",
                )
            },
        )?;
        let prepared = match operation.panic_operation() {
            Ok(prepared) => prepared,
            Err(error) => {
                self.retain_ordered_operation(operation);
                return Err(error);
            }
        };
        match self
            .panic_shadow
            .publish(PanicOwnedOperation::Prepared(Arc::new(prepared.clone())))
        {
            Ok(()) => {}
            Err(PanicShadowSealed) => {
                // The Pending form won publication before the seal and is therefore owned by
                // the hook's one-shot snapshot. Continue the already-admitted ordered write.
            }
        }

        {
            let mut pending = lock(&self.pending);
            if pending
                .get(&kind)
                .is_some_and(|current| current.order > order)
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "ordered persistence fallback was superseded before admission",
                ));
            }
            pending.insert_owned(operation);
        }

        if !retain_newest_inflight(&self.inflight, prepared.clone()) {
            let mut pending = lock(&self.pending);
            if pending
                .get(&kind)
                .is_some_and(|current| current.order == order)
            {
                pending.remove(&kind);
            }
            return Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "ordered persistence fallback was superseded during admission",
            ));
        }
        self.dirty.notify_one();

        let result =
            crate::util::blocking::spawn_io(move || write_panic_operation(&prepared)).await;
        match result {
            Ok(Ok(())) => {
                let mut pending = lock(&self.pending);
                if pending
                    .get(&kind)
                    .is_some_and(|current| current.order == order)
                {
                    pending.remove(&kind);
                }
                drop(pending);
                remove_inflight_if_order(&self.inflight, kind, order);
                self.panic_shadow.clear_through(kind, order);
                Ok(())
            }
            Ok(Err(error)) => {
                self.dirty.notify_one();
                Err(error)
            }
            Err(error) => {
                self.dirty.notify_one();
                Err(std::io::Error::other(format!(
                    "ordered persistence fallback task failed: {error}"
                )))
            }
        }
    }

    fn retain_ordered_operation(&self, operation: ShadowCoveredOperation) {
        let kind = operation.kind();
        let order = operation.order;
        let mut pending = lock(&self.pending);
        if pending
            .get(&kind)
            .is_none_or(|current| current.order <= order)
        {
            pending.insert_owned(operation);
        }
        drop(pending);
        self.dirty.notify_one();
    }
}
