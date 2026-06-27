//! Translate raw crossterm terminal events into application [`Msg`]s.
//!
//! Key *release*/*repeat* events (which Windows delivers in addition to presses) are
//! filtered out here so the reducer only ever sees presses.

use crossterm::event::{
    Event, KeyCode, KeyEventKind, KeyModifiers, ModifierKeyCode, MouseButton, MouseEventKind,
};

use crate::app::Msg;

#[derive(Debug)]
pub struct Translator {
    held_modifiers: KeyModifiers,
}

impl Default for Translator {
    fn default() -> Self {
        Self {
            held_modifiers: KeyModifiers::NONE,
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
            // A left-button press may hit a UI button or the player's seekbar.
            Event::Mouse(m) if m.kind == MouseEventKind::Down(MouseButton::Left) => {
                Some(Msg::MouseClick {
                    col: m.column,
                    row: m.row,
                })
            }
            Event::Resize(_, _) => Some(Msg::Resize),
            _ => None,
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
