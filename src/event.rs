//! Translate raw crossterm terminal events into application [`Msg`]s.
//!
//! Key *release*/*repeat* events (which Windows delivers in addition to presses) are
//! filtered out here so the reducer only ever sees presses.

use std::time::{Duration, Instant};

use crossterm::event::{
    Event, KeyCode, KeyEventKind, KeyModifiers, ModifierKeyCode, MouseButton, MouseEventKind,
};

use crate::app::Msg;

/// Two left-presses within this window at the same cell count as a double-click.
const DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(400);

#[derive(Debug)]
pub struct Translator {
    held_modifiers: KeyModifiers,
    /// The most recent left-button press (time + cell), for double-click detection.
    last_left_down: Option<(Instant, u16, u16)>,
}

impl Default for Translator {
    fn default() -> Self {
        Self {
            held_modifiers: KeyModifiers::NONE,
            last_left_down: None,
        }
    }
}

impl Translator {
    pub fn translate(&mut self, ev: Event) -> Option<Msg> {
        match ev {
            Event::Key(mut k) => {
                if let Some(modifier) = modifier_key(k.code) {
                    self.update_held_modifier(modifier, k.kind);
                    return None;
                }
                if k.kind != KeyEventKind::Press {
                    return None;
                }
                k.modifiers |= self.held_modifiers;
                Some(Msg::Key(k))
            }
            // A left-button press may hit a UI button or the player's seekbar. A second
            // press at the same cell within the double-click window plays a song row /
            // queue entry instead of merely selecting it.
            Event::Mouse(m) if m.kind == MouseEventKind::Down(MouseButton::Left) => {
                Some(self.classify_left_down(m.column, m.row))
            }
            // Dragging with the left button held extends the queue window's selection.
            Event::Mouse(m) if m.kind == MouseEventKind::Drag(MouseButton::Left) => {
                Some(Msg::MouseDrag {
                    col: m.column,
                    row: m.row,
                })
            }
            // Wheel scroll moves the active list's cursor (Library / Search). `up` means
            // toward earlier items.
            Event::Mouse(m) if m.kind == MouseEventKind::ScrollUp => Some(Msg::MouseScroll { up: true }),
            Event::Mouse(m) if m.kind == MouseEventKind::ScrollDown => {
                Some(Msg::MouseScroll { up: false })
            }
            Event::Resize(_, _) => Some(Msg::Resize),
            // Terminal focus in/out (DECSET ?1004, enabled in `tui::init`). Lets the reducer
            // park animations while the window is hidden/behind another. A spurious startup
            // `FocusGained` (some multiplexers emit one) is harmless — `focused` is already true.
            Event::FocusGained => Some(Msg::Focus(true)),
            Event::FocusLost => Some(Msg::Focus(false)),
            _ => None,
        }
    }

    /// Classify a left-button press as a single or double click, updating the timing state.
    fn classify_left_down(&mut self, col: u16, row: u16) -> Msg {
        self.classify_left_down_at(Instant::now(), col, row)
    }

    /// Timing core, split out so tests can supply a deterministic clock.
    fn classify_left_down_at(&mut self, now: Instant, col: u16, row: u16) -> Msg {
        let is_double = self.last_left_down.is_some_and(|(t, c, r)| {
            c == col && r == row && now.duration_since(t) <= DOUBLE_CLICK_WINDOW
        });
        // Reset after a double so a third quick press starts a fresh single click.
        self.last_left_down = if is_double { None } else { Some((now, col, row)) };
        if is_double {
            Msg::MouseDoubleClick { col, row }
        } else {
            Msg::MouseClick { col, row }
        }
    }

    fn update_held_modifier(&mut self, modifier: KeyModifiers, kind: KeyEventKind) {
        match kind {
            KeyEventKind::Press | KeyEventKind::Repeat => self.held_modifiers |= modifier,
            KeyEventKind::Release => self.held_modifiers.remove(modifier),
        }
    }
}

