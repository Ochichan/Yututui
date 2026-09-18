//! Local Deck crossfade keys and the status chip.

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
        let next = self.audio.local_crossfade.nudge(steps);
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
        vec![Cmd::Persist(PersistCmd::Config(Box::new(
            self.config.clone(),
        )))]
    }

    pub fn crossfade_chip(&self) -> Option<String> {
        self.audio
            .overlap_support
            .is_available()
            .then(|| self.audio.local_crossfade)
            .filter(|setting| !setting.is_off())
            .map(LocalCrossfade::label)
    }
}
