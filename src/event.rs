//! Translate raw crossterm terminal events into application [`Msg`]s.
//!
//! Key *release* events (which Windows and the enhanced keyboard protocol deliver in
//! addition to presses) are filtered out here. Key *repeat* events — held keys on
//! enhanced terminals — are dropped too, except for the navigation keys, which are
//! forwarded so holding an arrow keeps scrolling (see [`is_autorepeat_nav_key`]).

use std::time::{Duration, Instant};

use crossterm::event::{
    Event, KeyCode, KeyEventKind, KeyModifiers, ModifierKeyCode, MouseButton, MouseEventKind,
};

use crate::app::Msg;

/// Two same-button presses within this window at the same cell count as a double-click.
const DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(400);

/// Modifiers that turn a left click into a multi-select toggle: Ctrl on Windows/Linux,
/// Cmd on macOS. Terminals disagree on whether Cmd arrives as SUPER or META, and
/// accepting Ctrl everywhere costs nothing, so all three count on every platform.
const MULTI_SELECT_MODIFIERS: KeyModifiers = KeyModifiers::CONTROL
    .union(KeyModifiers::SUPER)
    .union(KeyModifiers::META);

#[derive(Debug)]
pub struct Translator {
    held_modifiers: KeyModifiers,
    /// The most recent left-button press (time + cell), for double-click detection.
    last_left_down: Option<(Instant, u16, u16)>,
    /// The most recent right-button press (time + cell), for double-click detection.
    last_right_down: Option<(Instant, u16, u16)>,
    /// Whether a left-button press has not yet been released. Some terminals report motion
    /// during a press as `Moved` rather than `Drag`, so keep enough state to preserve dragging.
    left_down: bool,
}

impl Default for Translator {
    fn default() -> Self {
        Self {
            held_modifiers: KeyModifiers::NONE,
            last_left_down: None,
            last_right_down: None,
            left_down: false,
        }
    }
}

