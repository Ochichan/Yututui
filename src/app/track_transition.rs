//! Admission-atomic queue cursor moves and track loads.
//!
//! A transition is prepared from immutable queue/player state, admitted as one ordered mpv
//! batch, and only then committed to the reducer. This keeps queue position, listening signals,
//! history, playback epochs, art/lyrics, recorder state, persistence, and correlated remote
//! replies aligned with the command lane's authoritative acceptance result.

use super::*;
use crate::queue::{QueueMutationPlan, QueueRemovalPlayback, QueueReplacementDraft};

mod why_gem;

#[derive(Clone, Copy)]
enum TrackMove {
    Next { auto: bool },
    Previous,
    Stay,
    MoveTo { cursor: usize },
}

#[derive(Clone, Copy)]
enum CursorTransition {
    Stay,
    MoveTo { cursor: usize },
}

impl CursorTransition {
    fn from_cursors(expected: usize, target: usize) -> Self {
        if expected == target {
            Self::Stay
        } else {
            Self::MoveTo { cursor: target }
        }
    }

    fn target(self, expected: usize) -> usize {
        match self {
            Self::Stay => expected,
            Self::MoveTo { cursor } => cursor,
        }
    }
}

#[derive(Clone, Default)]
struct TrackPostCommit {
    close_queue_popup: bool,
    queue_removal_cursor: Option<usize>,
    force_autoplay_extend: bool,
    player_mode: bool,
    romanize_songs: Vec<Song>,
    persist_playback_modes: bool,
    clear_heal_video_id: Option<String>,
    mode_switch: Option<super::mode_transition::ModeSwitchPlan>,
    why_gem: Option<super::why_gem::WhyGemCommit>,
    recommendation_queued: Option<why_gem::RecommendationQueuedCommit>,
}

/// Caller-owned reducer projections which become valid only with an accepted queue replacement.
/// Keeping these effects in the track plan prevents a rejected load from changing screens,
/// consuming a romanization request id, or persisting a queue mode mpv never received.
#[derive(Clone, Default)]
pub(in crate::app) struct QueueReplacementOptions {
    pub(in crate::app) player_mode: bool,
    pub(in crate::app) romanize_all: bool,
    pub(in crate::app) persist_playback_modes: bool,
    pub(in crate::app) force_autoplay_extend: bool,
    /// `None` clears all provenance with the replacement. Recommendation-owned replacements
    /// provide exact per-video models and install them only after player admission.
    pub(in crate::app) why_gem: Option<Vec<super::why_gem::WhyGemPick>>,
}

#[derive(Clone)]
pub(in crate::app) struct PreparedTrackLoad {
    pub(in crate::app) song: Song,
    pub(in crate::app) url: String,
    pub(in crate::app) prefetched_url: Option<String>,
    pub(in crate::app) invalid_prefetch: Option<(String, String)>,
}

#[derive(Clone)]
pub(in crate::app) struct SkippedCandidate {
    pub(in crate::app) song: Song,
    pub(in crate::app) reason: SkippedReason,
}

#[derive(Clone)]
pub(in crate::app) enum SkippedReason {
    UnplayableYoutube(String),
    InvalidUrl(String),
}

#[derive(Clone)]
enum TrackTransitionKind {
    Load {
        cursor: CursorTransition,
        load: Box<PreparedTrackLoad>,
    },
    End {
        /// `Some` when preparation exhausted invalid candidates and should leave the cursor on
        /// the last one examined. A normal end-of-queue keeps the current cursor unchanged.
        target_cursor: Option<usize>,
    },
}

#[derive(Clone)]
enum VideoFollowUp {
    Continue { status: String, video_url: String },
    AudioFallback,
    QueueEnded,
}

