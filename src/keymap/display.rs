use crossterm::event::{KeyCode, KeyModifiers, MediaKeyCode, ModifierKeyCode};

use super::Chord;

pub fn format_chord_for_display(chord: Chord, retro: bool) -> String {
    if retro {
        format_chord_retro(chord)
    } else {
        format_chord(chord)
    }
}

/// Render a chord as a compact human-readable label for footers / cheat-sheet:
/// `␣`, `←/→/↑/↓`, `Enter`, `Esc`, `Tab`, `^r`, `M-x`, etc.
pub fn format_chord(chord: Chord) -> String {
    let mut s = String::new();
    if chord.mods.contains(KeyModifiers::CONTROL) {
        s.push('^');
    }
    if chord.mods.contains(KeyModifiers::ALT) {
        s.push_str("M-");
    }
    if chord.mods.contains(KeyModifiers::SHIFT) {
        s.push('⇧');
    }
    if matches!(chord.code, KeyCode::Char(c) if c.is_ascii_uppercase())
        && !chord
            .mods
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT)
    {
        s.push('⇧');
    }
    match chord.code {
        KeyCode::Char(' ') => s.push('␣'),
        KeyCode::Char(c) => s.push(c),
        KeyCode::Left => s.push('←'),
        KeyCode::Right => s.push('→'),
        KeyCode::Up => s.push('↑'),
        KeyCode::Down => s.push('↓'),
        KeyCode::Enter => s.push_str("Enter"),
        KeyCode::Esc => s.push_str("Esc"),
        KeyCode::Tab => s.push_str("Tab"),
        KeyCode::BackTab => s.push_str("⇧Tab"),
        KeyCode::Backspace => s.push('⌫'),
        KeyCode::Delete => s.push_str("Del"),
        KeyCode::Insert => s.push_str("Ins"),
        KeyCode::Home => s.push_str("Home"),
        KeyCode::End => s.push_str("End"),
        KeyCode::PageUp => s.push_str("PgUp"),
        KeyCode::PageDown => s.push_str("PgDn"),
        KeyCode::F(n) => s.push_str(&format!("F{n}")),
        KeyCode::Null => s.push_str("Null"),
        KeyCode::CapsLock => s.push_str("Caps"),
        KeyCode::ScrollLock => s.push_str("Scroll"),
        KeyCode::NumLock => s.push_str("Num"),
        KeyCode::PrintScreen => s.push_str("PrtSc"),
        KeyCode::Pause => s.push_str("Pause"),
        KeyCode::Menu => s.push_str("Menu"),
        KeyCode::KeypadBegin => s.push_str("Begin"),
        KeyCode::Media(media) => s.push_str(media_label(media)),
        KeyCode::Modifier(modifier) => s.push_str(modifier_label(modifier)),
    }
    s
}

/// Retro-mode key labels avoid glyphs outside the 256-cell console set. This keeps the
/// key editor and help sheet readable after the final retro frame scrubber runs.
pub fn format_chord_retro(chord: Chord) -> String {
    let mut parts = Vec::new();
    if chord.mods.contains(KeyModifiers::CONTROL) {
        parts.push("Ctrl".to_owned());
    }
    if chord.mods.contains(KeyModifiers::ALT) {
        parts.push("Alt".to_owned());
    }
    if chord.mods.contains(KeyModifiers::SHIFT) {
        parts.push("Shift".to_owned());
    }
    if matches!(chord.code, KeyCode::Char(c) if c.is_ascii_uppercase())
        && !chord
            .mods
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT)
    {
        parts.push("Shift".to_owned());
    }
    if chord.code == KeyCode::BackTab {
        if !chord.mods.contains(KeyModifiers::SHIFT) {
            parts.push("Shift".to_owned());
        }
        parts.push("Tab".to_owned());
    } else {
        parts.push(retro_key_label(
            chord.code,
            chord.mods.contains(KeyModifiers::SHIFT)
                || chord
                    .mods
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT),
        ));
    }
    parts.join("+")
}

