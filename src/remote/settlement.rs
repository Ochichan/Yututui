//! Wire-level lifecycle barrier for owner-accepted remote requests.
//!
//! A request is not settled when the owner sends its oneshot response: one-shot connections still
//! have to serialize/flush it, and session replies still have to cross the outbound writer. Each
//! accepted request owns one token until that final write completes (or the peer/write fails).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Notify;

#[derive(Default)]
struct SettlementState {
    active: usize,
}

#[derive(Clone, Default)]
pub(crate) struct WireSettlements {
    inner: Arc<WireSettlementsInner>,
}

#[derive(Default)]
struct WireSettlementsInner {
    state: Mutex<SettlementState>,
    idle: Notify,
}

#[must_use = "an accepted remote request must retain its wire-settlement token until flush"]
pub struct WireSettlement {
    inner: Option<Arc<WireSettlementsInner>>,
}

impl WireSettlements {
    /// Must be called while the hub's owner-admission lock is held. That outer lock is what makes
    /// token creation and the monotonic quiesce transition one total order.
    pub(crate) fn begin(&self) -> WireSettlement {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.active = state
            .active
            .checked_add(1)
            .expect("bounded remote settlement count cannot overflow");
        WireSettlement {
            inner: Some(Arc::clone(&self.inner)),
        }
    }

    pub(crate) fn active(&self) -> usize {
        self.inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .active
    }

    pub(crate) async fn wait_for_idle(&self, budget: Duration) -> bool {
        tokio::time::timeout(budget, async {
            loop {
                let notified = self.inner.idle.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if self.active() == 0 {
                    return;
                }
                notified.await;
            }
        })
        .await
        .is_ok()
    }
}

impl Drop for WireSettlement {
    fn drop(&mut self) {
        let Some(inner) = self.inner.take() else {
            return;
        };
        let became_idle = {
            let mut state = inner
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.active = state
                .active
                .checked_sub(1)
                .expect("remote settlement token retired exactly once");
            state.active == 0
        };
        if became_idle {
            inner.idle.notify_waiters();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn idle_wait_covers_every_live_token_without_losing_the_final_wakeup() {
        let settlements = WireSettlements::default();
        let first = settlements.begin();
        let second = settlements.begin();
        assert_eq!(settlements.active(), 2);

        let waiter = tokio::spawn({
            let settlements = settlements.clone();
            async move { settlements.wait_for_idle(Duration::from_millis(200)).await }
        });
        tokio::task::yield_now().await;
        drop(first);
        assert_eq!(settlements.active(), 1);
        drop(second);

        assert!(waiter.await.unwrap());
        assert_eq!(settlements.active(), 0);
    }
}
