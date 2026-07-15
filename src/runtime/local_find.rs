//! Latest-wins runtime workers for the pure in-memory Local Find engine.

use std::sync::atomic::Ordering;

use super::{Msg, RuntimeEvent, RuntimeHandles};

impl RuntimeHandles {
    pub(super) fn cancel_local_find_queries(&self) {
        self.local_find_query_epoch.fetch_add(1, Ordering::AcqRel);
    }

    pub(super) fn dispatch_local_find_build(
        &mut self,
        generation: u64,
        tracks: Vec<crate::local::LocalTrack>,
        playlists: Vec<crate::local::find::LocalFindPlaylistInput>,
        revision: crate::local::find::LocalFindCorpusRevision,
        options: crate::local::find::LocalFindCorpusOptions,
    ) {
        self.cancel_local_find_queries();
        let emitter = self.background_tasks.emitter(self.worker_tx.clone());
        self.background_tasks
            .spawn_blocking("local_find_build", move || {
                let corpus = crate::local::find::LocalFindCorpus::build(
                    &tracks, &playlists, revision, &options,
                );
                emitter.emit_terminal_blocking(RuntimeEvent::App(Msg::Local(
                    crate::app::LocalMsg::FindCorpusReady {
                        generation,
                        corpus: std::sync::Arc::new(corpus),
                    },
                )));
            });
    }

    pub(super) fn dispatch_local_find_query(
        &mut self,
        request_id: u64,
        generation: u64,
        corpus: std::sync::Arc<crate::local::find::LocalFindCorpus>,
        query: crate::local::find::LocalFindQuery,
        scope: crate::local::find::LocalFindScope,
        sort: crate::local::find::LocalFindSort,
    ) {
        let query_epoch = self
            .local_find_query_epoch
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);
        let latest_query_epoch = std::sync::Arc::clone(&self.local_find_query_epoch);
        let emitter = self.background_tasks.emitter(self.worker_tx.clone());
        self.background_tasks
            .spawn_blocking("local_find_query", move || {
                // Row/action generations advance per query request, while `generation` below
                // still guards the corpus build that owned this evaluation.
                let Some(snapshot) =
                    corpus.search_cancellable(&query, scope, sort, request_id, || {
                        latest_query_epoch.load(Ordering::Acquire) != query_epoch
                    })
                else {
                    return;
                };
                emitter.emit_terminal_blocking(RuntimeEvent::App(Msg::Local(
                    crate::app::LocalMsg::FindResultsReady {
                        request_id,
                        generation,
                        snapshot,
                    },
                )));
            });
    }
}
