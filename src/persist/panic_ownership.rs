use super::*;

impl PendingOperation {
    pub(super) fn panic_operation(&self) -> std::io::Result<PanicOperation> {
        self.ensure_ordering()?;
        let action = match self.action.as_ref() {
            PendingAction::Save(snapshot) => {
                #[cfg(test)]
                if let OwnedSnapshot::Test {
                    storage_path: None,
                    writer,
                    ..
                } = &**snapshot
                {
                    return Ok(PanicOperation {
                        order: self.order,
                        kind: self.kind(),
                        label: self.label(),
                        action: PanicAction::Direct(Arc::clone(writer)),
                    });
                }
                match snapshot.storage_path() {
                    Some(path) => PanicAction::Replace {
                        path,
                        bytes: snapshot.to_json_bytes().map_err(std::io::Error::other)?,
                    },
                    None => PanicAction::Noop,
                }
            }
            PendingAction::DeleteRomanizedTitles => match crate::romanize::cache_path() {
                Some(path) => PanicAction::Delete { path },
                None => PanicAction::Noop,
            },
            #[cfg(test)]
            PendingAction::TestDeleteRomanizedTitles { deleter } => {
                PanicAction::Direct(Arc::clone(deleter))
            }
        };
        Ok(PanicOperation {
            order: self.order,
            kind: self.kind(),
            label: self.label(),
            action,
        })
    }
}

#[derive(Clone)]
enum PanicAction {
    Replace {
        path: PathBuf,
        bytes: Vec<u8>,
    },
    Delete {
        path: PathBuf,
    },
    #[cfg(test)]
    Direct(Arc<dyn Fn() -> std::io::Result<()> + Send + Sync>),
    Noop,
}

#[derive(Clone)]
pub(super) struct PanicOperation {
    pub(super) order: JournalOrder,
    kind: StoreKind,
    label: &'static str,
    action: PanicAction,
}

#[derive(Clone)]
pub(super) enum PanicOwnedOperation {
    Pending(Arc<PendingOperation>),
    Prepared(Arc<PanicOperation>),
}

impl PanicOwnedOperation {
    pub(super) fn order(&self) -> JournalOrder {
        match self {
            Self::Pending(operation) => operation.order,
            Self::Prepared(operation) => operation.order,
        }
    }

    pub(super) fn kind(&self) -> StoreKind {
        match self {
            Self::Pending(operation) => operation.kind(),
            Self::Prepared(operation) => operation.kind,
        }
    }

    pub(super) fn write(&self) -> std::io::Result<()> {
        match self {
            Self::Pending(operation) => write_operation_caught(operation),
            Self::Prepared(operation) => write_panic_operation(operation),
        }
    }

    pub(super) fn priority(&self) -> u8 {
        match self {
            Self::Pending(_) => 0,
            Self::Prepared(_) => 1,
        }
    }
}

/// Wrap the current panic hook so pending operations hit disk before the inherited chain.
pub fn install_panic_flush(pending: PanicPending) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        match pending.shadow.seal_and_snapshot() {
            Ok(snapshot) => {
                for operation in snapshot.into_iter().flatten() {
                    let _ = operation.write();
                }
            }
            Err(PanicShadowSealed) => {
                // A concurrent or nested hook owns the one-shot persistence frontier.
            }
        }
        previous(info);
    }));
}

pub(super) fn retain_newest_inflight(inflight: &SharedInflight, operation: PanicOperation) -> bool {
    let kind = operation.kind;
    let mut map = lock_inflight(inflight);
    match map.entry(kind) {
        std::collections::hash_map::Entry::Occupied(mut entry) => {
            if entry.get().order > operation.order {
                false
            } else {
                if entry.get().order < operation.order {
                    entry.insert(operation);
                }
                true
            }
        }
        std::collections::hash_map::Entry::Vacant(entry) => {
            entry.insert(operation);
            true
        }
    }
}

pub(super) fn write_panic_operation(operation: &PanicOperation) -> std::io::Result<()> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match &operation.action {
        PanicAction::Replace { path, bytes } => {
            ensure_persistence_writes_allowed()?;
            let _lock = acquire_intent_lock(path)?;
            let intent = JournalIntent::Replace {
                order: operation.order,
                kind: operation.kind,
                path: path.clone(),
                bytes: bytes.clone(),
            };
            let record = prepare_journal_record(&intent)?;
            let state = replace_journal_with_record_locked(operation.kind, path, &record)?;
            if verify_intent_state(&state, operation.order)? == IntentState::Superseded {
                return Ok(());
            }
            crate::util::safe_fs::write_private_atomic(path, bytes)?;
            commit_journal_generation_locked(operation.kind, path, operation.order)
        }
        PanicAction::Delete { path } => {
            ensure_persistence_writes_allowed()?;
            let _lock = acquire_intent_lock(path)?;
            let intent = JournalIntent::Delete {
                order: operation.order,
                kind: operation.kind,
                path: path.clone(),
            };
            let record = prepare_journal_record(&intent)?;
            let state = replace_journal_with_record_locked(operation.kind, path, &record)?;
            if verify_intent_state(&state, operation.order)? == IntentState::Superseded {
                return Ok(());
            }
            // Do not publish a committed delete frontier until the parent directory has crossed
            // the same durability boundary as an ordinary actor deletion.
            let _existed = super::remove_store_file(path)?;
            commit_journal_generation_locked(operation.kind, path, operation.order)
        }
        #[cfg(test)]
        PanicAction::Direct(writer) => {
            ensure_persistence_writes_allowed()?;
            writer()
        }
        PanicAction::Noop => ensure_persistence_writes_allowed(),
    }))
    .unwrap_or_else(|_| {
        Err(std::io::Error::other(format!(
            "panic-time {} writer panicked",
            operation.label
        )))
    })
}

pub(super) fn lock_inflight(
    inflight: &SharedInflight,
) -> std::sync::MutexGuard<'_, HashMap<StoreKind, PanicOperation>> {
    inflight.lock().unwrap_or_else(PoisonError::into_inner)
}

pub(super) fn remove_inflight_if_order(
    inflight: &SharedInflight,
    kind: StoreKind,
    order: JournalOrder,
) {
    let mut guard = lock_inflight(inflight);
    if guard
        .get(&kind)
        .is_some_and(|operation| operation.order == order)
    {
        guard.remove(&kind);
    }
}
