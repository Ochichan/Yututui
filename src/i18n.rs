//! Lightweight runtime internationalization.
//!
//! A single process-wide language (set once at startup from [`crate::config::Config`], and
//! again whenever the user changes the Settings → General language dropdown) drives a
//! [`t!`](crate::t) macro that returns the right `&'static str`. Keeping both the English and
//! Korean strings side-by-side at each call site keeps translations reviewable and — crucially
//! — avoids threading a language parameter through every `label()`/render function. The few
//! `format!` sites that can't wrap a string literal pick a whole translated string with
//! [`is_korean`] instead.

use std::sync::atomic::{AtomicU8, Ordering};

use serde::{Deserialize, Serialize};

/// The UI language. `English` is the default; the value persists in `config.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Language {
    #[default]
    English,
    Korean,
}

impl Language {
    /// All languages in the settings dropdown order.
    pub const CYCLE: [Language; 2] = [Language::English, Language::Korean];

    /// The language's own native name, shown in the settings dropdown. Never translated —
    /// each language names itself the same way regardless of the active UI language.
    pub fn native_name(self) -> &'static str {
        match self {
            Language::English => "English",
            Language::Korean => "한국어",
        }
    }

    /// The next language when stepping the dropdown forward/backward (wraps both ways).
    pub fn cycled(self, forward: bool) -> Self {
        let i = Self::CYCLE.iter().position(|&l| l == self).unwrap_or(0);
        let n = Self::CYCLE.len();
        let j = if forward {
            (i + 1) % n
        } else {
            (i + n - 1) % n
        };
        Self::CYCLE[j]
    }

    fn from_u8(v: u8) -> Self {
        match v {
            1 => Language::Korean,
            _ => Language::English,
        }
    }
}

/// The language DJ Gem replies in, set in Settings → DJ Gem *independently* of the UI
/// [`Language`]. [`Auto`](Self::Auto) reproduces the historical behavior — it follows the UI
/// language (Korean UI → Korean replies, otherwise the model answers in whatever language the
/// user writes in). Each concrete variant forces that language regardless of what the user
/// types. Retro mode overrides all of this to English; that resolution happens once in
/// [`crate::config::Config::effective_dj_gem_language`], and the *resolved* value is what the
/// AI actor reads back via [`dj_gem_language`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DjGemLanguage {
    #[default]
    Auto,
    English,
    Korean,
    Japanese,
    ChineseSimplified,
    ChineseTraditional,
}

impl DjGemLanguage {
    /// All choices in the settings dropdown order (Auto first, then concrete languages).
    pub const CYCLE: [DjGemLanguage; 6] = [
        DjGemLanguage::Auto,
        DjGemLanguage::English,
        DjGemLanguage::Korean,
        DjGemLanguage::Japanese,
        DjGemLanguage::ChineseSimplified,
        DjGemLanguage::ChineseTraditional,
    ];

    /// The label shown in the settings picker. Only [`Auto`](Self::Auto) is translated (it
    /// isn't a language); every concrete language names itself in its own script, so the row
    /// reads the same regardless of the active UI language.
    pub fn picker_label(self) -> &'static str {
        match self {
            DjGemLanguage::Auto => crate::t!("Auto (interface)", "자동 (인터페이스)"),
            DjGemLanguage::English => "English",
            DjGemLanguage::Korean => "한국어",
            DjGemLanguage::Japanese => "日本語",
            DjGemLanguage::ChineseSimplified => "简体中文",
            DjGemLanguage::ChineseTraditional => "繁體中文",
        }
    }

    /// The next choice when stepping the dropdown forward/backward (wraps both ways).
    pub fn cycled(self, forward: bool) -> Self {
        let i = Self::CYCLE.iter().position(|&l| l == self).unwrap_or(0);
        let n = Self::CYCLE.len();
        let j = if forward {
            (i + 1) % n
        } else {
            (i + n - 1) % n
        };
        Self::CYCLE[j]
    }

    /// The system-prompt line that forces the assistant to reply in this language, or `None`
    /// to leave the base prompt's "reply in the user's language" in charge (the resolved
    /// [`Auto`](Self::Auto) case). Concrete languages keep their native script in parentheses so
    /// the model is unambiguous. The Korean line is byte-for-byte what the app sent before this
    /// setting existed, so a Korean UI keeps its exact prior behavior.
    pub fn reply_directive(self) -> Option<String> {
        let named = match self {
            DjGemLanguage::Auto => return None,
            DjGemLanguage::English => "English",
            DjGemLanguage::Korean => "Korean (한국어)",
            DjGemLanguage::Japanese => "Japanese (日本語)",
            DjGemLanguage::ChineseSimplified => "Simplified Chinese (简体中文)",
            DjGemLanguage::ChineseTraditional => "Traditional Chinese (繁體中文)",
        };
        Some(format!(
            "Respond in {named} regardless of the language the user writes in."
        ))
    }

    fn from_u8(v: u8) -> Self {
        match v {
            1 => DjGemLanguage::English,
            2 => DjGemLanguage::Korean,
            3 => DjGemLanguage::Japanese,
            4 => DjGemLanguage::ChineseSimplified,
            5 => DjGemLanguage::ChineseTraditional,
            _ => DjGemLanguage::Auto,
        }
    }
}