/// Reducer state that becomes valid only after the complete track command batch is accepted.
#[derive(Clone)]
pub struct TrackTransitionPlan {
    expected_queue_rev: u64,
    expected_cursor: usize,
    expected_video_id: Option<String>,
    mutation: Option<QueueMutationPlan>,
    recorder: Option<crate::recorder::RecorderTransitionPlan>,
    kind: TrackTransitionKind,
    outgoing: Option<bool>,
    skipped: Vec<SkippedCandidate>,
    status_after_commit: Option<(StatusKind, String)>,
    video_follow_up: Option<VideoFollowUp>,
    post_commit: TrackPostCommit,
}

impl TrackTransitionPlan {
    fn target_song(&self) -> Option<&Song> {
        match &self.kind {
            TrackTransitionKind::Load { load, .. } => Some(&load.song),
            TrackTransitionKind::End { .. } => None,
        }
    }

    fn is_load(&self) -> bool {
        matches!(self.kind, TrackTransitionKind::Load { .. })
    }
}

impl App {
    /// Verify every state guard used by [`Self::commit_track_transition`] without mutating or
    /// panicking. Deferred startup/restart work crosses owner turns, so this check must happen
    /// before its command batch is admitted to mpv.
    pub(crate) fn track_transition_is_current(&self, plan: &TrackTransitionPlan) -> bool {
        let target_cursor = match &plan.kind {
            TrackTransitionKind::Load { cursor, .. } => Some(cursor.target(plan.expected_cursor)),
            TrackTransitionKind::End { target_cursor } => *target_cursor,
        };
        let queue_matches = plan.mutation.as_ref().map_or_else(
            || {
                self.queue.planned_transition_matches(
                    plan.expected_queue_rev,
                    plan.expected_cursor,
                    plan.expected_video_id.as_deref(),
                    target_cursor,
                )
            },
            |mutation| self.queue.mutation_matches(mutation),
        );
        queue_matches
            && plan
                .post_commit
                .clear_heal_video_id
                .as_deref()
                .is_none_or(|video_id| self.heal.pending_video_id.as_deref() == Some(video_id))
            && plan
                .post_commit
                .mode_switch
                .as_ref()
                .is_none_or(|mode| self.mode_switch_is_current(mode))
            && plan
                .recorder
                .as_ref()
                .is_some_and(|recorder| self.recorder_transition_is_current(recorder))
    }

    /// Release only the token-scoped Local intent/continuation owned by this rejected batch.
    pub(crate) fn reject_track_transition(&mut self, plan: &TrackTransitionPlan) {
        if plan
            .post_commit
            .recommendation_queued
            .as_ref()
            .is_some_and(|commit| commit.streaming_refill)
        {
            self.cancel_pending_streaming_recommendation();
        }
        let Some(mode_switch) = plan.post_commit.mode_switch.as_ref() else {
            return;
        };
        if let Some(token) = mode_switch.local_intent_token()
            && self.local_mode.pending_intent_token == Some(token)
        {
            self.local_mode.pending_intent_token = None;
        }
        if let Some(token) = mode_switch.local_import_search_confirmation_token()
            && self
                .local_mode
                .pending_import_search
                .as_ref()
                .is_some_and(|pending| pending.confirmation_token == token)
        {
            self.local_mode.pending_import_search = None;
        }
    }

    /// Move forward without recording an outgoing preference signal. Playback-error and
    /// late-streaming recovery paths use this: a track that failed to play is not a dislike.
    pub(in crate::app) fn advance(&mut self, auto: bool) -> Vec<Cmd> {
        self.prepare_track_transition(TrackMove::Next { auto }, None, TrackPostCommit::default())
    }

    /// Move forward and defer the just-finished/just-skipped signal into the same accepted
    /// commit as the new load.
    pub(in crate::app) fn advance_with_outgoing(&mut self, auto: bool, full: bool) -> Vec<Cmd> {
        self.prepare_track_transition(
            TrackMove::Next { auto },
            Some(full),
            TrackPostCommit::default(),
        )
    }

