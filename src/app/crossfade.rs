use super::*;
use crate::crossfade::LocalCrossfade;

impl App {
    pub(in crate::app) fn local_crossfade_key(&mut self, chord: Chord) -> Option<Vec<Cmd>> {
        let steps = match self.keymap.context_action(KeyContext::LocalDeck, chord)? {
            Action::LocalCrossfadeDown => -1,
            Action::LocalCrossfadeUp => 1,
            _ => return None,
        };
        Some(self.nudge_local_crossfade(steps))
    }

    fn nudge_local_crossfade(&mut self, steps: i8) -> Vec<Cmd> {
        let previous = self.audio.local_crossfade;
        let next = previous.nudge(steps);
        self.audio.local_crossfade = next;
        self.config.local_crossfade_secs = Some(next.as_secs_f64());
        self.status.kind = StatusKind::Info;
        self.status.text = format!(
            "{}: {}",
            t!(
                "Local crossfade",
                "로컬 크로스페이드",
                "ローカルクロスフェード"
            ),
            next.label()
        );
        self.dirty = true;
        let mut cmds = Vec::new();
        if !previous.is_off() && next.is_off() {
            cmds.extend(self.player_intent(
                "retire_extra",
                PlayerCmd::RetireExtra,
                PlayerCommit::RetireExtra,
            ));
        }
        cmds.push(Cmd::Persist(PersistCmd::Config(Box::new(
            self.config.clone(),
        ))));
        cmds
    }

    pub fn crossfade_chip(&self) -> Option<String> {
        if self.video.proc.is_some() {
            return None;
        }
        self.audio
            .overlap_support
            .is_available()
            .then_some(self.audio.local_crossfade)
            .filter(|setting| !setting.is_off())
            .map(LocalCrossfade::label)
    }

    pub(in crate::app) fn begin_crossfade_if_due(&mut self) -> Vec<Cmd> {
        if self.playback.paused || self.video.proc.is_some() {
            return Vec::new();
        }
        if self.audio.local_crossfade.is_off() {
            return Vec::new();
        }
        let fade_secs = self.audio.local_crossfade.as_secs_f64();
        let position = self.playback.time_pos.unwrap_or(0.0);
        if !crate::crossfade::remaining_in_overlap_window(
            self.playback.duration,
            position,
            fade_secs,
        ) {
            self.playback.overlap_fired = false;
            return Vec::new();
        }
        if self.playback.overlap_fired {
            return Vec::new();
        }
        let cursor = self.queue.cursor_pos();
        let Some(next) = self.queue.plan_next_cursor(cursor, true) else {
            return Vec::new();
        };
        let Some(song) = self.queue.song_at_cursor(next).cloned() else {
            return Vec::new();
        };
        let Ok(load) = self.prepare_track_load(song) else {
            return Vec::new();
        };
        let incoming = load.as_playback_load();
        let crate::crossfade::TrackHandoff::Overlap { .. } = crate::crossfade::handoff(
            self.playback.loaded.as_ref(),
            self.playback.duration,
            &incoming,
            self.audio.local_crossfade,
            self.audio.overlap_support,
        ) else {
            return Vec::new();
        };
        self.playback.overlap_fired = true;
        self.advance_with_outgoing(true, true)
    }
}
