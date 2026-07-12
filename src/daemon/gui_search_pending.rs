//! Owner-lane routing state for in-flight GUI searches.
//!
//! The API actor receives only an opaque correlation id. Live session handles stay here, bounded
//! by the remote session cap, and are released on page replacement, disconnect, completion, or
//! shutdown.

use std::collections::{HashMap, VecDeque};

use crate::api::GuiSearchRequestId;
use crate::remote::{MAX_SESSIONS, RemoteSessionScope};
use crate::search_source::SearchSource;

use super::engine::RequesterKey;

pub(super) struct PendingGuiSearch {
    pub(super) requester_key: RequesterKey,
    pub(super) requester: RemoteSessionScope,
    pub(super) ticket: u64,
    pub(super) query: String,
    pub(super) source: SearchSource,
}

pub(super) struct GuiSearchPending {
    next_epoch: u64,
    next_sequence: u64,
    entries: HashMap<GuiSearchRequestId, PendingGuiSearch>,
    /// Oldest request first, for deterministic bounded eviction in defensive/test scenarios.
    order: VecDeque<GuiSearchRequestId>,
}

impl Default for GuiSearchPending {
    fn default() -> Self {
        Self {
            next_epoch: 0,
            next_sequence: 1,
            entries: HashMap::new(),
            order: VecDeque::new(),
        }
    }
}

impl GuiSearchPending {
    /// Retain the newest request for the session's current page and return its API correlation id.
    /// A session has only one active page generation, so a new page also retires its predecessor.
    pub(super) fn begin(
        &mut self,
        requester_key: RequesterKey,
        requester: RemoteSessionScope,
        ticket: u64,
        query: String,
        source: SearchSource,
    ) -> GuiSearchRequestId {
        self.prune_closed();
        debug_assert_eq!(requester_key.session_id(), requester.session_id());
        debug_assert_eq!(requester_key.page_id(), requester.page_id());
        let session_id = requester_key.session_id();
        self.entries
            .retain(|_, pending| pending.requester_key.session_id() != session_id);
        self.sync_order();

        while self.entries.len() >= MAX_SESSIONS {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.entries.remove(&oldest);
        }

        let request_id = self.allocate_id();
        let replaced = self.entries.insert(
            request_id,
            PendingGuiSearch {
                requester_key,
                requester,
                ticket,
                query,
                source,
            },
        );
        debug_assert!(replaced.is_none());
        self.order.push_back(request_id);
        debug_assert!(self.entries.len() <= MAX_SESSIONS);
        request_id
    }

    /// Consume exactly one known completion. Unknown, replaced, and duplicate ids are inert.
    pub(super) fn take(&mut self, request_id: GuiSearchRequestId) -> Option<PendingGuiSearch> {
        let pending = self.entries.remove(&request_id)?;
        self.order.retain(|queued| *queued != request_id);
        pending.requester.is_live().then_some(pending)
    }

    pub(super) fn prune_closed(&mut self) {
        self.entries
            .retain(|_, pending| pending.requester.is_live());
        self.sync_order();
    }

    /// Explicitly release every live routing handle before daemon shutdown joins producers.
    pub(super) fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
    }

    fn allocate_id(&mut self) -> GuiSearchRequestId {
        loop {
            let candidate = GuiSearchRequestId::new(self.next_epoch, self.next_sequence);
            if self.next_sequence == u64::MAX {
                self.next_sequence = 0;
                self.next_epoch = self.next_epoch.wrapping_add(1);
            } else {
                self.next_sequence += 1;
            }
            if !self.entries.contains_key(&candidate) {
                return candidate;
            }
        }
    }

    fn sync_order(&mut self) {
        self.order
            .retain(|request_id| self.entries.contains_key(request_id));
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    fn set_next_id(&mut self, epoch: u64, sequence: u64) {
        self.next_epoch = epoch;
        self.next_sequence = sequence;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::{SessionTuning, test_register};

    fn begin(
        pending: &mut GuiSearchPending,
        requester: RemoteSessionScope,
        ticket: u64,
    ) -> GuiSearchRequestId {
        let requester_key = RequesterKey::new(
            requester.session_id(),
            requester.page_id().map(str::to_owned),
        );
        pending.begin(
            requester_key,
            requester,
            ticket,
            format!("query-{ticket}"),
            SearchSource::Youtube,
        )
    }

    #[test]
    fn newer_request_retires_same_session_and_old_completion_is_unknown() {
        let mut pending = GuiSearchPending::default();
        let old = begin(
            &mut pending,
            RemoteSessionScope::for_test(7, Some("old-page")),
            1,
        );
        let new = begin(
            &mut pending,
            RemoteSessionScope::for_test(7, Some("new-page")),
            2,
        );

        assert_eq!(pending.len(), 1);
        assert!(pending.take(old).is_none());
        let routed = pending
            .take(new)
            .expect("latest request must remain routed");
        assert_eq!(routed.requester.page_id(), Some("new-page"));
        assert_eq!(routed.ticket, 2);
        assert_eq!(routed.query, "query-2");
    }

    #[test]
    fn table_never_exceeds_live_session_cap() {
        let mut pending = GuiSearchPending::default();
        let mut ids = Vec::new();
        for session_id in 0..=MAX_SESSIONS as u64 {
            ids.push(begin(
                &mut pending,
                RemoteSessionScope::for_test(session_id, Some("page")),
                session_id,
            ));
        }

        assert_eq!(pending.len(), MAX_SESSIONS);
        assert!(pending.take(ids[0]).is_none());
        assert!(pending.take(*ids.last().unwrap()).is_some());
    }

    #[test]
    fn closed_scope_is_pruned_before_completion() {
        let (_hub, session, _rx) = test_register(SessionTuning::default());
        let scope = RemoteSessionScope::new(session.clone(), Some("page".to_owned()));
        let mut pending = GuiSearchPending::default();
        let request_id = begin(&mut pending, scope, 1);

        session.close_for_test();
        pending.prune_closed();

        assert_eq!(pending.len(), 0);
        assert!(pending.take(request_id).is_none());
    }

    #[test]
    fn sequence_wrap_advances_epoch_and_active_collision_is_skipped() {
        let mut pending = GuiSearchPending::default();
        pending.set_next_id(11, u64::MAX);
        let before_wrap = begin(
            &mut pending,
            RemoteSessionScope::for_test(1, Some("page")),
            1,
        );
        let after_wrap = begin(
            &mut pending,
            RemoteSessionScope::for_test(2, Some("page")),
            2,
        );
        assert_eq!(before_wrap.parts(), (11, u64::MAX));
        assert_eq!(after_wrap.parts(), (12, 0));

        pending.set_next_id(11, u64::MAX);
        let skipped = begin(
            &mut pending,
            RemoteSessionScope::for_test(3, Some("page")),
            3,
        );
        assert_eq!(skipped.parts(), (12, 1));
        assert_ne!(skipped, before_wrap);
        assert_ne!(skipped, after_wrap);
    }

    #[test]
    fn clear_releases_all_routing_entries_and_makes_late_answers_inert() {
        let mut pending = GuiSearchPending::default();
        let request_id = begin(
            &mut pending,
            RemoteSessionScope::for_test(1, Some("page")),
            1,
        );

        pending.clear();

        assert_eq!(pending.len(), 0);
        assert!(pending.take(request_id).is_none());
    }
}
