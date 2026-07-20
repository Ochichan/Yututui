//! Queue-admission projections for per-track recommendation provenance.

use super::*;

#[derive(Clone, Copy)]
pub(super) struct RecommendationQueuedCommit {
    pub(super) added: usize,
    pub(super) streaming_refill: bool,
}

impl App {
    /// Load a caller-prepared queue mutation as one admission transaction. Play-now and idle
    /// enqueue use this after inspecting their typed capacity outcome; screen selection,
    /// provenance, and caller-owned romanization remain deferred with the queue and player load.
    pub(in crate::app) fn load_prepared_queue_mutation(
        &mut self,
        mutation: QueueMutationPlan,
        romanize_songs: Vec<Song>,
        accepted_manual_count: usize,
    ) -> Vec<Cmd> {
        let manual_video_ids = romanize_songs
            .iter()
            .take(accepted_manual_count)
            .map(|song| song.video_id.clone())
            .collect();
        self.prepare_queue_mutation_track_transition(
            mutation,
            TrackPostCommit {
                player_mode: true,
                romanize_songs,
                why_gem: Some(crate::app::why_gem::WhyGemCommit::Forget(manual_video_ids)),
                ..TrackPostCommit::default()
            },
        )
    }

    /// Admission-atomic idle enqueue for autoplay and DJ Gem. A rejected player intent changes
    /// neither queue/provenance nor the success toast and streaming circuit-breaker projection.
    pub(in crate::app) fn load_prepared_recommendation_queue_mutation(
        &mut self,
        mutation: QueueMutationPlan,
        romanize_songs: Vec<Song>,
        why_gem: Vec<crate::app::why_gem::WhyGemPick>,
        added: usize,
        streaming_refill: bool,
    ) -> Vec<Cmd> {
        self.prepare_queue_mutation_track_transition(
            mutation,
            TrackPostCommit {
                romanize_songs,
                why_gem: Some(crate::app::why_gem::WhyGemCommit::Upsert(why_gem)),
                recommendation_queued: Some(RecommendationQueuedCommit {
                    added,
                    streaming_refill,
                }),
                ..TrackPostCommit::default()
            },
        )
    }

    pub(super) fn commit_why_gem_post_commit(&mut self, post_commit: &mut TrackPostCommit) {
        if let Some(commit) = post_commit.why_gem.take() {
            match commit {
                crate::app::why_gem::WhyGemCommit::Clear => self.clear_why_gem(),
                crate::app::why_gem::WhyGemCommit::Forget(video_ids) => {
                    self.forget_why_gem_ids(video_ids.iter().map(String::as_str));
                }
                crate::app::why_gem::WhyGemCommit::Replace(picks) => {
                    self.replace_why_gem_picks(&picks);
                }
                crate::app::why_gem::WhyGemCommit::Upsert(picks) => {
                    self.upsert_why_gem_picks(&picks);
                }
            }
        }
        if let Some(commit) = post_commit.recommendation_queued.take() {
            self.commit_recommendation_queue_success(commit.added, commit.streaming_refill);
        }
    }
}
