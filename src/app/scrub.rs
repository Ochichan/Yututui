//! Preview-only TUI seekbar sessions.

use super::*;

impl App {
    pub(in crate::app) fn begin_seekbar_scrub(&mut self, column: u16, area: Rect, duration: f64) {
        crate::player::diagnostics::scrub_started();
        let column = column.clamp(area.x, area.right().saturating_sub(1));
        let target = seekbar_target(column, area, duration);
        self.interaction.seekbar_scrub = Some(SeekbarScrub {
            queue_revision: self.queue.rev(),
            track_id: self.queue.current().map(|song| song.video_id.clone()),
            position_epoch: self.playback.position_epoch,
            duration,
            area,
            column,
            target,
            awaiting_admission: false,
        });
        self.dirty = true;
    }

    pub(in crate::app) fn update_seekbar_scrub(&mut self, column: u16) {
        if !self.seekbar_scrub_is_current() {
            self.cancel_seekbar_scrub();
            return;
        }
        let scrub = self
            .interaction
            .seekbar_scrub
            .as_mut()
            .expect("current scrub remains installed");
        if scrub.awaiting_admission {
            return;
        }
        let column = column.clamp(scrub.area.x, scrub.area.right().saturating_sub(1));
        if column == scrub.column {
            return;
        }
        scrub.column = column;
        scrub.target = seekbar_target(column, scrub.area, scrub.duration);
        self.dirty = true;
    }

    pub(in crate::app) fn commit_seekbar_scrub(&mut self) -> Vec<Cmd> {
        if !self.seekbar_scrub_is_current() {
            self.cancel_seekbar_scrub();
            return Vec::new();
        }
        let scrub = self
            .interaction
            .seekbar_scrub
            .as_mut()
            .expect("current scrub remains installed");
        if scrub.awaiting_admission {
            return Vec::new();
        }
        scrub.awaiting_admission = true;
        let target = scrub.target;
        crate::player::diagnostics::scrub_committed();
        self.player_intent(
            "seek_absolute",
            PlayerCmd::interactive_seek(target),
            PlayerCommit::Seek {
                optimistic_position: Some(target),
            },
        )
    }

    pub(in crate::app) fn cancel_seekbar_scrub(&mut self) {
        if let Some(scrub) = self.interaction.seekbar_scrub.take() {
            if !scrub.awaiting_admission {
                crate::player::diagnostics::scrub_cancelled();
            }
            self.dirty = true;
        }
    }

    pub(in crate::app) fn cancel_stale_seekbar_scrub(&mut self) {
        if self.interaction.seekbar_scrub.is_some() && !self.seekbar_scrub_is_current() {
            self.cancel_seekbar_scrub();
        }
    }

    pub(crate) fn seekbar_preview_target(&self) -> Option<f64> {
        self.seekbar_scrub_is_current()
            .then(|| {
                self.interaction
                    .seekbar_scrub
                    .as_ref()
                    .map(|scrub| scrub.target)
            })
            .flatten()
    }

    /// Runtime calls this for every admitted seek; only a released mouse scrub is awaiting a
    /// correlated admission, so keyboard/remote seeks cannot clear an active drag preview.
    pub(crate) fn settle_mouse_seek_admission(&mut self, _accepted: bool) {
        if self
            .interaction
            .seekbar_scrub
            .as_ref()
            .is_some_and(|scrub| scrub.awaiting_admission)
        {
            self.cancel_seekbar_scrub();
        }
    }

    fn seekbar_scrub_is_current(&self) -> bool {
        let Some(scrub) = self.interaction.seekbar_scrub.as_ref() else {
            return false;
        };
        self.focused
            && self.player_controls_live()
            && !self.queue_popup.open
            && !self.current_is_radio_stream()
            && self.hits.seekbar_rect() == Some(scrub.area)
            && self.queue.rev() == scrub.queue_revision
            && self.queue.current().map(|song| song.video_id.as_str()) == scrub.track_id.as_deref()
            && self.playback.position_epoch.eq(&scrub.position_epoch)
            && self
                .playback
                .duration
                .is_some_and(|duration| duration.to_bits() == scrub.duration.to_bits())
    }
}

fn seekbar_target(column: u16, area: Rect, duration: f64) -> f64 {
    let fraction = f64::from(column.saturating_sub(area.x)) / f64::from(area.width);
    (fraction * duration).clamp(0.0, duration)
}
