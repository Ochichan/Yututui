use super::*;
use crate::queue::{QueueMutationPlan, QueueRemovalOutcome, QueueRemovalPlayback};

impl App {
    pub(in crate::app) fn apply_queue_removal(
        &mut self,
        mutation: QueueMutationPlan,
        outcome: QueueRemovalOutcome,
        mut post_commit: TrackPostCommit,
        outgoing: Option<bool>,
    ) -> Vec<Cmd> {
        debug_assert!(outcome.removed() > 0);
        post_commit.queue_removal_cursor = Some(outcome.popup_cursor());
        match outcome.playback() {
            QueueRemovalPlayback::Unchanged => {
                self.queue.commit_mutation(mutation);
                if let Some(edit) = post_commit.taste.take() {
                    let _ = self.streaming.taste.apply(edit);
                }
                self.commit_queue_removal_ui(outcome.popup_cursor());
                self.reconcile_why_gem();
                self.dirty = true;
                if post_commit.force_autoplay_extend {
                    self.force_autoplay_extend()
                } else {
                    Vec::new()
                }
            }
            QueueRemovalPlayback::LoadSelected => {
                self.prepare_queue_mutation_track_transition(mutation, post_commit, outgoing)
            }
            QueueRemovalPlayback::Stop => {
                self.prepare_queue_mutation_stop_transition(mutation, post_commit, outgoing)
            }
        }
    }

    fn prepare_queue_mutation_stop_transition(
        &self,
        mutation: QueueMutationPlan,
        post_commit: TrackPostCommit,
        outgoing: Option<bool>,
    ) -> Vec<Cmd> {
        self.track_transition_intent(TrackTransitionPlan {
            expected_queue_rev: self.queue.rev(),
            expected_cursor: self.queue.cursor_pos(),
            expected_video_id: self.queue.current().map(|song| song.video_id.clone()),
            mutation: Some(mutation),
            recorder: None,
            kind: TrackTransitionKind::End {
                target_cursor: None,
            },
            outgoing,
            skipped: Vec::new(),
            status_after_commit: None,
            video_follow_up: None,
            post_commit,
        })
    }

    pub(in crate::app) fn prepare_queue_mutation_track_transition(
        &mut self,
        mut mutation: QueueMutationPlan,
        post_commit: TrackPostCommit,
        outgoing: Option<bool>,
    ) -> Vec<Cmd> {
        let expected_queue_rev = self.queue.rev();
        let expected_cursor = self.queue.cursor_pos();
        let expected_video_id = self.queue.current().map(|song| song.video_id.clone());
        if mutation.is_empty() {
            return self.track_transition_intent(TrackTransitionPlan {
                expected_queue_rev,
                expected_cursor,
                expected_video_id,
                mutation: Some(mutation),
                recorder: None,
                kind: TrackTransitionKind::End {
                    target_cursor: None,
                },
                outgoing,
                skipped: Vec::new(),
                status_after_commit: None,
                video_follow_up: None,
                post_commit,
            });
        }

        let mut selected_load = None;
        let mut skipped = Vec::new();
        let playback = mutation.select_first_playable(|song| {
            let song = song.clone();
            match self.prepare_track_load(song.clone()) {
                Ok(load) => {
                    selected_load = Some(load);
                    true
                }
                Err(reason) => {
                    skipped.push(SkippedCandidate { song, reason });
                    false
                }
            }
        });
        let cursor = mutation.cursor_pos();
        let kind = match playback {
            QueueRemovalPlayback::LoadSelected => TrackTransitionKind::Load {
                cursor: CursorTransition::MoveTo { cursor },
                load: Box::new(selected_load.expect("playable mutation row prepared a load")),
            },
            QueueRemovalPlayback::Stop => TrackTransitionKind::End {
                target_cursor: Some(cursor),
            },
            QueueRemovalPlayback::Unchanged => {
                unreachable!("playable cursor selection cannot leave playback unchanged")
            }
        };
        self.track_transition_intent(TrackTransitionPlan {
            expected_queue_rev,
            expected_cursor,
            expected_video_id,
            mutation: Some(mutation),
            recorder: None,
            kind,
            outgoing,
            skipped,
            status_after_commit: None,
            video_follow_up: None,
            post_commit,
        })
    }
}