    pub(in crate::app) fn previous_track(&mut self) -> Vec<Cmd> {
        self.prepare_track_transition(TrackMove::Previous, None, TrackPostCommit::default())
    }

    /// Reload the current queue entry without projecting any load-side state until the player
    /// batch is admitted. This is the cold play/resume primitive.
    pub(in crate::app) fn stay_on_current_track(&mut self) -> Vec<Cmd> {
        self.prepare_track_transition(TrackMove::Stay, None, TrackPostCommit::default())
    }

    /// Resume the current queue entry and preserve the historical forced streaming refill, but
    /// defer that refill until the load itself has been accepted.
    pub(in crate::app) fn resume_current_track(&mut self) -> Vec<Cmd> {
        self.prepare_track_transition(
            TrackMove::Stay,
            None,
            TrackPostCommit {
                force_autoplay_extend: self.autoplay_streaming,
                ..TrackPostCommit::default()
            },
        )
    }

    /// Reload a self-healed track from its freshly resolved direct URL, retaining the pending
    /// heal marker until the complete track batch is admitted.
    pub(in crate::app) fn reload_healed_track(&mut self, video_id: String) -> Vec<Cmd> {
        self.prepare_track_transition(
            TrackMove::Stay,
            None,
            TrackPostCommit {
                clear_heal_video_id: Some(video_id),
                ..TrackPostCommit::default()
            },
        )
    }

    /// Jump to an existing queue order position. Cursor and popup state are committed together
    /// only after the selected load batch is admitted.
    pub(in crate::app) fn move_to_queue_track(&mut self, cursor: usize) -> Vec<Cmd> {
        self.prepare_track_transition(
            TrackMove::MoveTo { cursor },
            None,
            TrackPostCommit {
                close_queue_popup: true,
                ..TrackPostCommit::default()
            },
        )
    }

