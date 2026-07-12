use crate::theme::ThemeRole;

pub struct MascotAsset {
    pub name: &'static str,
    pub width: u16,
    pub height: u16,
    pub fps: u16,
    pub looped: bool,
    pub frames: &'static [MascotFrame],
    pub fallback: Option<&'static MascotAsset>,
    /// Rectangular color overlays, checked in order (first hit wins); cells outside every
    /// region keep the frame's base style. Asset-level (not per-frame) on purpose: the
    /// groove tests pin nonblank bounds stable across frames, so one map fits them all.
    pub regions: &'static [MascotRegion],
}

/// A rectangle of the art (asset-local cell coordinates) drawn in its own theme style,
/// so a mascot can carry per-part colors while the frame data stays plain strings.
pub struct MascotRegion {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
    pub style: MascotStyle,
    pub bold: bool,
}

impl MascotRegion {
    pub fn contains(&self, x: u16, y: u16) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }
}

pub struct MascotFrame {
    pub hold: u16,
    pub lines: &'static [&'static str],
    pub style: MascotStyle,
}

#[derive(Clone, Copy)]
pub enum MascotStyle {
    Theme(ThemeRole),
    Accent,
    Muted,
    Thinking,
    Error,
}