impl Translator {
    /// `col_scale`/`row_scale` are the per-axis text-zoom factors currently applied by the
    /// terminal backend (from [`crate::zoom::ZoomHandle::mouse_scale`]): mouse events arrive
    /// in physical cells, but the app lays out (and hit-tests) on the zoom backend's virtual
    /// grid, so every mouse coordinate is divided here — before double-click detection, which
    /// must compare virtual cells too. The two axes can differ: DECDHL doubles the row while
    /// keeping the column logical, so a single scalar would land clicks half a screen left.
    pub fn translate(&mut self, ev: Event, col_scale: u8, row_scale: u8) -> Option<Msg> {
        let cs = u16::from(col_scale.max(1));
        let rs = u16::from(row_scale.max(1));
        match ev {
            Event::Key(mut k) => {
                if let Some(modifier) = modifier_key(k.code) {
                    self.update_held_modifier(modifier, k.kind);
                    return None;
                }
                if k.kind == KeyEventKind::Release {
                    return None;
                }
                // Held keys stream as `Repeat` on enhanced terminals (plain terminals
                // re-send `Press`). Forward `Repeat` only for the navigation keys — nav /
                // seek / volume / value-change, all repeatable — so holding one keeps going;
                // `Repeat` for chars, Enter, Space, etc. stays dropped so text entry and
                // one-shot commands are never auto-spammed.
                if k.kind == KeyEventKind::Repeat && !is_autorepeat_nav_key(k.code) {
                    return None;
                }
                k.modifiers |= self.held_modifiers;
                Some(Msg::Key(k))
            }
            // A left-button press may hit a UI button or the player's seekbar. A second
            // press at the same cell within the double-click window plays a song row /
            // queue entry instead of merely selecting it. With the multi-select modifier
            // held (Ctrl, or Cmd on macOS — some terminals encode it in the mouse report,
            // others only as modifier key events, so honour both like Ctrl+wheel) the press
            // is always a single toggle click: it must never chain into a double-click,
            // so the timing state is reset instead of recorded.
            Event::Mouse(m) if m.kind == MouseEventKind::Down(MouseButton::Left) => {
                self.left_down = true;
                if (m.modifiers | self.held_modifiers).intersects(MULTI_SELECT_MODIFIERS) {
                    self.last_left_down = None;
                    Some(Msg::MouseClick {
                        col: m.column / cs,
                        row: m.row / rs,
                        multi: true,
                    })
                } else {
                    Some(self.classify_left_down(m.column / cs, m.row / rs))
                }
            }
            // A right-button press is interpreted by the active surface. As with left clicks,
            // classify a second press at the same virtual cell separately so bindings can
            // distinguish single and double clicks.
            Event::Mouse(m) if m.kind == MouseEventKind::Down(MouseButton::Right) => {
                Some(self.classify_right_down(m.column / cs, m.row / rs))
            }
            // Dragging with the left button held extends the queue window's selection.
            Event::Mouse(m) if m.kind == MouseEventKind::Drag(MouseButton::Left) => {
                Some(Msg::MouseDrag {
                    col: m.column / cs,
                    row: m.row / rs,
                })
            }
            Event::Mouse(m) if m.kind == MouseEventKind::Moved && self.left_down => {
                Some(Msg::MouseDrag {
                    col: m.column / cs,
                    row: m.row / rs,
                })
            }
            Event::Mouse(m) if m.kind == MouseEventKind::Up(MouseButton::Left) => {
                self.left_down = false;
                Some(Msg::MouseLeftUp)
            }
            // Wheel scroll moves the active list's viewport, or nudges volume over the
            // player volume cluster. Preserve the pointer cell so the reducer can decide.
            // Ctrl+wheel is the text-zoom gesture: some terminals encode the modifier in
            // the mouse report, others (with the enhanced keyboard protocol) only in
            // modifier key events — honour both.
            Event::Mouse(m) if m.kind == MouseEventKind::ScrollUp => Some(Msg::MouseScroll {
                up: true,
                col: m.column / cs,
                row: m.row / rs,
                ctrl: (m.modifiers | self.held_modifiers).contains(KeyModifiers::CONTROL),
            }),
            Event::Mouse(m) if m.kind == MouseEventKind::ScrollDown => Some(Msg::MouseScroll {
                up: false,
                col: m.column / cs,
                row: m.row / rs,
                ctrl: (m.modifiers | self.held_modifiers).contains(KeyModifiers::CONTROL),
            }),
            Event::Resize(_, _) => Some(Msg::Resize),
            // Terminal focus in/out (DECSET ?1004, enabled in `tui::init`). Lets the reducer
            // park animations while the window is hidden/behind another. A spurious startup
            // `FocusGained` (some multiplexers emit one) is harmless — `focused` is already true.
            Event::FocusGained => Some(Msg::Focus(true)),
            Event::FocusLost => {
                // Losing focus can swallow the key/button *release* that would clear these
                // latches, leaving a held modifier or an in-progress drag "stuck" — the next
                // keypress would then be misread as a command chord. Reset on blur (the
                // universal desktop convention of dropping held-key/drag state on focus loss).
                self.held_modifiers = KeyModifiers::NONE;
                self.left_down = false;
                self.last_left_down = None;
                self.last_right_down = None;
                Some(Msg::Focus(false))
            }
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
        self.last_left_down = if is_double {
            None
        } else {
            Some((now, col, row))
        };
        if is_double {
            Msg::MouseDoubleClick { col, row }
        } else {
            Msg::MouseClick {
                col,
                row,
                multi: false,
            }
        }
    }

    /// Classify a right-button press as a single or double click, updating the timing state.
    fn classify_right_down(&mut self, col: u16, row: u16) -> Msg {
        self.classify_right_down_at(Instant::now(), col, row)
    }

    /// Timing core, split out so tests can supply a deterministic clock.
    fn classify_right_down_at(&mut self, now: Instant, col: u16, row: u16) -> Msg {
        let is_double = self.last_right_down.is_some_and(|(t, c, r)| {
            c == col && r == row && now.duration_since(t) <= DOUBLE_CLICK_WINDOW
        });
        // Reset after a double so a third quick press starts a fresh single click.
        self.last_right_down = if is_double {
            None
        } else {
            Some((now, col, row))
        };
        if is_double {
            Msg::MouseRightDoubleClick { col, row }
        } else {
            Msg::MouseRightClick { col, row }
        }
    }

    fn update_held_modifier(&mut self, modifier: KeyModifiers, kind: KeyEventKind) {
        match kind {
            KeyEventKind::Press | KeyEventKind::Repeat => self.held_modifiers |= modifier,
            KeyEventKind::Release => self.held_modifiers.remove(modifier),
        }
    }
}

/// Keys whose held-repeat we forward: arrows, Page, Home, End. Everywhere they resolve to
/// repeatable semantics (list nav, seek, volume, settings value change), so auto-repeating
/// them is always safe — unlike typed characters or one-shot commands.
fn is_autorepeat_nav_key(code: KeyCode) -> bool {
    matches!(
        code,
        KeyCode::Up
            | KeyCode::Down
            | KeyCode::Left
            | KeyCode::Right
            | KeyCode::PageUp
            | KeyCode::PageDown
            | KeyCode::Home
            | KeyCode::End
    )
}

fn modifier_key(code: KeyCode) -> Option<KeyModifiers> {
    let KeyCode::Modifier(code) = code else {
        return None;
    };
    Some(match code {
        ModifierKeyCode::LeftShift | ModifierKeyCode::RightShift => KeyModifiers::SHIFT,
        ModifierKeyCode::LeftControl | ModifierKeyCode::RightControl => KeyModifiers::CONTROL,
        ModifierKeyCode::LeftAlt | ModifierKeyCode::RightAlt => KeyModifiers::ALT,
        // Cmd on macOS (enhanced-keyboard terminals report it only as key events, never in
        // the SGR mouse report) — tracked so Cmd+click can resolve as a multi-select toggle.
        ModifierKeyCode::LeftSuper | ModifierKeyCode::RightSuper => KeyModifiers::SUPER,
        ModifierKeyCode::LeftMeta | ModifierKeyCode::RightMeta => KeyModifiers::META,
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
                .translate(
                    key(
                        KeyCode::Char('q'),
                        KeyModifiers::NONE,
                        KeyEventKind::Release,
                    ),
                    1,
                    1
                )
                .is_none()
        );
        // A char repeat stays dropped so held letters don't auto-spam text fields / commands.
        assert!(
            input
                .translate(
                    key(KeyCode::Char('q'), KeyModifiers::NONE, KeyEventKind::Repeat,),
                    1,
                    1
                )
                .is_none()
        );
    }

    #[test]
    fn forwards_repeat_for_nav_keys_but_still_drops_their_release() {
        let mut input = Translator::default();
        // Held arrow (Repeat) is forwarded so holding a nav key keeps scrolling.
        assert!(matches!(
            input.translate(
                key(KeyCode::Down, KeyModifiers::NONE, KeyEventKind::Repeat),
                1,
                1
            ),
            Some(Msg::Key(k)) if k.code == KeyCode::Down
        ));
        // Release is always dropped, even for a nav key.
        assert!(
            input
                .translate(
                    key(KeyCode::Down, KeyModifiers::NONE, KeyEventKind::Release),
                    1,
                    1
                )
                .is_none()
        );
    }

    #[test]
    fn two_quick_presses_at_same_cell_are_a_double_click() {
        let mut t = Translator::default();
        let t0 = Instant::now();
        assert!(matches!(
            t.classify_left_down_at(t0, 10, 5),
            Msg::MouseClick {
                col: 10,
                row: 5,
                multi: false
            }
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
        assert!(matches!(
            t.classify_left_down_at(t0, 10, 5),
            Msg::MouseClick { .. }
        ));
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
    fn two_quick_right_presses_at_same_cell_are_a_double_click() {
        let mut t = Translator::default();
        let t0 = Instant::now();
        assert!(matches!(
            t.classify_right_down_at(t0, 10, 5),
            Msg::MouseRightClick { col: 10, row: 5 }
        ));
        assert!(matches!(
            t.classify_right_down_at(t0 + Duration::from_millis(200), 10, 5),
            Msg::MouseRightDoubleClick { col: 10, row: 5 }
        ));
        // A third quick press starts a new sequence rather than extending the double-click.
        assert!(matches!(
            t.classify_right_down_at(t0 + Duration::from_millis(300), 10, 5),
            Msg::MouseRightClick { col: 10, row: 5 }
        ));
    }

    #[test]
    fn slow_or_moved_right_presses_stay_single_clicks() {
        let mut t = Translator::default();
        let t0 = Instant::now();
        assert!(matches!(
            t.classify_right_down_at(t0, 10, 5),
            Msg::MouseRightClick { .. }
        ));
        assert!(matches!(
            t.classify_right_down_at(t0 + Duration::from_millis(600), 10, 5),
            Msg::MouseRightClick { .. }
        ));
        assert!(matches!(
            t.classify_right_down_at(t0 + Duration::from_millis(650), 11, 5),
            Msg::MouseRightClick { .. }
        ));
    }

    #[test]
    fn focus_loss_resets_right_double_click_detection() {
        let mut t = Translator::default();
        let t0 = Instant::now();
        assert!(matches!(
            t.classify_right_down_at(t0, 10, 5),
            Msg::MouseRightClick { .. }
        ));
        assert!(matches!(
            t.translate(Event::FocusLost, 1, 1),
            Some(Msg::Focus(false))
        ));
        assert!(matches!(
            t.classify_right_down_at(t0 + Duration::from_millis(200), 10, 5),
            Msg::MouseRightClick { .. }
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
        assert!(matches!(
            t.translate(ev, 1, 1),
            Some(Msg::MouseDrag { col: 7, row: 3 })
        ));
    }

    #[test]
    fn moved_while_left_button_is_down_becomes_a_drag_message() {
        let mut t = Translator::default();
        let moved = |column, row| {
            Event::Mouse(crossterm::event::MouseEvent {
                kind: MouseEventKind::Moved,
                column,
                row,
                modifiers: KeyModifiers::NONE,
            })
        };
        assert!(t.translate(moved(7, 3), 1, 1).is_none());

        assert!(matches!(
            t.translate(
                Event::Mouse(crossterm::event::MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: 7,
                    row: 3,
                    modifiers: KeyModifiers::NONE,
                }),
                1,
                1
            ),
            Some(Msg::MouseClick { .. })
        ));
        assert!(matches!(
            t.translate(moved(8, 4), 1, 1),
            Some(Msg::MouseDrag { col: 8, row: 4 })
        ));
        assert!(matches!(
            t.translate(
                Event::Mouse(crossterm::event::MouseEvent {
                    kind: MouseEventKind::Up(MouseButton::Left),
                    column: 8,
                    row: 4,
                    modifiers: KeyModifiers::NONE,
                }),
                1,
                1
            ),
            Some(Msg::MouseLeftUp)
        ));
        assert!(t.translate(moved(9, 5), 1, 1).is_none());
    }

    #[test]
    fn left_button_up_ends_drag_selection() {
        let mut t = Translator::default();
        let ev = Event::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 7,
            row: 3,
            modifiers: KeyModifiers::NONE,
        });
        assert!(matches!(t.translate(ev, 1, 1), Some(Msg::MouseLeftUp)));
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
            t.translate(wheel(MouseEventKind::ScrollUp), 1, 1),
            Some(Msg::MouseScroll {
                up: true,
                col: 4,
                row: 9,
                ctrl: false
            })
        ));
        assert!(matches!(
            t.translate(wheel(MouseEventKind::ScrollDown), 1, 1),
            Some(Msg::MouseScroll {
                up: false,
                col: 4,
                row: 9,
                ctrl: false
            })
        ));
    }

    #[test]
    fn ctrl_wheel_is_flagged_for_text_zoom() {
        let mut t = Translator::default();
        // Modifier encoded in the SGR mouse report itself.
        let ev = Event::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 4,
            row: 9,
            modifiers: KeyModifiers::CONTROL,
        });
        assert!(matches!(
            t.translate(ev, 1, 1),
            Some(Msg::MouseScroll { ctrl: true, .. })
        ));

