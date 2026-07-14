//! Settings color-picker state and the terminal-safe xterm palette.

use std::cell::Cell;

use crate::theme::ThemeRole;

/// Number of real colors in the picker: the 216-color RGB cube plus 24 grays.
pub const COLOR_PICKER_COLOR_COUNT: usize = 240;
/// A transparent choice precedes the 240 real colors in the navigable grid.
pub const COLOR_PICKER_CHOICE_COUNT: usize = COLOR_PICKER_COLOR_COUNT + 1;

/// A selectable value in the picker grid. The current value is a separate header choice so an
/// arbitrary 24-bit color (or `none`) can be accepted without being coerced into the palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorPickerChoice {
    Transparent,
    Rgb(u8, u8, u8),
}

impl ColorPickerChoice {
    pub fn value(self) -> String {
        match self {
            Self::Transparent => "none".to_owned(),
            Self::Rgb(r, g, b) => format!("#{r:02X}{g:02X}{b:02X}"),
        }
    }
}

/// Return one grid choice: transparent first, then the xterm RGB cube and gray ramp.
pub fn color_picker_choice(index: usize) -> Option<ColorPickerChoice> {
    if index == 0 {
        return Some(ColorPickerChoice::Transparent);
    }
    let color = index - 1;
    if color < 216 {
        const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
        let r = LEVELS[color / 36];
        let g = LEVELS[(color / 6) % 6];
        let b = LEVELS[color % 6];
        Some(ColorPickerChoice::Rgb(r, g, b))
    } else if color < COLOR_PICKER_COLOR_COUNT {
        let gray = 8 + 10 * (color - 216) as u8;
        Some(ColorPickerChoice::Rgb(gray, gray, gray))
    } else {
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorPickerSelection {
    Current,
    Choice(usize),
}

/// Modal picker state owned by the Settings session. Layout facts are bridged from render with
/// `Cell`s because only the render pass knows the terminal's current virtual cell grid.
#[derive(Debug, Clone)]
pub struct ColorPickerState {
    pub role: ThemeRole,
    current: String,
    selection: ColorPickerSelection,
    columns: Cell<usize>,
    visible_rows: Cell<usize>,
    row_offset: Cell<usize>,
}

impl ColorPickerState {
    pub fn new(role: ThemeRole, current: String) -> Self {
        Self {
            role,
            current,
            selection: ColorPickerSelection::Current,
            columns: Cell::new(1),
            visible_rows: Cell::new(1),
            row_offset: Cell::new(0),
        }
    }

    pub fn current(&self) -> &str {
        &self.current
    }

    pub fn selection(&self) -> ColorPickerSelection {
        self.selection
    }

    pub fn selected_value(&self) -> String {
        match self.selection {
            ColorPickerSelection::Current => self.current.clone(),
            ColorPickerSelection::Choice(index) => color_picker_choice(index)
                .expect("picker selection is always in range")
                .value(),
        }
    }

    pub fn select_current(&mut self) {
        self.selection = ColorPickerSelection::Current;
        self.ensure_visible();
    }

    pub fn select_choice(&mut self, index: usize) {
        self.selection = ColorPickerSelection::Choice(index.min(COLOR_PICKER_CHOICE_COUNT - 1));
        self.ensure_visible();
    }

    pub fn set_layout(&self, columns: usize, visible_rows: usize) {
        self.columns.set(columns.max(1));
        self.visible_rows.set(visible_rows.max(1));
        self.ensure_visible();
    }

    pub fn columns(&self) -> usize {
        self.columns.get().max(1)
    }

    pub fn visible_rows(&self) -> usize {
        self.visible_rows.get().max(1)
    }

    pub fn row_offset(&self) -> usize {
        self.row_offset.get()
    }

    pub fn choice_rows(&self) -> usize {
        COLOR_PICKER_CHOICE_COUNT.div_ceil(self.columns())
    }

    pub fn move_left(&mut self) {
        match self.selection {
            ColorPickerSelection::Current => self.select_choice(COLOR_PICKER_CHOICE_COUNT - 1),
            ColorPickerSelection::Choice(0) => self.select_current(),
            ColorPickerSelection::Choice(index) => self.select_choice(index - 1),
        }
    }

    pub fn move_right(&mut self) {
        match self.selection {
            ColorPickerSelection::Current => self.select_choice(0),
            ColorPickerSelection::Choice(index) => {
                self.select_choice((index + 1).min(COLOR_PICKER_CHOICE_COUNT - 1));
            }
        }
    }

    pub fn move_up(&mut self) {
        match self.selection {
            ColorPickerSelection::Current => self.select_choice(COLOR_PICKER_CHOICE_COUNT - 1),
            ColorPickerSelection::Choice(index) if index < self.columns() => {
                self.select_current();
            }
            ColorPickerSelection::Choice(index) => self.select_choice(index - self.columns()),
        }
    }

    pub fn move_down(&mut self) {
        match self.selection {
            ColorPickerSelection::Current => self.select_choice(0),
            ColorPickerSelection::Choice(index) => {
                self.select_choice((index + self.columns()).min(COLOR_PICKER_CHOICE_COUNT - 1))
            }
        }
    }

    pub fn wheel(&mut self, up: bool, rows: usize) {
        for _ in 0..rows.max(1) {
            if up {
                self.move_up();
            } else {
                self.move_down();
            }
        }
    }

    fn ensure_visible(&self) {
        let max_offset = self.choice_rows().saturating_sub(self.visible_rows());
        let next = match self.selection {
            ColorPickerSelection::Current => 0,
            ColorPickerSelection::Choice(index) => {
                let selected_row = index / self.columns();
                let current = self.row_offset.get().min(max_offset);
                if selected_row < current {
                    selected_row
                } else if selected_row >= current + self.visible_rows() {
                    selected_row + 1 - self.visible_rows()
                } else {
                    current
                }
            }
        };
        self.row_offset.set(next.min(max_offset));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_is_exact_xterm_cube_then_gray_ramp() {
        assert_eq!(COLOR_PICKER_COLOR_COUNT, 216 + 24);
        assert_eq!(color_picker_choice(0), Some(ColorPickerChoice::Transparent));
        assert_eq!(
            color_picker_choice(1),
            Some(ColorPickerChoice::Rgb(0, 0, 0))
        );
        assert_eq!(
            color_picker_choice(216),
            Some(ColorPickerChoice::Rgb(255, 255, 255))
        );
        assert_eq!(
            color_picker_choice(217),
            Some(ColorPickerChoice::Rgb(8, 8, 8))
        );
        assert_eq!(
            color_picker_choice(240),
            Some(ColorPickerChoice::Rgb(238, 238, 238))
        );
        assert_eq!(color_picker_choice(241), None);
    }

    #[test]
    fn current_value_is_lossless_until_navigation_changes_the_selection() {
        let mut picker = ColorPickerState::new(ThemeRole::Accent, "#123456".to_owned());
        picker.set_layout(18, 14);
        assert_eq!(picker.selected_value(), "#123456");
        picker.move_down();
        assert_eq!(picker.selected_value(), "none");

        let transparent = ColorPickerState::new(ThemeRole::Background, "none".to_owned());
        assert_eq!(transparent.selected_value(), "none");
        assert_eq!(transparent.selection(), ColorPickerSelection::Current);
    }

    #[test]
    fn navigation_tracks_selection_in_a_narrow_viewport() {
        let mut picker = ColorPickerState::new(ThemeRole::Accent, "#ABCDEF".to_owned());
        picker.set_layout(4, 3);
        for _ in 0..5 {
            picker.move_down();
        }
        assert_eq!(picker.selection(), ColorPickerSelection::Choice(16));
        assert_eq!(picker.row_offset(), 2);
        picker.move_up();
        picker.move_up();
        picker.move_up();
        picker.move_up();
        picker.move_up();
        assert_eq!(picker.selection(), ColorPickerSelection::Current);
        assert_eq!(picker.row_offset(), 0);
    }
}
