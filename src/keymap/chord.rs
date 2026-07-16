use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MediaKeyCode, ModifierKeyCode};

use super::Action;

/// A normalized key combination: a [`KeyCode`] plus the ctrl/alt/shift modifiers.
///
/// Equality is normalized so terminal quirks don't cause misses: 2-beolsik Korean IME
/// jamo are mapped back to their physical QWERTY keys, plain shifted `Char` keys are
/// represented by the produced character (an uppercase `'L'` already encodes shift), while
/// Ctrl/Alt character chords keep `SHIFT` so `Ctrl+X` and `Ctrl+Shift+X` remain distinct.
/// Ctrl/Alt letters ignore case, and `Shift+Tab` collapses to `BackTab`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Chord {
    pub code: KeyCode,
    pub mods: KeyModifiers,
}

impl Chord {
    pub fn new(code: KeyCode, mods: KeyModifiers) -> Self {
        let mut mods = mods & (KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT);
        // Normalize Shift+Tab → BackTab (terminals report either).
        let mut code = if code == KeyCode::Tab && mods.contains(KeyModifiers::SHIFT) {
            KeyCode::BackTab
        } else {
            code
        };
        let modified_char = mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        if let KeyCode::Char(c) = code
            && let Some(latin) = korean_2set_key(c)
        {
            if modified_char {
                mods.set(
                    KeyModifiers::SHIFT,
                    mods.contains(KeyModifiers::SHIFT) || latin.is_ascii_uppercase(),
                );
                code = KeyCode::Char(latin.to_ascii_lowercase());
            } else {
                code = KeyCode::Char(
                    if mods.contains(KeyModifiers::SHIFT) && latin.is_ascii_lowercase() {
                        latin.to_ascii_uppercase()
                    } else {
                        latin
                    },
                );
            }
        }
        if let KeyCode::Char(c) = code
            && !modified_char
            && mods.contains(KeyModifiers::SHIFT)
            && c.is_ascii_lowercase()
        {
            code = KeyCode::Char(c.to_ascii_uppercase());
        }
        // Plain char case already encodes shift; BackTab is inherently shifted. Preserve
        // Shift on Ctrl/Alt chars so enhanced terminals can bind Ctrl+Shift+letter separately.
        if matches!(code, KeyCode::BackTab) || (matches!(code, KeyCode::Char(_)) && !modified_char)
        {
            mods.remove(KeyModifiers::SHIFT);
        }
        // Terminals can report Ctrl+Q as either Char('q') or Char('Q'); persisted chord
        // labels use lowercase modifiers, so normalize modified ASCII letters.
        if let KeyCode::Char(c) = code
            && mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
            && c.is_ascii_alphabetic()
        {
            code = KeyCode::Char(c.to_ascii_lowercase());
        }
        Chord { code, mods }
    }

    /// Whether this chord would normally produce a typed character (so it must not be
    /// swallowed as a command while a text field is focused).
    pub fn is_typeable(self) -> bool {
        matches!(self.code, KeyCode::Char(_))
            && !self.mods.contains(KeyModifiers::CONTROL)
            && !self.mods.contains(KeyModifiers::ALT)
    }
}

fn korean_2set_key(c: char) -> Option<char> {
    Some(match c {
        'ㅂ' => 'q',
        'ㅈ' => 'w',
        'ㄷ' => 'e',
        'ㄱ' => 'r',
        'ㅅ' => 't',
        'ㅛ' => 'y',
        'ㅕ' => 'u',
        'ㅑ' => 'i',
        'ㅐ' => 'o',
        'ㅔ' => 'p',
        'ㅁ' => 'a',
        'ㄴ' => 's',
        'ㅇ' => 'd',
        'ㄹ' => 'f',
        'ㅎ' => 'g',
        'ㅗ' => 'h',
        'ㅓ' => 'j',
        'ㅏ' => 'k',
        'ㅣ' => 'l',
        'ㅋ' => 'z',
        'ㅌ' => 'x',
        'ㅊ' => 'c',
        'ㅍ' => 'v',
        'ㅠ' => 'b',
        'ㅜ' => 'n',
        'ㅡ' => 'm',
        'ㅃ' => 'Q',
        'ㅉ' => 'W',
        'ㄸ' => 'E',
        'ㄲ' => 'R',
        'ㅆ' => 'T',
        'ㅒ' => 'O',
        'ㅖ' => 'P',
        _ => return None,
    })
}

