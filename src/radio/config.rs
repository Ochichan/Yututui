//! Tunable parameters for the local radio engine, persisted under [`crate::config::Config`].
//!
//! Only a single tuned `Balanced` profile ships today (the user-facing mode toggle is
//! deferred), but the per-mode parameters live here so enabling it later is config-only.
//! Every field is `#[serde(default)]` so old `config.json` files keep loading.

use serde::{Deserialize, Serialize};

/// How adventurous the station is. Drives MMR λ, sampling temperature, and artist spacing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum RadioMode {
    /// Stay close to the seed (tight, relevance-dominant).
    Focused,
    /// The shipped default: a balance of familiarity and exploration.
    #[default]
    Balanced,
    /// Lean into discovery (more diverse, more exploratory sampling).
    Discovery,
}

impl RadioMode {
    /// All modes in toggle order (the settings cycle steps through these).
    pub const CYCLE: [RadioMode; 3] =
        [RadioMode::Focused, RadioMode::Balanced, RadioMode::Discovery];

    /// A short human label for the settings field and the player status line.
    pub fn label(self) -> &'static str {
        match self {
            RadioMode::Focused => crate::t!("Focused", "집중"),
            RadioMode::Balanced => crate::t!("Balanced", "균형"),
            RadioMode::Discovery => crate::t!("Discovery", "발견"),
        }
    }

    /// The next mode when stepping the toggle forward/backward (wraps both ways).
    pub fn cycled(self, forward: bool) -> Self {
        let i = Self::CYCLE.iter().position(|&m| m == self).unwrap_or(1);
        let n = Self::CYCLE.len();
        let j = if forward { (i + 1) % n } else { (i + n - 1) % n };
        Self::CYCLE[j]
    }

    /// MMR relevance/diversity trade-off: higher = more relevance, less diversity. Tuned
    /// down from typical playlist values because a *radio* wants more variety than a
    /// hand-built playlist (research: 0.55–0.65 reads best for stations).
    pub fn mmr_lambda(self) -> f32 {
        match self {
            RadioMode::Focused => 0.70,
            RadioMode::Balanced => 0.60,
            RadioMode::Discovery => 0.50,
        }
    }

    /// Softmax temperature for the final pick. Higher = more exploration. These operate on
    /// [0,1]-normalized scores, so values this size give real (not near-greedy) sampling.
    pub fn temperature(self) -> f32 {
        match self {
            RadioMode::Focused => 0.35,
            RadioMode::Balanced => 0.50,
            RadioMode::Discovery => 0.65,
        }
    }

    /// Minimum number of other tracks between two by the same artist (cooldown).
    pub fn artist_gap(self) -> usize {
        match self {
            RadioMode::Focused => 7,
            RadioMode::Balanced => 8,
            RadioMode::Discovery => 10,
        }
    }
}

/// Additive weights for the base score's [0,1]-normalized feature terms (Balanced default).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ScoreWeights {
    pub cooccurrence: f32,
    pub seed_affinity: f32,
    pub novelty: f32,
    pub ytm_continuation: f32,
    pub completion: f32,
    /// Magnitude of the positive bonus added for a [`super::musicgate::music_tier_score`] signal
    /// (Topic/VEVO channel, "official audio/video" title). Applied to the raw [0,1] tier, not a
    /// normalized column, so an "official audio" track gets a fixed nudge regardless of the batch.
    pub music_tier: f32,
    /// Magnitude of the penalty subtracted for a disliked artist / version mismatch.
    pub dislike_penalty: f32,
}

impl Default for ScoreWeights {
    fn default() -> Self {
        Self {
            cooccurrence: 0.40,
            seed_affinity: 0.25,
            novelty: 0.15,
            ytm_continuation: 0.15,
            completion: 0.05,
            music_tier: 0.15,
            dislike_penalty: 0.40,
        }
    }
}

/// Weights for the MMR similarity kernel (behaviorally-derived similarity beats surface
/// metadata; title-token Jaccard is intentionally dropped as noisy).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SimWeights {
    pub cooc: f32,
    pub artist: f32,
    pub album: f32,
}

impl Default for SimWeights {
    fn default() -> Self {
        Self { cooc: 0.60, artist: 0.30, album: 0.10 }
    }
}

/// Co-occurrence (SPPMI) graph parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CoocConfig {
    /// Max distance (in tracks, within a session) counted as a co-occurrence.
    pub window: usize,
    /// SPPMI shift `k`: subtract `ln(k)` from PMI (k=1 → plain PPMI).
    pub sppmi_k: f32,
    /// Weight of the reverse (later→earlier) edge relative to the forward edge.
    pub reverse: f32,
    /// Inactivity gap (minutes) that splits the raw play log into sessions.
    pub session_gap_min: i64,
    /// Max tracks per session window (caps spurious long-session pairs).
    pub session_max: usize,
}

impl Default for CoocConfig {
    fn default() -> Self {
        Self { window: 8, sppmi_k: 1.0, reverse: 0.6, session_gap_min: 20, session_max: 10 }
    }
}

/// AI reranker knobs. When a Gemini key is configured and this is enabled, the engine hands
/// the model a diverse local shortlist and asks it to pick ids only — it can never invent a
/// track, and any failure degrades to the pure-local pick with no user-visible breakage.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AiRerankConfig {
    /// Run the AI reranker when a key is present. Off → always the pure local engine.
    pub enabled: bool,
    /// Diverse candidates handed to the model (a tight list reranks sharper than a long one).
    pub shortlist: usize,
    /// Tracks enqueued per refill.
    pub picks: usize,
}

