//! App owner for non-music segment skipping. The daemon mirrors this in
//! `daemon/engine/sponsorblock.rs`; both feed `yututui_core::sponsorblock::SkipPolicy`.

use yututui_core::sponsorblock::Observation;

use super::*;
use crate::sponsorblock::{SponsorBlockEvent, SponsorBlockRequest, eligible_id};

impl App {
    fn sponsorblock_enabled(&self) -> bool {
        self.config.streaming.effective_skip_non_music()
    }

    /// YouTube id of the loaded track when it is the queue's current, eligible track.
    fn sponsorblock_loaded_id(&self) -> Option<String> {
        let song = self.queue.current()?;
        (self.prefetch.loaded_video_id.as_deref() == Some(song.video_id.as_str()))
            .then(|| eligible_id(song).map(str::to_owned))
            .flatten()
    }

    pub(in crate::app) fn sponsorblock_on_load(&mut self, song: &Song) -> Vec<Cmd> {
        self.playback
            .skip_policy
            .on_load(self.playback.position_epoch);
        if !self.sponsorblock_enabled() {
            return Vec::new();
        }
        eligible_id(song)
            .map(|id| {
                Cmd::Fetch(FetchCmd::SponsorBlock(SponsorBlockRequest {
                    video_id: id.to_owned(),
                }))
            })
            .into_iter()
            .collect()
    }

    pub(in crate::app) fn handle_sponsorblock_event(
        &mut self,
        event: SponsorBlockEvent,
    ) -> Vec<Cmd> {
        let SponsorBlockEvent::Result { video_id, segments } = event;
        if self.sponsorblock_enabled()
            && self.sponsorblock_loaded_id().as_deref() == Some(video_id.as_str())
        {
            self.playback.skip_policy.set_segments(video_id, segments);
        }
        Vec::new()
    }

    pub(in crate::app) fn observe_sponsorblock_position(&mut self, position: f64) -> Vec<Cmd> {
        if !self.sponsorblock_enabled() {
            return Vec::new();
        }
        let track_id = self.sponsorblock_loaded_id();
        let blocked = self.personal_state.listening.has_pending_seek()
            || self.interaction.seekbar_scrub.is_some()
            || self.prefetch.source_recovery.active_ticket().is_some()
            || self.recorder.transport_recovery_active;
        let Some(target) = self.playback.skip_policy.observe(Observation {
            position,
            duration: self.playback.duration,
            epoch: self.playback.position_epoch,
            track_id: track_id.as_deref(),
            blocked,
        }) else {
            return Vec::new();
        };
        self.player_intent(
            "sponsorblock_skip",
            PlayerCmd::exact_seek(target),
            PlayerCommit::SponsorBlockSkip { target },
        )
    }

    /// Runs inside the admitted-intent turn; `update` bumps the epoch once right after, which
    /// the policy must not read as a foreign discontinuity.
    pub(in crate::app) fn commit_sponsorblock_skip(&mut self, target: f64) {
        self.supersede_source_recovery();
        self.playback.time_pos = Some(target);
        self.playback.time_pos_at = Some(Instant::now());
        self.playback.skip_policy.skip_committed();
        self.set_status_info(t!(
            "Skipped a non-music segment (SponsorBlock)",
            "음악이 아닌 구간을 건너뛰었습니다 (SponsorBlock)",
            "音楽以外の区間をスキップしました (SponsorBlock)"
        ));
    }

    pub(crate) fn reject_sponsorblock_skip(&mut self) {
        self.playback.skip_policy.skip_abandoned();
    }

    /// Enable the setting and install `segments` for the loaded current track.
    #[cfg(test)]
    pub(crate) fn install_sponsorblock_parity_state(&mut self, segments: &[(f64, f64)]) {
        self.config.streaming.skip_non_music = Some(true);
        let id = self
            .sponsorblock_loaded_id()
            .expect("eligible loaded track");
        self.playback
            .skip_policy
            .on_load(self.playback.position_epoch);
        self.playback
            .skip_policy
            .set_segments(id, crate::sponsorblock::sanitize(segments, None));
    }
}