impl From<KeyEvent> for Chord {
    fn from(k: KeyEvent) -> Self {
        Chord::new(k.code, k.modifiers)
    }
}

/// Parse a config chord string like `"space"`, `"ctrl+n"`, `"L"`, `">"` into a [`Chord`].
pub fn parse_chord(s: &str) -> Option<Chord> {
    let mut rest = s.trim();
    let mut mods = KeyModifiers::empty();
    loop {
        if let Some(r) = strip_ci(rest, "ctrl+").or_else(|| strip_ci(rest, "control+")) {
            mods |= KeyModifiers::CONTROL;
            rest = r;
        } else if let Some(r) = strip_ci(rest, "alt+") {
            mods |= KeyModifiers::ALT;
            rest = r;
        } else if let Some(r) = strip_ci(rest, "shift+") {
            mods |= KeyModifiers::SHIFT;
            rest = r;
        } else {
            break;
        }
    }
    parse_code(rest).map(|code| Chord::new(code, mods))
}

fn strip_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    if s.get(..prefix.len())
        .is_some_and(|p| p.eq_ignore_ascii_case(prefix))
    {
        Some(&s[prefix.len()..])
    } else {
        None
    }
}

fn parse_code(t: &str) -> Option<KeyCode> {
    let lower = t.to_ascii_lowercase();
    let code = match lower.as_str() {
        "space" => KeyCode::Char(' '),
        "enter" | "return" => KeyCode::Enter,
        "esc" | "escape" => KeyCode::Esc,
        "tab" => KeyCode::Tab,
        "backtab" => KeyCode::BackTab,
        "backspace" | "bs" => KeyCode::Backspace,
        "delete" | "del" => KeyCode::Delete,
        "insert" | "ins" => KeyCode::Insert,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" | "pgup" => KeyCode::PageUp,
        "pagedown" | "pgdn" => KeyCode::PageDown,
        "null" => KeyCode::Null,
        "capslock" | "caps_lock" => KeyCode::CapsLock,
        "scrolllock" | "scroll_lock" => KeyCode::ScrollLock,
        "numlock" | "num_lock" => KeyCode::NumLock,
        "printscreen" | "print_screen" | "prtsc" => KeyCode::PrintScreen,
        "pause" => KeyCode::Pause,
        "menu" => KeyCode::Menu,
        "keypadbegin" | "keypad_begin" | "begin" => KeyCode::KeypadBegin,
        "media_play" => KeyCode::Media(MediaKeyCode::Play),
        "media_pause" => KeyCode::Media(MediaKeyCode::Pause),
        "media_play_pause" | "media_playpause" => KeyCode::Media(MediaKeyCode::PlayPause),
        "media_reverse" => KeyCode::Media(MediaKeyCode::Reverse),
        "media_stop" => KeyCode::Media(MediaKeyCode::Stop),
        "media_fast_forward" | "media_fastforward" => KeyCode::Media(MediaKeyCode::FastForward),
        "media_rewind" => KeyCode::Media(MediaKeyCode::Rewind),
        "media_track_next" | "media_next" => KeyCode::Media(MediaKeyCode::TrackNext),
        "media_track_previous" | "media_previous" | "media_prev" => {
            KeyCode::Media(MediaKeyCode::TrackPrevious)
        }
        "media_record" => KeyCode::Media(MediaKeyCode::Record),
        "media_lower_volume" | "media_volume_down" => KeyCode::Media(MediaKeyCode::LowerVolume),
        "media_raise_volume" | "media_volume_up" => KeyCode::Media(MediaKeyCode::RaiseVolume),
        "media_mute_volume" | "media_mute" => KeyCode::Media(MediaKeyCode::MuteVolume),
        "left_shift" => KeyCode::Modifier(ModifierKeyCode::LeftShift),
        "left_ctrl" | "left_control" => KeyCode::Modifier(ModifierKeyCode::LeftControl),
        "left_alt" => KeyCode::Modifier(ModifierKeyCode::LeftAlt),
        "left_super" => KeyCode::Modifier(ModifierKeyCode::LeftSuper),
        "left_hyper" => KeyCode::Modifier(ModifierKeyCode::LeftHyper),
        "left_meta" => KeyCode::Modifier(ModifierKeyCode::LeftMeta),
        "right_shift" => KeyCode::Modifier(ModifierKeyCode::RightShift),
        "right_ctrl" | "right_control" => KeyCode::Modifier(ModifierKeyCode::RightControl),
        "right_alt" => KeyCode::Modifier(ModifierKeyCode::RightAlt),
        "right_super" => KeyCode::Modifier(ModifierKeyCode::RightSuper),
        "right_hyper" => KeyCode::Modifier(ModifierKeyCode::RightHyper),
        "right_meta" => KeyCode::Modifier(ModifierKeyCode::RightMeta),
        "iso_level3_shift" | "iso_level_3_shift" => {
            KeyCode::Modifier(ModifierKeyCode::IsoLevel3Shift)
        }
        "iso_level5_shift" | "iso_level_5_shift" => {
            KeyCode::Modifier(ModifierKeyCode::IsoLevel5Shift)
        }
        _ => {
            if let Some(n) = lower.strip_prefix('f').and_then(|d| d.parse::<u8>().ok())
                && (1..=12).contains(&n)
            {
                KeyCode::F(n)
            } else {
                // A single literal character, taking the *original* case (so `L` ≠ `l`).
                let mut chars = t.chars();
                let c = chars.next()?;
                if chars.next().is_some() {
                    return None;
                }
                KeyCode::Char(c)
            }
        }
    };
    Some(code)
}

