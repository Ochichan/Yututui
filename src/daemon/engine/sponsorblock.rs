//! Daemon owner for non-music segment skipping. The App mirrors this in
//! `app/sponsorblock.rs`; both feed `yututui_core::sponsorblock::SkipPolicy`.

use yututui_core::sponsorblock::{Observation, SkipPolicy};

use super::*;
use crate::sponsorblock::{Segment, SponsorBlockRequest, eligible_id};

#[derive(Default)]
pub(super) struct DaemonSkip {
    policy: SkipPolicy,
    /// A fetch the serve loop has not yet handed to the actor.
    request: Option<SponsorBlockRequest>,
}

impl DaemonEngine {
    fn sponsorblock_enabled(&self) -> bool {
        self.config.streaming.effective_skip_non_music()
    }

    /// YouTube id of the loaded track when it is the queue's current, eligible track.
    fn sponsorblock_loaded_id(&self) -> Option<String> {
        let song = self.queue.current()?;
        (self.loaded_video_id.as_deref() == Some(song.video_id.as_str()))
            .then(|| eligible_id(song).map(str::to_owned))
            .flatten()
    }

    pub(super) fn sponsorblock_on_load(&mut self, song: &Song) {
        self.sponsorblock
            .policy
            .on_load(self.playback.position_epoch);
        self.sponsorblock.request = None;
        if self.sponsorblock_enabled()
            && let Some(id) = eligible_id(song)
        {
            self.sponsorblock.request = Some(SponsorBlockRequest {
                video_id: id.to_owned(),
            });
        }
    }

    /// The fetch queued by the latest load, if any. The serve loop forwards it to the actor.
    pub(crate) fn take_sponsorblock_request(&mut self) -> Option<SponsorBlockRequest> {
        self.sponsorblock.request.take()
    }

    pub(crate) fn on_sponsorblock_result(&mut self, video_id: String, segments: Vec<Segment>) {
        if self.sponsorblock_enabled()
            && self.sponsorblock_loaded_id().as_deref() == Some(video_id.as_str())
        {
            self.sponsorblock.policy.set_segments(video_id, segments);
        }
    }

    pub(super) fn observe_sponsorblock_position(&mut self, position: f64) {
        if !self.sponsorblock_enabled() {
            return;
        }
        let track_id = self.sponsorblock_loaded_id();
        let blocked = self.listening.has_pending_seek()
            || self.source_recovery.active_ticket().is_some()
            || matches!(
                self.transport_recovery,
                TransportRecoveryState::Recovering(_)
            );
        let Some(target) = self.sponsorblock.policy.observe(Observation {
            position,
            duration: self.playback.duration,
            epoch: self.playback.position_epoch,
            track_id: track_id.as_deref(),
            blocked,
        }) else {
            return;
        };
        if let Err(error) =
            self.send_active_player_command("sponsorblock_skip", PlayerCmd::exact_seek(target))
        {
            tracing::warn!(%error, "daemon could not send a non-music skip");
            self.sponsorblock.policy.skip_abandoned();
            return;
        }
        self.note_seek(target);
        self.sponsorblock.policy.skip_committed();
        tracing::info!(
            target_secs = target,
            "skipped a non-music segment (SponsorBlock)"
        );
    }

    /// Enable the setting and install `segments` for the loaded current track.
    #[cfg(test)]
    pub(crate) fn install_sponsorblock_parity_state(&mut self, segments: &[(f64, f64)]) {
        self.config.streaming.skip_non_music = Some(true);
        let id = self
            .sponsorblock_loaded_id()
            .expect("eligible loaded track");
        self.sponsorblock
            .policy
            .on_load(self.playback.position_epoch);
        self.sponsorblock
            .policy
            .set_segments(id, crate::sponsorblock::sanitize(segments, None));
    }
}