fn retro_key_label(code: KeyCode, shifted: bool) -> String {
    match code {
        KeyCode::Char(' ') => "Space".to_owned(),
        KeyCode::Char('+') => "Plus".to_owned(),
        KeyCode::Char(c) if shifted && c.is_ascii_alphabetic() => {
            c.to_ascii_uppercase().to_string()
        }
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Left => "Left".to_owned(),
        KeyCode::Right => "Right".to_owned(),
        KeyCode::Up => "Up".to_owned(),
        KeyCode::Down => "Down".to_owned(),
        KeyCode::Enter => "Enter".to_owned(),
        KeyCode::Esc => "Esc".to_owned(),
        KeyCode::Tab => "Tab".to_owned(),
        KeyCode::BackTab => "Shift+Tab".to_owned(),
        KeyCode::Backspace => "Backspace".to_owned(),
        KeyCode::Delete => "Delete".to_owned(),
        KeyCode::Insert => "Insert".to_owned(),
        KeyCode::Home => "Home".to_owned(),
        KeyCode::End => "End".to_owned(),
        KeyCode::PageUp => "PageUp".to_owned(),
        KeyCode::PageDown => "PageDown".to_owned(),
        KeyCode::F(n) => format!("F{n}"),
        KeyCode::Null => "Null".to_owned(),
        KeyCode::CapsLock => "CapsLock".to_owned(),
        KeyCode::ScrollLock => "ScrollLock".to_owned(),
        KeyCode::NumLock => "NumLock".to_owned(),
        KeyCode::PrintScreen => "PrintScreen".to_owned(),
        KeyCode::Pause => "Pause".to_owned(),
        KeyCode::Menu => "Menu".to_owned(),
        KeyCode::KeypadBegin => "KeypadBegin".to_owned(),
        KeyCode::Media(media) => media_label(media).replace(' ', ""),
        KeyCode::Modifier(modifier) => modifier_label(modifier).replace(' ', ""),
    }
}

fn media_label(media: MediaKeyCode) -> &'static str {
    match media {
        MediaKeyCode::Play => "Play",
        MediaKeyCode::Pause => "Pause",
        MediaKeyCode::PlayPause => "Play/Pause",
        MediaKeyCode::Reverse => "Reverse",
        MediaKeyCode::Stop => "Stop",
        MediaKeyCode::FastForward => "Fast Forward",
        MediaKeyCode::Rewind => "Rewind",
        MediaKeyCode::TrackNext => "Next Track",
        MediaKeyCode::TrackPrevious => "Previous Track",
        MediaKeyCode::Record => "Record",
        MediaKeyCode::LowerVolume => "Lower Volume",
        MediaKeyCode::RaiseVolume => "Raise Volume",
        MediaKeyCode::MuteVolume => "Mute Volume",
    }
}

fn modifier_label(modifier: ModifierKeyCode) -> &'static str {
    match modifier {
        ModifierKeyCode::LeftShift => "Left Shift",
        ModifierKeyCode::LeftControl => "Left Ctrl",
        ModifierKeyCode::LeftAlt => "Left Alt",
        ModifierKeyCode::LeftSuper => "Left Super",
        ModifierKeyCode::LeftHyper => "Left Hyper",
        ModifierKeyCode::LeftMeta => "Left Meta",
        ModifierKeyCode::RightShift => "Right Shift",
        ModifierKeyCode::RightControl => "Right Ctrl",
        ModifierKeyCode::RightAlt => "Right Alt",
        ModifierKeyCode::RightSuper => "Right Super",
        ModifierKeyCode::RightHyper => "Right Hyper",
        ModifierKeyCode::RightMeta => "Right Meta",
        ModifierKeyCode::IsoLevel3Shift => "Iso Level 3 Shift",
        ModifierKeyCode::IsoLevel5Shift => "Iso Level 5 Shift",
    }
}
