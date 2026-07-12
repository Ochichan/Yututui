//! Producer-side shutdown linearization for shared owner ingress.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

/// Generation-scoped cancellation for callback producers which retain one exact terminal event.
/// This retires a live platform generation without closing the application-wide owner ingress.
#[derive(Clone, Debug, Default)]
pub(crate) struct CallbackCancellation {
    cancelled: Arc<AtomicBool>,
}

impl CallbackCancellation {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

pub(super) struct IngressAdmission {
    open: Mutex<bool>,
}

impl IngressAdmission {
    pub(super) fn new() -> Self {
        Self {
            open: Mutex::new(true),
        }
    }

    pub(super) fn while_open(&self) -> Option<MutexGuard<'_, bool>> {
        let guard = self
            .open
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *guard { Some(guard) } else { None }
    }

    pub(super) fn close(&self) -> bool {
        let mut open = self
            .open
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let changed = *open;
        *open = false;
        changed
    }
}
