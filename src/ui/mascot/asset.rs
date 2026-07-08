use crate::theme::ThemeRole;

pub struct MascotAsset {
    pub name: &'static str,
    pub width: u16,
    pub height: u16,
    pub fps: u16,
    pub looped: bool,
    pub frames: &'static [MascotFrame],
    pub fallback: Option<&'static MascotAsset>,
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
