//! Translate raw crossterm terminal events into application [`Msg`]s.
//!
//! Key *release*/*repeat* events (which Windows delivers in addition to presses) are
//! filtered out here so the reducer only ever sees presses.

use crossterm::event::{Event, KeyEventKind, MouseButton, MouseEventKind};

use crate::app::Msg;

pub fn translate(ev: Event) -> Option<Msg> {
    match ev {
        Event::Key(k) if k.kind == KeyEventKind::Press => Some(Msg::Key(k)),
        // A left-button press is click-to-seek; the reducer hit-tests the seekbar.
        Event::Mouse(m) if m.kind == MouseEventKind::Down(MouseButton::Left) => {
            Some(Msg::MouseClick { col: m.column, row: m.row })
        }
        Event::Resize(_, _) => Some(Msg::Resize),
        _ => None,
    }
}