/// The process-wide current language. An atomic (not a lock) so [`current`] is cheap to call
/// from every render path; relaxed ordering is fine since it's a lone value nothing else
/// synchronizes against.
static CURRENT: AtomicU8 = AtomicU8::new(Language::English as u8);

/// Set the active UI language. Called once at startup from config and again whenever the user
/// changes the Settings dropdown, so the whole UI re-renders translated on the next frame.
pub fn set_language(lang: Language) {
    CURRENT.store(lang as u8, Ordering::Relaxed);
}

/// The active UI language.
pub fn current() -> Language {
    Language::from_u8(CURRENT.load(Ordering::Relaxed))
}

/// Whether the active language is Korean. A readable shorthand for `format!`-template sites
/// that pick a whole translated string with `if`/`match` rather than the [`t!`](crate::t)
/// macro (which only works when both arms are string literals).
pub fn is_korean() -> bool {
    current() == Language::Korean
}

/// The process-wide *resolved* DJ Gem reply language. Stored resolved (retro already folded to
/// English, and `Auto` resolved against the UI language in
/// [`crate::config::Config::effective_dj_gem_language`]) so the AI actor can read it with no
/// knowledge of retro/UI state. Set at startup and on every settings save. Defaults to `Auto`,
/// matching the config default, so any read before the first apply is still sane.
static DJ_GEM: AtomicU8 = AtomicU8::new(DjGemLanguage::Auto as u8);

/// Set the resolved DJ Gem reply language (see [`DjGemLanguage`]).
pub fn set_dj_gem_language(lang: DjGemLanguage) {
    DJ_GEM.store(lang as u8, Ordering::Relaxed);
}

/// The resolved DJ Gem reply language the assistant should answer in.
pub fn dj_gem_language() -> DjGemLanguage {
    DjGemLanguage::from_u8(DJ_GEM.load(Ordering::Relaxed))
}

/// Pick a `&'static str` by the active language: `t!("English text", "한국어 텍스트")`. Returns
/// the English arm for any non-Korean language. Both arms must be string literals (or
/// `&'static str` consts) so the result stays `&'static str` and the macro drops cleanly into
/// existing `match self => "…"` label functions.
#[macro_export]
macro_rules! t {
    ($en:expr, $ko:expr $(,)?) => {
        match $crate::i18n::current() {
            $crate::i18n::Language::Korean => $ko,
            _ => $en,
        }
    };
}

/// Serializes tests that read or write the process-wide language. The language lives in a
/// single global atomic, so a test that flips it to Korean would otherwise race any parallel
/// test asserting an English label. Every such test takes this lock first and resets the
/// language to English, making them deterministic regardless of scheduling. Poison is ignored
/// (a panicking test only leaves the unit `()` behind).
#[cfg(test)]
pub(crate) fn lock_for_test() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    set_language(Language::English);
    guard
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_english() {
        assert_eq!(Language::default(), Language::English);
    }

    #[test]
    fn serde_uses_snake_case_tags() {
        assert_eq!(
            serde_json::to_string(&Language::Korean).unwrap(),
            "\"korean\""
        );
        let back: Language = serde_json::from_str("\"english\"").unwrap();
        assert_eq!(back, Language::English);
    }

    #[test]
    fn cycle_wraps_both_ways() {
        assert_eq!(Language::English.cycled(true), Language::Korean);
        assert_eq!(Language::Korean.cycled(true), Language::English); // wraps
        assert_eq!(Language::English.cycled(false), Language::Korean); // wraps back
    }

    #[test]
    fn native_names_are_self_describing() {
        assert_eq!(Language::English.native_name(), "English");
        assert_eq!(Language::Korean.native_name(), "한국어");
    }

    #[test]
    fn macro_and_global_track_the_active_language() {
        // The language is a process-wide global; this lock serializes against any parallel
        // test that asserts an English label, and resets to English on acquire.
        let _guard = lock_for_test();

        set_language(Language::Korean);
        assert!(is_korean());
        assert_eq!(current(), Language::Korean);
        assert_eq!(t!("Settings", "설정"), "설정");

        set_language(Language::English);
        assert!(!is_korean());
        assert_eq!(t!("Settings", "설정"), "Settings");
    }
}