        // Modifier known only from a held-Ctrl key event (enhanced keyboard protocol).
        assert!(
            t.translate(
                key(
                    KeyCode::Modifier(ModifierKeyCode::LeftControl),
                    KeyModifiers::CONTROL,
                    KeyEventKind::Press,
                ),
                1,
                1,
            )
            .is_none()
        );
        let ev = Event::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 4,
            row: 9,
            modifiers: KeyModifiers::NONE,
        });
        assert!(matches!(
            t.translate(ev, 1, 1),
            Some(Msg::MouseScroll { ctrl: true, .. })
        ));
    }

    #[test]
    fn modifier_click_becomes_a_toggle_click_and_never_a_double_click() {
        let mut t = Translator::default();
        let down = |modifiers| {
            Event::Mouse(crossterm::event::MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 10,
                row: 5,
                modifiers,
            })
        };
        // Ctrl encoded in the SGR mouse report itself.
        assert!(matches!(
            t.translate(down(KeyModifiers::CONTROL), 1, 1),
            Some(Msg::MouseClick {
                col: 10,
                row: 5,
                multi: true
            })
        ));
        // A second quick modifier press at the same cell stays a toggle click — a
        // Ctrl/Cmd click sequence must never be promoted to a double-click…
        assert!(matches!(
            t.translate(down(KeyModifiers::CONTROL), 1, 1),
            Some(Msg::MouseClick { multi: true, .. })
        ));
        // …and a quick plain press right after starts a fresh single click too.
        assert!(matches!(
            t.translate(down(KeyModifiers::NONE), 1, 1),
            Some(Msg::MouseClick { multi: false, .. })
        ));
    }

    #[test]
    fn held_cmd_key_flags_the_next_click_as_a_toggle() {
        // Cmd known only from a held-Super key event (enhanced keyboard protocol) —
        // macOS terminals never encode Cmd in the SGR mouse report.
        let mut t = Translator::default();
        assert!(
            t.translate(
                key(
                    KeyCode::Modifier(ModifierKeyCode::LeftSuper),
                    KeyModifiers::SUPER,
                    KeyEventKind::Press,
                ),
                1,
                1,
            )
            .is_none()
        );
        let down = Event::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 3,
            row: 4,
            modifiers: KeyModifiers::NONE,
        });
        assert!(matches!(
            t.translate(down.clone(), 1, 1),
            Some(Msg::MouseClick {
                col: 3,
                row: 4,
                multi: true
            })
        ));
        // Releasing Cmd restores plain clicks.
        assert!(
            t.translate(
                key(
                    KeyCode::Modifier(ModifierKeyCode::LeftSuper),
                    KeyModifiers::SUPER,
                    KeyEventKind::Release,
                ),
                1,
                1,
            )
            .is_none()
        );
        assert!(matches!(
            t.translate(down, 1, 1),
            Some(Msg::MouseClick {
                col: 3,
                row: 4,
                multi: false
            })
        ));
    }

    #[test]
    fn mouse_cells_map_onto_the_zoomed_virtual_grid() {
        let mut t = Translator::default();
        // Physical cell (9, 5) at scale 2 → virtual cell (4, 2).
        assert!(matches!(
            t.translate(
                Event::Mouse(crossterm::event::MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: 9,
                    row: 5,
                    modifiers: KeyModifiers::NONE,
                }),
                2,
                2,
            ),
            Some(Msg::MouseClick {
                col: 4,
                row: 2,
                multi: false
            })
        ));
        // A second press one physical cell over lands in the same virtual cell and must
        // count as a double-click — the whole point of dividing in the translator.
        assert!(matches!(
            t.translate(
                Event::Mouse(crossterm::event::MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: 8,
                    row: 4,
                    modifiers: KeyModifiers::NONE,
                }),
                2,
                2,
            ),
            Some(Msg::MouseDoubleClick { col: 4, row: 2 })
        ));
    }

    #[test]
    fn right_double_clicks_use_the_zoomed_virtual_grid() {
        let mut t = Translator::default();
        let down = |column, row| {
            Event::Mouse(crossterm::event::MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Right),
                column,
                row,
                modifiers: KeyModifiers::NONE,
            })
        };
        // These adjacent physical cells collapse onto the same virtual cell at 2x zoom.
        assert!(matches!(
            t.translate(down(9, 5), 2, 2),
            Some(Msg::MouseRightClick { col: 4, row: 2 })
        ));
        assert!(matches!(
            t.translate(down(8, 4), 2, 2),
            Some(Msg::MouseRightDoubleClick { col: 4, row: 2 })
        ));
    }

    #[test]
    fn decdhl_mouse_keeps_the_column_logical_and_halves_the_row() {
        // Large-text (DECDHL) mode is geometrically asymmetric: each virtual row is two
        // physical rows (ESC#3/ESC#4) but the column is addressed logically, so
        // `ZoomHandle::mouse_scale()` is (col=1, row=2). Dividing the column too — as a
        // single scalar would — lands clicks half a screen left of the glyph under the
        // pointer, which was the large-text mouse bug on Windows Terminal.
        let mut t = Translator::default();
        let down = |column, row| {
            Event::Mouse(crossterm::event::MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column,
                row,
                modifiers: KeyModifiers::NONE,
            })
        };
        // Physical (40, 10) → virtual (40, 5): column unchanged, only the row halved.
        assert!(matches!(
            t.translate(down(40, 10), 1, 2),
            Some(Msg::MouseClick {
                col: 40,
                row: 5,
                multi: false
            })
        ));
        // A second press on the other physical half of the same virtual row (row 11 → 5) at
        // the same column is a double-click — proof that only the row collapses.
        assert!(matches!(
            t.translate(down(40, 11), 1, 2),
            Some(Msg::MouseDoubleClick { col: 40, row: 5 })
        ));
    }

    #[test]
    fn held_shift_modifier_is_applied_to_following_key_events() {
        let mut input = Translator::default();
        assert!(
            input
                .translate(
                    key(
                        KeyCode::Modifier(ModifierKeyCode::LeftShift),
                        KeyModifiers::SHIFT,
                        KeyEventKind::Press,
                    ),
                    1,
                    1
                )
                .is_none()
        );

        let Some(Msg::Key(k)) = input.translate(
            key(KeyCode::Char('ㅣ'), KeyModifiers::NONE, KeyEventKind::Press),
            1,
            1,
        ) else {
            panic!("expected a key message");
        };
        assert!(k.modifiers.contains(KeyModifiers::SHIFT));

        assert!(
            input
                .translate(
                    key(
                        KeyCode::Modifier(ModifierKeyCode::LeftShift),
                        KeyModifiers::SHIFT,
                        KeyEventKind::Release,
                    ),
                    1,
                    1
                )
                .is_none()
        );

        let Some(Msg::Key(k)) = input.translate(
            key(KeyCode::Char('ㅣ'), KeyModifiers::NONE, KeyEventKind::Press),
            1,
            1,
        ) else {
            panic!("expected a key message");
        };
        assert!(!k.modifiers.contains(KeyModifiers::SHIFT));
    }
}