/// The canonical persisted form of a chord (inverse of [`parse_chord`]).
pub fn chord_to_config(chord: Chord) -> String {
    let mut s = String::new();
    if chord.mods.contains(KeyModifiers::CONTROL) {
        s.push_str("ctrl+");
    }
    if chord.mods.contains(KeyModifiers::ALT) {
        s.push_str("alt+");
    }
    if chord.mods.contains(KeyModifiers::SHIFT) {
        s.push_str("shift+");
    }
    match chord.code {
        KeyCode::Char(' ') => s.push_str("space"),
        KeyCode::Char(c) => s.push(c),
        KeyCode::F(n) => s.push_str(&format!("f{n}")),
        other => s.push_str(code_token(other)),
    }
    s
}

/// Convert a TUI chord into mpv `input.conf` key-name syntax for the video overlay.
/// Unsupported terminal-only keys return `None` so Settings can reject them up front.
pub fn chord_to_mpv_input(chord: Chord) -> Option<String> {
    let (base, inherent_shift) = match chord.code {
        KeyCode::Char(' ') => ("SPACE".to_owned(), false),
        KeyCode::Char(c) if c.is_ascii() && !c.is_ascii_control() => (c.to_string(), false),
        KeyCode::Esc => ("ESC".to_owned(), false),
        KeyCode::Left => ("LEFT".to_owned(), false),
        KeyCode::Right => ("RIGHT".to_owned(), false),
        KeyCode::Up => ("UP".to_owned(), false),
        KeyCode::Down => ("DOWN".to_owned(), false),
        KeyCode::Enter => ("ENTER".to_owned(), false),
        KeyCode::Tab => ("TAB".to_owned(), false),
        KeyCode::BackTab => ("TAB".to_owned(), true),
        KeyCode::Backspace => ("BS".to_owned(), false),
        KeyCode::Delete => ("DEL".to_owned(), false),
        KeyCode::Home => ("HOME".to_owned(), false),
        KeyCode::End => ("END".to_owned(), false),
        KeyCode::PageUp => ("PGUP".to_owned(), false),
        KeyCode::PageDown => ("PGDWN".to_owned(), false),
        KeyCode::F(n) if (1..=12).contains(&n) => (format!("F{n}"), false),
        _ => return None,
    };
    let mut out = String::new();
    if chord.mods.contains(KeyModifiers::CONTROL) {
        out.push_str("Ctrl+");
    }
    if chord.mods.contains(KeyModifiers::ALT) {
        out.push_str("Alt+");
    }
    if inherent_shift || chord.mods.contains(KeyModifiers::SHIFT) {
        out.push_str("Shift+");
    }
    out.push_str(&base);
    Some(out)
}