impl Default for AiRerankConfig {
    fn default() -> Self {
        Self { enabled: true, shortlist: 12, picks: 8 }
    }
}

/// The MusicGate: a rule-based content filter that keeps non-music videos (reactions,
/// podcasts, tutorials, …) and gimmick re-uploads out of the radio candidate pool. The
/// non-music reject is always on (when `enabled`); the gimmick reject (karaoke / nightcore /
/// 8D / sped-up / slowed+reverb) is mode-tied — forced in [`RadioMode::Focused`], opt-in via
/// `block_altered_versions` otherwise — and self-disables when the pool would starve.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MusicGateConfig {
    /// Master switch. When false, no candidates are hard-rejected by the gate (the soft
    /// `version_penalty` in `canonical.rs` still applies independently).
    pub enabled: bool,
    /// Also apply the non-music hard-reject to `WatchPlaylist` candidates. Default true (the
    /// strong-reject list is conservative enough that it essentially never trips on YTM's own
    /// curated radio); set false to fully exempt that pre-curated source.
    pub gate_watch_playlist: bool,
    /// Hard-reject gimmick versions (karaoke / nightcore / 8D / sped-up / slowed+reverb) in
    /// Balanced/Discovery too. Focused always blocks them regardless of this flag.
    pub block_altered_versions: bool,
}

impl Default for MusicGateConfig {
    fn default() -> Self {
        Self { enabled: true, gate_watch_playlist: true, block_altered_versions: false }
    }
}

/// The full set of local-radio tuning knobs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RadioConfig {
    /// Active mode (only `Balanced` is surfaced today).
    pub mode: RadioMode,
    pub weights: ScoreWeights,
    pub sim_weights: SimWeights,
    /// Same-album cooldown (independent of [`RadioMode::artist_gap`]).
    pub album_gap: usize,
    /// Softmax candidate pool: sample the final picks from the top-K scored candidates.
    pub sample_top_k: usize,
    /// Recency half-life (days) for the novelty decay.
    pub recency_half_life_days: f32,
    /// Drop candidates shorter than this (seconds) — interludes/skits.
    pub min_duration_secs: u32,
    /// Drop candidates longer than this (seconds) — mixes/podcasts.
    pub max_duration_secs: u32,
    pub cooc: CoocConfig,
    pub ai: AiRerankConfig,
    pub gate: MusicGateConfig,
}

impl Default for RadioConfig {
    fn default() -> Self {
        Self {
            mode: RadioMode::Balanced,
            weights: ScoreWeights::default(),
            sim_weights: SimWeights::default(),
            album_gap: 5,
            sample_top_k: 40,
            recency_half_life_days: 46.0,
            min_duration_secs: 30,
            max_duration_secs: 15 * 60,
            cooc: CoocConfig::default(),
            ai: AiRerankConfig::default(),
            gate: MusicGateConfig::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_mode_is_balanced() {
        assert_eq!(RadioMode::default(), RadioMode::Balanced);
        assert_eq!(RadioConfig::default().mode, RadioMode::Balanced);
    }

    #[test]
    fn mode_cycles_through_all_three_both_ways() {
        let _guard = crate::i18n::lock_for_test();
        assert_eq!(RadioMode::Balanced.cycled(true), RadioMode::Discovery);
        assert_eq!(RadioMode::Discovery.cycled(true), RadioMode::Focused); // wraps
        assert_eq!(RadioMode::Focused.cycled(false), RadioMode::Discovery); // wraps back
        // Every mode has a distinct, non-empty label (English default, asserted under lock).
        let labels: Vec<&str> = RadioMode::CYCLE.iter().map(|m| m.label()).collect();
        assert_eq!(labels, vec!["Focused", "Balanced", "Discovery"]);
    }

    #[test]
    fn mode_params_are_ordered_by_adventurousness() {
        // More adventurous → lower λ (more diversity), higher temperature, wider artist gap.
        assert!(RadioMode::Focused.mmr_lambda() > RadioMode::Discovery.mmr_lambda());
        assert!(RadioMode::Focused.temperature() < RadioMode::Discovery.temperature());
        assert!(RadioMode::Focused.artist_gap() < RadioMode::Discovery.artist_gap());
    }

    #[test]
    fn config_round_trips_and_defaults_fill_missing() {
        let cfg = RadioConfig::default();
        let json = serde_json::to_string(&cfg).unwrap();
        let back: RadioConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.sample_top_k, cfg.sample_top_k);
        // A bare object fills every field from defaults.
        let bare: RadioConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(bare.weights.cooccurrence, 0.40);
        assert_eq!(bare.cooc.window, 8);
    }

    #[test]
    fn musicgate_defaults_and_fill_missing() {
        let cfg = RadioConfig::default();
        assert!(cfg.gate.enabled);
        assert!(cfg.gate.gate_watch_playlist);
        assert!(!cfg.gate.block_altered_versions);
        // Old configs (no `gate` key, or a partial one) fill from defaults.
        let bare: RadioConfig = serde_json::from_str("{}").unwrap();
        assert!(bare.gate.enabled);
        let partial: RadioConfig =
            serde_json::from_str(r#"{"gate":{"block_altered_versions":true}}"#).unwrap();
        assert!(partial.gate.enabled, "missing sub-field fills from default");
        assert!(partial.gate.block_altered_versions);
    }
}
