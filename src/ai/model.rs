//! The Gemini model selector — the single place model API IDs live.
//!
//! The three UI labels are frozen by product decision. The API IDs they map to are the
//! **2.5 generation**, because the literal `gemini-2.0-flash` / `-lite` models were shut
//! down on 2026-06-01 (they 404). The 2.5 line itself has a scheduled shutdown on
//! 2026-10-16, so keep these IDs centralized: a future bump is a one-line edit here.

use serde::{Deserialize, Serialize};

/// The model exposed in the settings AI tab. Persisted as snake_case in `config.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum GeminiModel {
    /// Label "Flash" → `gemini-2.5-flash`.
    Flash,
    /// Label "Flash Lite" → `gemini-2.5-flash-lite` (default: cheapest + fastest).
    #[default]
    FlashLite,
    /// Label "Latest" → `gemini-flash-latest` (always-current free-tier alias).
    Latest,
}

impl GeminiModel {
    /// Order the settings AI tab cycles through with ←/→.
    pub const CYCLE: [GeminiModel; 3] = [GeminiModel::Flash, GeminiModel::FlashLite, GeminiModel::Latest];

    /// The Gemini REST model id used in the request path.
    pub fn api_id(self) -> &'static str {
        match self {
            GeminiModel::Flash => "gemini-2.5-flash",
            GeminiModel::FlashLite => "gemini-2.5-flash-lite",
            GeminiModel::Latest => "gemini-flash-latest",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            GeminiModel::Flash => "Flash",
            GeminiModel::FlashLite => "Flash Lite",
            GeminiModel::Latest => "Latest",
        }
    }

    /// The model to fall back to on persistent 429/5xx. Disabled by the caller once a
    /// side-effecting tool has run (so a retry can't double-apply playback changes).
    pub fn fallback(self) -> Option<Self> {
        match self {
            GeminiModel::Latest => Some(GeminiModel::Flash),
            GeminiModel::Flash => Some(GeminiModel::FlashLite),
            GeminiModel::FlashLite => None,
        }
    }

    /// The next model in the settings cycle (wraps).
    pub fn cycled(self, forward: bool) -> Self {
        let i = Self::CYCLE.iter().position(|&m| m == self).unwrap_or(0);
        let n = Self::CYCLE.len();
        let j = if forward { (i + 1) % n } else { (i + n - 1) % n };
        Self::CYCLE[j]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_ids_are_2_5_generation() {
        assert_eq!(GeminiModel::Flash.api_id(), "gemini-2.5-flash");
        assert_eq!(GeminiModel::FlashLite.api_id(), "gemini-2.5-flash-lite");
        assert_eq!(GeminiModel::Latest.api_id(), "gemini-flash-latest");
    }

    #[test]
    fn default_is_flash_lite() {
        assert_eq!(GeminiModel::default(), GeminiModel::FlashLite);
    }

    #[test]
    fn fallback_chain_terminates() {
        assert_eq!(GeminiModel::Latest.fallback(), Some(GeminiModel::Flash));
        assert_eq!(GeminiModel::Flash.fallback(), Some(GeminiModel::FlashLite));
        assert_eq!(GeminiModel::FlashLite.fallback(), None);
    }

    #[test]
    fn serializes_snake_case() {
        assert_eq!(serde_json::to_string(&GeminiModel::FlashLite).unwrap(), "\"flash_lite\"");
        let back: GeminiModel = serde_json::from_str("\"latest\"").unwrap();
        assert_eq!(back, GeminiModel::Latest);
    }

    #[test]
    fn cycle_wraps_both_ways() {
        assert_eq!(GeminiModel::Flash.cycled(true), GeminiModel::FlashLite);
        assert_eq!(GeminiModel::Latest.cycled(true), GeminiModel::Flash);
        assert_eq!(GeminiModel::Flash.cycled(false), GeminiModel::Latest);
    }
}
