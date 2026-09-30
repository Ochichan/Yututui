use super::transport::LoadCurrentIntent;
use super::*;

pub(super) struct DaemonCrossfade {
    pub(super) local_crossfade: crate::crossfade::LocalCrossfade,
    pub(super) overlap_support: crate::crossfade::OverlapSupport,
    overlap_fired: bool,
    #[cfg(test)]
    test_overlap_support: Option<crate::crossfade::OverlapSupport>,
}

impl DaemonCrossfade {
    pub(super) fn new(local_crossfade: crate::crossfade::LocalCrossfade) -> Self {
        Self {
            local_crossfade,
            overlap_support: crate::crossfade::OverlapSupport::Untried,
            overlap_fired: false,
            #[cfg(test)]
            test_overlap_support: None,
        }
    }

    pub(super) fn on_player_spawn(&mut self) {
        if !matches!(
            self.overlap_support,
            crate::crossfade::OverlapSupport::Untried
        ) {
            return;
        }
        let support = crate::crossfade::overlap_support();
        #[cfg(test)]
        let support = self.test_overlap_support.unwrap_or(support);
        self.overlap_support = support;
    }

    pub(super) fn overlap_unavailable(&mut self, blocker: crate::crossfade::OverlapBlocker) {
        self.overlap_support = crate::crossfade::OverlapSupport::Unavailable(blocker);
    }

    pub(super) fn load_for_advance(
        &self,
        incoming: crate::player::PlaybackLoad,
        cause: crate::crossfade::AdvanceCause,
        outgoing: Option<&crate::player::PlaybackLoad>,
        outgoing_duration: Option<f64>,
    ) -> crate::player::PlaybackLoad {
        let handoff = crate::crossfade::handoff_for_advance(
            cause,
            outgoing,
            outgoing_duration,
            &incoming,
            self.local_crossfade,
            self.overlap_support,
            false,
        );
        incoming.with_handoff(handoff)
    }

    #[cfg(test)]
    pub(super) fn set_overlap_support_for_test(
        &mut self,
        support: crate::crossfade::OverlapSupport,
    ) {
        self.test_overlap_support = Some(support);
        self.overlap_support = support;
    }
}

impl DaemonEngine {
    /// Ordinary loads rearm transport recovery; recovery has a distinct intent so it cannot
    /// replay a different queue item or duplicate history/signals.
    pub(super) fn load_current_loaded(&mut self) -> Result<(), EngineError> {
        self.load_current_loaded_for(
            LoadCurrentIntent::Ordinary,
            crate::crossfade::AdvanceCause::Manual,
            None,
        )
    }

    pub(super) fn load_current_loaded_after_end(
        &mut self,
        outgoing: Option<crate::player::PlaybackLoad>,
    ) -> Result<(), EngineError> {
        self.load_current_loaded_for(
            LoadCurrentIntent::Ordinary,
            crate::crossfade::AdvanceCause::EndOfTrack,
            outgoing,
        )
    }

    pub(super) async fn advance_crossfade_if_due(&mut self, position: f64) -> Vec<EngineEffect> {
        let local_crossfade = self.crossfade.local_crossfade;
        if self.playback.paused || local_crossfade.is_off() {
            return Vec::new();
        }
        if !crate::crossfade::remaining_in_overlap_window(
            self.playback.duration,
            position,
            local_crossfade.as_secs_f64(),
        ) {
            self.crossfade.overlap_fired = false;
            return Vec::new();
        }
        if self.crossfade.overlap_fired {
            return Vec::new();
        }

        let outgoing = self.current_crossfade_load();
        let incoming = self.next_crossfade_load();
        if crate::crossfade::overlap_due(crate::crossfade::OverlapDueInput {
            paused: self.playback.paused,
            video_overlay: false,
            setting: local_crossfade,
            support: self.crossfade.overlap_support,
            duration: self.playback.duration,
            position,
            already_fired: self.crossfade.overlap_fired,
            outgoing: outgoing.as_ref(),
            next: incoming.as_ref(),
        })
        .is_none()
        {
            return Vec::new();
        }

        self.crossfade.overlap_fired = true;
        self.record_outgoing(true);
        self.advance_after_end().await
    }

    pub(super) fn current_crossfade_load(&self) -> Option<crate::player::PlaybackLoad> {
        let song = self.queue.current()?;
        (self.loaded_video_id.as_deref() == Some(song.video_id.as_str()))
            .then(|| playback_load(song))
            .flatten()
    }

    fn next_crossfade_load(&self) -> Option<crate::player::PlaybackLoad> {
        let cursor = self.queue.plan_next_cursor(self.queue.cursor_pos(), true)?;
        playback_load(self.queue.song_at_cursor(cursor)?)
    }

    pub(super) async fn load_current_after_end(
        &mut self,
        outgoing: Option<crate::player::PlaybackLoad>,
    ) -> Result<(), EngineError> {
        self.ensure_player().await?;
        self.load_current_loaded_after_end(outgoing)
    }

    pub(super) fn set_local_crossfade(
        &mut self,
        tenths: u8,
    ) -> (RemoteResponse, Vec<EngineEffect>) {
        let Some(next) = crate::crossfade::LocalCrossfade::from_remote_tenths(tenths) else {
            return (RemoteResponse::err("crossfade_range"), Vec::new());
        };
        let previous = self.crossfade.local_crossfade;
        // Commit even when the live player refuses SetOverlap, as the App does: every spawn is
        // seeded from this setting, so a replacement player picks it up.
        if previous.is_off() != next.is_off()
            && let Err(error) = self
                .send_player_command_if_active("set_overlap", PlayerCmd::SetOverlap(!next.is_off()))
        {
            tracing::warn!(%error, "daemon could not deliver SetOverlap; setting kept for next spawn");
        }
        self.crossfade.local_crossfade = next;
        self.config.local_crossfade_secs = Some(next.as_secs_f64());
        self.save_config("daemon local crossfade setting");
        (RemoteResponse::status(self.status()), Vec::new())
    }

    #[cfg(test)]
    pub(crate) fn player_file_generation_for_test(&self) -> Option<u64> {
        self.player
            .as_ref()
            .map(|player| player.handle.current_file_generation())
    }

    #[cfg(test)]
    pub(crate) fn set_overlap_support_for_test(
        &mut self,
        support: crate::crossfade::OverlapSupport,
    ) {
        self.crossfade.set_overlap_support_for_test(support);
    }
}

fn playback_load(song: &Song) -> Option<crate::player::PlaybackLoad> {
    let destination = song.playback_destination_checked().ok()?;
    Some(crate::player::PlaybackLoad::from_destination(
        destination,
        crate::player::MediaSourceContext::from_live(song.is_radio_station()),
    ))
}