fn modifier_key(code: KeyCode) -> Option<KeyModifiers> {
    let KeyCode::Modifier(code) = code else {
        return None;
    };
    Some(match code {
        ModifierKeyCode::LeftShift | ModifierKeyCode::RightShift => KeyModifiers::SHIFT,
        ModifierKeyCode::LeftControl | ModifierKeyCode::RightControl => KeyModifiers::CONTROL,
        ModifierKeyCode::LeftAlt | ModifierKeyCode::RightAlt => KeyModifiers::ALT,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent, KeyEventState};

    fn key(code: KeyCode, modifiers: KeyModifiers, kind: KeyEventKind) -> Event {
        Event::Key(KeyEvent {
            code,
            modifiers,
            kind,
            state: KeyEventState::NONE,
        })
    }

    #[test]
    fn filters_release_and_repeat_key_events() {
        let mut input = Translator::default();
        assert!(
            input
                .translate(key(
                    KeyCode::Char('q'),
                    KeyModifiers::NONE,
                    KeyEventKind::Release,
                ))
                .is_none()
        );
        assert!(
            input
                .translate(key(
                    KeyCode::Char('q'),
                    KeyModifiers::NONE,
                    KeyEventKind::Repeat,
                ))
                .is_none()
        );
    }

    #[test]
    fn two_quick_presses_at_same_cell_are_a_double_click() {
        let mut t = Translator::default();
        let t0 = Instant::now();
        assert!(matches!(
            t.classify_left_down_at(t0, 10, 5),
            Msg::MouseClick { col: 10, row: 5 }
        ));
        // Within the window, same cell -> double click.
        assert!(matches!(
            t.classify_left_down_at(t0 + Duration::from_millis(200), 10, 5),
            Msg::MouseDoubleClick { col: 10, row: 5 }
        ));
        // A third quick press is a fresh single click (state reset after the double).
        assert!(matches!(
            t.classify_left_down_at(t0 + Duration::from_millis(300), 10, 5),
            Msg::MouseClick { .. }
        ));
    }

    #[test]
    fn slow_or_moved_presses_stay_single_clicks() {
        let mut t = Translator::default();
        let t0 = Instant::now();
        assert!(matches!(t.classify_left_down_at(t0, 10, 5), Msg::MouseClick { .. }));
        // Too slow.
        assert!(matches!(
            t.classify_left_down_at(t0 + Duration::from_millis(600), 10, 5),
            Msg::MouseClick { .. }
        ));
        // Different cell.
        assert!(matches!(
            t.classify_left_down_at(t0 + Duration::from_millis(650), 11, 5),
            Msg::MouseClick { .. }
        ));
    }

    #[test]
    fn left_drag_becomes_a_drag_message() {
        let mut t = Translator::default();
        let ev = Event::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 7,
            row: 3,
            modifiers: KeyModifiers::NONE,
        });
        assert!(matches!(t.translate(ev), Some(Msg::MouseDrag { col: 7, row: 3 })));
    }

    #[test]
    fn wheel_scroll_becomes_a_scroll_message() {
        let mut t = Translator::default();
        let wheel = |kind| {
            Event::Mouse(crossterm::event::MouseEvent {
                kind,
                column: 4,
                row: 9,
                modifiers: KeyModifiers::NONE,
            })
        };
        assert!(matches!(
            t.translate(wheel(MouseEventKind::ScrollUp)),
            Some(Msg::MouseScroll { up: true })
        ));
        assert!(matches!(
            t.translate(wheel(MouseEventKind::ScrollDown)),
            Some(Msg::MouseScroll { up: false })
        ));
    }

    #[test]
    fn held_shift_modifier_is_applied_to_following_key_events() {
        let mut input = Translator::default();
        assert!(
            input
                .translate(key(
                    KeyCode::Modifier(ModifierKeyCode::LeftShift),
                    KeyModifiers::SHIFT,
                    KeyEventKind::Press,
                ))
                .is_none()
        );

        let Some(Msg::Key(k)) = input.translate(key(
            KeyCode::Char('ㅣ'),
            KeyModifiers::NONE,
            KeyEventKind::Press,
        )) else {
            panic!("expected a key message");
        };
        assert!(k.modifiers.contains(KeyModifiers::SHIFT));

        assert!(
            input
                .translate(key(
                    KeyCode::Modifier(ModifierKeyCode::LeftShift),
                    KeyModifiers::SHIFT,
                    KeyEventKind::Release,
                ))
                .is_none()
        );

        let Some(Msg::Key(k)) = input.translate(key(
            KeyCode::Char('ㅣ'),
            KeyModifiers::NONE,
            KeyEventKind::Press,
        )) else {
            panic!("expected a key message");
        };
        assert!(!k.modifiers.contains(KeyModifiers::SHIFT));
    }
}
