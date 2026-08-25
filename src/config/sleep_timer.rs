//! Sleep-timer defaults: the popup preset and the fade-out length.

use serde::{Deserialize, Serialize};

/// Sleep-timer defaults read when the popup opens or a remote arm omits a value. Both values
/// clamp through the shared core policy ([`yututui_core::sleep_timer`]) at arm time, so a
/// hand-edited config cannot schedule a session-long timer or a fade that never lands.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct SleepTimerConfig {
    /// Minutes pre-filled in the sleep popup (also the preset used by remote arms that pass
    /// no explicit minutes). Defaults to 30.
    pub default_minutes: u32,
    /// Fade-out length in seconds; `0` pauses immediately at the deadline. Defaults to 30.
    pub fade_secs: u32,
}

impl Default for SleepTimerConfig {
    fn default() -> Self {
        Self {
            default_minutes: yututui_core::sleep_timer::SLEEP_DEFAULT_MINUTES,
            fade_secs: yututui_core::sleep_timer::SLEEP_DEFAULT_FADE_SECS,
        }
    }
}

impl SleepTimerConfig {
    /// The popup preset, clamped to the shared ceiling.
    pub fn effective_default_minutes(&self) -> u32 {
        self.default_minutes
            .clamp(1, yututui_core::sleep_timer::SLEEP_MAX_MINUTES)
    }

    /// The fade length, clamped to a sane band (a fade longer than the timer simply starts
    /// immediately — see the core policy).
    pub fn effective_fade_secs(&self) -> u32 {
        self.fade_secs.min(600)
    }
}