/// Fixed mpv-compatibility aliases that remain active in the overlay even though the
/// primary displayed bindings are the remappable YuTuTui defaults.
pub fn mpv_overlay_fixed_alias(chord: Chord) -> Option<Action> {
    let ch = |c| Chord::new(KeyCode::Char(c), KeyModifiers::empty());
    if chord == ch('p') {
        Some(Action::VideoTogglePause)
    } else if chord == ch('>') {
        Some(Action::VideoNext)
    } else if chord == ch('<') {
        Some(Action::VideoPrev)
    } else {
        None
    }
}

fn code_token(code: KeyCode) -> &'static str {
    match code {
        KeyCode::Enter => "enter",
        KeyCode::Esc => "esc",
        KeyCode::Tab => "tab",
        KeyCode::BackTab => "backtab",
        KeyCode::Backspace => "backspace",
        KeyCode::Delete => "delete",
        KeyCode::Insert => "insert",
        KeyCode::Up => "up",
        KeyCode::Down => "down",
        KeyCode::Left => "left",
        KeyCode::Right => "right",
        KeyCode::Home => "home",
        KeyCode::End => "end",
        KeyCode::PageUp => "pageup",
        KeyCode::PageDown => "pagedown",
        KeyCode::Null => "null",
        KeyCode::CapsLock => "capslock",
        KeyCode::ScrollLock => "scrolllock",
        KeyCode::NumLock => "numlock",
        KeyCode::PrintScreen => "printscreen",
        KeyCode::Pause => "pause",
        KeyCode::Menu => "menu",
        KeyCode::KeypadBegin => "keypadbegin",
        KeyCode::Media(media) => media_token(media),
        KeyCode::Modifier(modifier) => modifier_token(modifier),
        KeyCode::F(_) | KeyCode::Char(_) => "?",
    }
}

fn media_token(media: MediaKeyCode) -> &'static str {
    match media {
        MediaKeyCode::Play => "media_play",
        MediaKeyCode::Pause => "media_pause",
        MediaKeyCode::PlayPause => "media_play_pause",
        MediaKeyCode::Reverse => "media_reverse",
        MediaKeyCode::Stop => "media_stop",
        MediaKeyCode::FastForward => "media_fast_forward",
        MediaKeyCode::Rewind => "media_rewind",
        MediaKeyCode::TrackNext => "media_track_next",
        MediaKeyCode::TrackPrevious => "media_track_previous",
        MediaKeyCode::Record => "media_record",
        MediaKeyCode::LowerVolume => "media_lower_volume",
        MediaKeyCode::RaiseVolume => "media_raise_volume",
        MediaKeyCode::MuteVolume => "media_mute_volume",
    }
}

fn modifier_token(modifier: ModifierKeyCode) -> &'static str {
    match modifier {
        ModifierKeyCode::LeftShift => "left_shift",
        ModifierKeyCode::LeftControl => "left_ctrl",
        ModifierKeyCode::LeftAlt => "left_alt",
        ModifierKeyCode::LeftSuper => "left_super",
        ModifierKeyCode::LeftHyper => "left_hyper",
        ModifierKeyCode::LeftMeta => "left_meta",
        ModifierKeyCode::RightShift => "right_shift",
        ModifierKeyCode::RightControl => "right_ctrl",
        ModifierKeyCode::RightAlt => "right_alt",
        ModifierKeyCode::RightSuper => "right_super",
        ModifierKeyCode::RightHyper => "right_hyper",
        ModifierKeyCode::RightMeta => "right_meta",
        ModifierKeyCode::IsoLevel3Shift => "iso_level3_shift",
        ModifierKeyCode::IsoLevel5Shift => "iso_level5_shift",
    }
}
