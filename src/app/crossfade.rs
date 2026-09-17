//! The reducer's crossfade behaviors: the Local Deck nudge keys.

use super::*;

impl App {
    /// `[` / `]` in Local Deck. `None` when the chord is not a nudge, so `on_key_local` keeps
    /// its existing fall-through order.
    pub(in crate::app) fn local_crossfade_key(&mut self, chord: Chord) -> Option<Vec<Cmd>> {
        let steps = match self.keymap.context_action(KeyContext::LocalDeck, chord)? {
            Action::LocalCrossfadeDown => -1,
            Action::LocalCrossfadeUp => 1,
            _ => return None,
        };
        Some(self.nudge_local_crossfade(steps))
    }

    /// Stored only, exactly like the seek-interval slider. Nothing is pushed to mpv, so this
    /// needs no player admission and no `PlayerCommit`. Config is written here rather than on a
    /// later settings save, so opening Settings always shows what the keys just set.
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
}