    fn prepare_track_transition(
        &mut self,
        movement: TrackMove,
        outgoing: Option<bool>,
        post_commit: TrackPostCommit,
    ) -> Vec<Cmd> {
        let expected_queue_rev = self.queue.rev();
        let expected_cursor = self.queue.cursor_pos();
        let expected_video_id = self.queue.current().map(|song| song.video_id.clone());
        let first_cursor = match movement {
            TrackMove::Next { auto } => self.queue.plan_next_cursor(expected_cursor, auto),
            TrackMove::Previous => self.queue.plan_prev_cursor(expected_cursor),
            TrackMove::Stay => self
                .queue
                .song_at_cursor(expected_cursor)
                .map(|_| expected_cursor),
            TrackMove::MoveTo { cursor } => self.queue.plan_goto_cursor(cursor),
        };

        let Some(mut cursor) = first_cursor else {
            if matches!(movement, TrackMove::Stay | TrackMove::MoveTo { .. }) {
                return Vec::new();
            }
            return self.track_transition_intent(TrackTransitionPlan {
                expected_queue_rev,
                expected_cursor,
                expected_video_id,
                mutation: None,
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
        };

        let mut skipped = Vec::new();
        let mut last_cursor = cursor;
        // Exactly one pass: repeat-all may wrap, but an all-invalid queue must terminate.
        for _ in 0..self.queue.len() {
            let Some(song) = self.queue.song_at_cursor(cursor).cloned() else {
                break;
            };
            match self.prepare_track_load(song.clone()) {
                Ok(load) => {
                    return self.track_transition_intent(TrackTransitionPlan {
                        expected_queue_rev,
                        expected_cursor,
                        expected_video_id,
                        mutation: None,
                        recorder: None,
                        kind: TrackTransitionKind::Load {
                            cursor: CursorTransition::from_cursors(expected_cursor, cursor),
                            load: Box::new(load),
                        },
                        outgoing,
                        skipped,
                        status_after_commit: None,
                        video_follow_up: None,
                        post_commit,
                    });
                }
                Err(reason) => skipped.push(SkippedCandidate { song, reason }),
            }
            last_cursor = cursor;
            let Some(next) = self.queue.plan_next_cursor(cursor, false) else {
                break;
            };
            cursor = next;
        }

        self.track_transition_intent(TrackTransitionPlan {
            expected_queue_rev,
            expected_cursor,
            expected_video_id,
            mutation: None,
            recorder: None,
            kind: TrackTransitionKind::End {
                target_cursor: Some(last_cursor),
            },
            outgoing,
            skipped,
            status_after_commit: None,
            video_follow_up: None,
            post_commit,
        })
    }

    /// Replace the queue and load its selected entry as one admission transaction. Preparation
    /// advances only a cloned queue RNG; every live projection is deferred to the accepted
    /// [`PlayerCommit::Track`].
    pub(in crate::app) fn replace_queue_and_load(
        &mut self,
        songs: Vec<Song>,
        start: usize,
        shuffle_override: Option<bool>,
        options: QueueReplacementOptions,
    ) -> Vec<Cmd> {
        let romanize_songs = if options.romanize_all {
            songs.clone()
        } else {
            Vec::new()
        };
        let mutation = self.queue.prepare_replacement(QueueReplacementDraft::new(
            songs,
            start,
            shuffle_override,
        ));
        self.prepare_queue_mutation_track_transition(
            mutation,
            TrackPostCommit {
                force_autoplay_extend: options.force_autoplay_extend,
                player_mode: options.player_mode,
                romanize_songs,
                persist_playback_modes: options.persist_playback_modes,
                why_gem: Some(options.why_gem.map_or(
                    super::why_gem::WhyGemCommit::Clear,
                    super::why_gem::WhyGemCommit::Replace,
                )),
                ..TrackPostCommit::default()
            },
        )
    }

    /// Swap a cached dedicated-mode queue and its active track under one player admission.
    /// A non-empty restore preserves the historical Stop-before-Load barrier; empty/all-invalid
    /// restores already carry their single Stop from the ordinary track transition.
    pub(in crate::app) fn load_mode_switch_queue(
        &mut self,
        mutation: QueueMutationPlan,
        mode_switch: super::mode_transition::ModeSwitchPlan,
    ) -> Vec<Cmd> {
        let release_video_pause = mode_switch.releases_video_pause();
        let mut cmds = self.prepare_queue_mutation_track_transition(
            mutation,
            TrackPostCommit {
                mode_switch: Some(mode_switch),
                why_gem: Some(super::why_gem::WhyGemCommit::Clear),
                ..TrackPostCommit::default()
            },
        );
        let intent = cmds.iter_mut().find_map(|cmd| match cmd {
            Cmd::PlayerControl(PlayerControl::Intent(intent)) => Some(intent),
            _ => None,
        });
        let intent = intent.expect("mode switch must produce one track intent");
        intent.label = "mode_switch";
        if let Some(load_at) = intent
            .commands
            .iter()
            .position(|command| matches!(command, PlayerCmd::Load(_)))
        {
            intent.commands.insert(load_at, PlayerCmd::Stop);
        }
        if release_video_pause {
            // mpv's pause property survives Stop/loadfile. Releasing overlay ownership must
            // therefore be part of the same admitted transaction as the mode switch.
            intent.commands.push(PlayerCmd::SetProperty {
                name: "pause".to_owned(),
                value: serde_json::Value::Bool(false),
            });
        }
        cmds
    }

    /// Apply a current-inclusive queue removal only after its Load/Stop batch is admitted.
    /// The caller handles `Unchanged` removals synchronously because they emit no player work.
    pub(in crate::app) fn load_prepared_queue_removal(
        &mut self,
        mutation: QueueMutationPlan,
        playback: QueueRemovalPlayback,
        popup_cursor: usize,
    ) -> Vec<Cmd> {
        let post_commit = TrackPostCommit {
            queue_removal_cursor: Some(popup_cursor),
            ..TrackPostCommit::default()
        };
        match playback {
            QueueRemovalPlayback::Unchanged => {
                unreachable!("non-current queue removal does not need player admission")
            }
            QueueRemovalPlayback::LoadSelected => {
                self.prepare_queue_mutation_track_transition(mutation, post_commit)
            }
            QueueRemovalPlayback::Stop => {
                self.prepare_queue_mutation_stop_transition(mutation, post_commit)
            }
        }
    }

    fn prepare_queue_mutation_stop_transition(
        &self,
        mutation: QueueMutationPlan,
        post_commit: TrackPostCommit,
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
            outgoing: None,
            skipped: Vec::new(),
            status_after_commit: None,
            video_follow_up: None,
            post_commit,
        })
    }

