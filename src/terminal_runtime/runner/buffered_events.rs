use std::collections::VecDeque;

use crate::runtime::RuntimeEvent;

/// Coalesced owner events which have left the keyed buffer but have not reached the reducer.
///
/// Keep the full runtime envelope until the exact reducer turn. In particular, converting a
/// file-scoped player event to `Msg` here would erase its generation and make a later buffered
/// turn unable to reject it after an earlier turn admitted a new Load/Stop.
#[derive(Default)]
pub(super) struct BufferedWorkerEvents {
    events: VecDeque<RuntimeEvent>,
}

impl BufferedWorkerEvents {
    pub(super) fn extend(&mut self, events: impl IntoIterator<Item = RuntimeEvent>) {
        self.events.extend(events);
    }

    pub(super) fn push_front(&mut self, event: RuntimeEvent) {
        self.events.push_front(event);
    }

    pub(super) fn pop_front(&mut self) -> Option<RuntimeEvent> {
        self.events.pop_front()
    }

    /// Drop player events whose file generation stopped being current while this batch waited.
    pub(super) fn pop_current(
        &mut self,
        mut player_is_current: impl FnMut(&crate::player::PlayerEvent) -> bool,
    ) -> Option<RuntimeEvent> {
        loop {
            let event = self.events.pop_front()?;
            if let RuntimeEvent::Player(player) = &event
                && !player_is_current(player)
            {
                continue;
            }
            return Some(event);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_is_rechecked_after_an_earlier_buffered_turn() {
        let mut buffered = BufferedWorkerEvents::default();
        buffered.extend([
            RuntimeEvent::App(crate::app::Msg::Noop),
            RuntimeEvent::Player(crate::player::PlayerEvent::file_scoped(
                1,
                crate::player::PlayerEvent::TimePos(17.0),
            )),
        ]);
        let mut current_generation = 1;

        assert!(matches!(
            buffered.pop_current(|event| event.file_generation() == Some(current_generation)),
            Some(RuntimeEvent::App(crate::app::Msg::Noop))
        ));
        // The first buffered reducer turn admitted a new file before the next buffered turn.
        current_generation = 2;
        assert!(
            buffered
                .pop_current(|event| event.file_generation() == Some(current_generation))
                .is_none(),
            "the old file event must retain its generation and be rejected"
        );
    }
}