    fn prepare_queue_mutation_track_transition(
        &mut self,
        mut mutation: QueueMutationPlan,
        post_commit: TrackPostCommit,
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
                outgoing: None,
                skipped: Vec::new(),
                status_after_commit: None,
                video_follow_up: None,
                post_commit,
            });
        }

        let mut cursor = mutation.cursor_pos();
        let mut last_cursor = cursor;
        let mut skipped = Vec::new();
        for _ in 0..mutation.len() {
            let Some(song) = mutation.song_at_cursor(cursor).cloned() else {
                break;
            };
            match self.prepare_track_load(song.clone()) {
                Ok(load) => {
                    mutation.select_cursor(cursor);
                    return self.track_transition_intent(TrackTransitionPlan {
                        expected_queue_rev,
                        expected_cursor,
                        expected_video_id,
                        mutation: Some(mutation),
                        recorder: None,
                        kind: TrackTransitionKind::Load {
                            cursor: CursorTransition::MoveTo { cursor },
                            load: Box::new(load),
                        },
                        outgoing: None,
                        skipped,
                        status_after_commit: None,
                        video_follow_up: None,
                        post_commit,
                    });
                }
                Err(reason) => skipped.push(SkippedCandidate { song, reason }),
            }
            last_cursor = cursor;
            let Some(next) = mutation.plan_next_cursor(cursor) else {
                break;
            };
            cursor = next;
        }
        mutation.select_cursor(last_cursor);
        self.track_transition_intent(TrackTransitionPlan {
            expected_queue_rev,
            expected_cursor,
            expected_video_id,
            mutation: Some(mutation),
            recorder: None,
            kind: TrackTransitionKind::End {
                target_cursor: Some(last_cursor),
            },
            outgoing: None,
            skipped,
            status_after_commit: None,
            video_follow_up: None,
            post_commit,
        })
    }

    fn track_transition_intent(&self, mut plan: TrackTransitionPlan) -> Vec<Cmd> {
        let recorder = self.prepare_recorder_teardown();
        let mut commands = Vec::with_capacity(4);
        commands.extend(self.recorder_transition_commands(&recorder));
        debug_assert!(plan.recorder.is_none());
        plan.recorder = Some(recorder);
        match &plan.kind {
            TrackTransitionKind::Load { load, .. } => {
                commands.push(PlayerCmd::load(
                    load.url.clone(),
                    crate::player::MediaSourceContext::from_live(load.song.is_radio_station()),
                ));
                if let Some(af) = self.track_audio_filter() {
                    commands.push(PlayerCmd::SetAudioFilter(af));
                }
            }
            TrackTransitionKind::End { .. } => commands.push(PlayerCmd::Stop),
        }
        vec![Cmd::PlayerControl(PlayerControl::Intent(Box::new(
            PlayerIntent::batch(
                "track_transition",
                commands,
                PlayerCommit::Track(Box::new(plan)),
            ),
        )))]
    }

    /// Defer the video-overlay side of a queue move until the audio load has committed. A real
    /// video continuation also adds its pause command to the same ordered player batch, after
    /// recorder-clear → Load → AF.
    pub(in crate::app) fn attach_video_track_follow_up(
        &mut self,
        cmds: &mut [Cmd],
        status: &str,
    ) -> bool {
        let target = cmds.iter().find_map(|cmd| match cmd {
            Cmd::PlayerControl(PlayerControl::Intent(intent)) => match &intent.commit {
                PlayerCommit::Track(plan) => plan.target_song().cloned(),
                _ => None,
            },
            _ => None,
        });
        let video_url = target.as_ref().and_then(|song| {
            self.recover_youtube_id(song)
                .map(|id| format!("https://www.youtube.com/watch?v={id}"))
        });

        let Some(intent) = cmds.iter_mut().find_map(|cmd| match cmd {
            Cmd::PlayerControl(PlayerControl::Intent(intent))
                if matches!(intent.commit, PlayerCommit::Track(_)) =>
            {
                Some(intent)
            }
            _ => None,
        }) else {
            return false;
        };
        let PlayerCommit::Track(plan) = &mut intent.commit else {
            unreachable!("track intent changed after matching")
        };
        plan.video_follow_up = Some(if !plan.is_load() {
            VideoFollowUp::QueueEnded
        } else if let Some(video_url) = video_url {
            intent.commands.push(PlayerCmd::SetProperty {
                name: "pause".to_owned(),
                value: serde_json::Value::Bool(true),
            });
            VideoFollowUp::Continue {
                status: status.to_owned(),
                video_url,
            }
        } else {
            VideoFollowUp::AudioFallback
        });
        true
    }

    /// Preserve a caller-specific post-load status (for example, the categorized reason an
    /// automatic playback-error skip occurred) without committing it before admission.
    pub(in crate::app) fn attach_track_commit_status(
        cmds: &mut [Cmd],
        kind: StatusKind,
        text: String,
    ) {
        if let Some(plan) = cmds.iter_mut().find_map(|cmd| match cmd {
            Cmd::PlayerControl(PlayerControl::Intent(intent)) => match &mut intent.commit {
                PlayerCommit::Track(plan) => Some(plan),
                _ => None,
            },
            _ => None,
        }) {
            plan.status_after_commit = Some((kind, text));
        }
    }

    pub(in crate::app) fn commit_track_transition(
        &mut self,
        plan: TrackTransitionPlan,
    ) -> Vec<Cmd> {
        let TrackTransitionPlan {
            expected_queue_rev,
            expected_cursor,
            expected_video_id,
            mut mutation,
            recorder,
            kind,
            outgoing,
            skipped,
            status_after_commit,
            video_follow_up,
            mut post_commit,
        } = plan;

        let target_cursor = match &kind {
            TrackTransitionKind::Load { cursor, .. } => Some(cursor.target(expected_cursor)),
            TrackTransitionKind::End { target_cursor } => *target_cursor,
        };
        // Establish the complete stale-plan guard before *any* reducer mutation. In particular,
        // `record_outgoing` updates listening signals/session state for the old current track and
        // must never run for a plan prepared against a queue that has since changed.
        if let Some(plan) = mutation.as_ref() {
            self.queue.validate_mutation(plan);
        } else {
            self.queue.validate_planned_transition(
                expected_queue_rev,
                expected_cursor,
                expected_video_id.as_deref(),
                target_cursor,
            );
        }
        if let Some(video_id) = post_commit.clear_heal_video_id.as_deref() {
            assert_eq!(
                self.heal.pending_video_id.as_deref(),
                Some(video_id),
                "self-heal marker changed before track-load commit"
            );
        }
        if let Some(plan) = post_commit.mode_switch.as_ref() {
            self.validate_mode_switch(plan);
        }
        let recorder = recorder.expect("track transition must carry recorder teardown");
        self.validate_recorder_transition(&recorder);
        let mut effects = outgoing
            .map(|full| self.record_outgoing(full))
            .unwrap_or_default();
        effects.extend(self.commit_recorder_transition(recorder));
        if let Some(plan) = post_commit.mode_switch.as_ref() {
            self.commit_mode_switch_before_track(plan);
        }

        match kind {
            TrackTransitionKind::Load { cursor, load } => {
                let target_cursor = cursor.target(expected_cursor);
                if let Some(plan) = mutation.take() {
                    debug_assert_eq!(plan.cursor_pos(), target_cursor);
                    self.queue.commit_mutation(plan);
                } else {
                    self.queue.commit_planned_cursor(
                        expected_queue_rev,
                        expected_cursor,
                        expected_video_id.as_deref(),
                        target_cursor,
                    );
                }
                self.log_skipped_candidates(&skipped);
                effects.extend(self.commit_prepared_track_load(*load));
            }
            TrackTransitionKind::End { target_cursor } => {
                if let Some(plan) = mutation.take() {
                    self.queue.commit_mutation(plan);
                } else if let Some(target_cursor) = target_cursor {
                    self.queue.commit_planned_cursor(
                        expected_queue_rev,
                        expected_cursor,
                        expected_video_id.as_deref(),
                        target_cursor,
                    );
                }
                self.log_skipped_candidates(&skipped);
                effects.extend(self.commit_playback_cleared());
                if !skipped.is_empty() {
                    self.status.kind = StatusKind::Error;
                    self.status.text = t!(
                        "No playable track remains in the queue",
                        "대기열에 재생할 수 있는 곡이 없습니다",
                        "キューに再生できる曲がありません"
                    )
                    .to_owned();
                }
                effects.extend(self.maybe_autoplay_extend());
            }
        }

        self.commit_why_gem_post_commit(&mut post_commit);
        self.reconcile_why_gem();

        if post_commit.close_queue_popup {
            self.queue_popup.open = false;
            self.queue_popup.cursor = self.queue.cursor_pos();
            self.queue_popup.anchor = self.queue_popup.cursor;
        }
        if let Some(cursor) = post_commit.queue_removal_cursor {
            self.commit_queue_removal_ui(cursor);
        }
        if post_commit.force_autoplay_extend {
            effects.extend(self.force_autoplay_extend());
        }
        if post_commit.player_mode {
            self.mode = Mode::Player;
        }
        if post_commit.persist_playback_modes {
            effects.push(self.save_playback_modes_cmd());
        }
        if !post_commit.romanize_songs.is_empty() {
            effects.extend(self.request_romanization_for_songs(&post_commit.romanize_songs));
        }
        if let Some(video_id) = post_commit.clear_heal_video_id.take() {
            debug_assert_eq!(
                self.heal.pending_video_id.as_deref(),
                Some(video_id.as_str())
            );
            self.heal.pending_video_id = None;
        }
        if let Some(plan) = post_commit.mode_switch.take() {
            effects.extend(self.commit_mode_switch_after_track(plan));
        }

        if let Some((kind, text)) = status_after_commit {
            self.status.kind = kind;
            self.status.text = text;
        }

        match video_follow_up {
            Some(VideoFollowUp::Continue { status, video_url }) => {
                self.playback.paused = true;
                self.video.paused_audio = true;
                self.status.kind = StatusKind::Info;
                self.status.text = status;
                effects.push(Cmd::VideoLoad(video_url));
            }
            Some(VideoFollowUp::AudioFallback) => {
                self.close_video();
                self.video.paused_audio = false;
                self.status.kind = StatusKind::Info;
                self.status.text = t!(
                    "This track is local-only — continuing with audio",
                    "로컬 전용 트랙이라 소리로 이어서 재생해요",
                    "ローカル専用の曲のため音声で続けて再生します"
                )
                .to_owned();
            }
            Some(VideoFollowUp::QueueEnded) => {
                self.close_video();
                self.video.paused_audio = false;
                self.status.kind = StatusKind::Info;
                self.status.text =
                    t!("Queue ended", "큐가 끝났어요", "キューが終了しました").to_owned();
            }
            None => {}
        }
        self.dirty = true;
        effects
    }
}
