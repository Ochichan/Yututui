//! 10-band graphic equalizer: presets, per-band gains, and the mpv lavfi filter chain.
//!
//! mpv applies audio filters via the `af` property (a comma-separated lavfi chain). We
//! model the EQ as ten `equalizer` instances, one per ISO octave band, each *labeled*
//! (`@eqN:`) so the settings screen can nudge a single band live with `af-command`
//! instead of rebuilding the whole graph (which clicks audibly). Optional `dynaudnorm`
//! normalization rides at the head of the chain. See [`build_af_string`].

use serde::{Deserialize, Serialize};

/// Number of EQ bands.
pub const BANDS: usize = 10;

/// Center frequencies of the ten bands, in Hz (ISO octave spacing).
pub const BAND_FREQS: [u32; BANDS] = [31, 62, 125, 250, 500, 1000, 2000, 4000, 8000, 16000];

/// The mpv filter label for band `i` (e.g. `eq3`). Used both in the `@label:` prefix of
/// the filter chain and as the `af-command` target for a live single-band edit.
pub fn band_label(i: usize) -> String {
    format!("eq{i}")
}

/// Built-in equalizer presets. Each non-`Custom` variant maps to a fixed set of ten band
/// gains (dB); `Custom` means "use the gains the user dialed in by hand".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum EqPreset {
    #[default]
    Flat,
    BassBoost,
    TrebleBoost,
    Vocal,
    Rock,
    Jazz,
    /// Hand-tuned bands (the array lives in `App`/`Config`, not the preset).
    Custom,
}

impl EqPreset {
    /// Every preset, in the order `e` cycles through them (Custom is reachable only by
    /// editing a band, so it's intentionally excluded from the cycle).
    pub const CYCLE: [EqPreset; 6] = [
        EqPreset::Flat,
        EqPreset::BassBoost,
        EqPreset::TrebleBoost,
        EqPreset::Vocal,
        EqPreset::Rock,
        EqPreset::Jazz,
    ];

    /// The next preset in the `e`-key cycle (Custom folds back to Flat).
    pub fn cycled(self) -> Self {
        let cur = Self::CYCLE.iter().position(|&p| p == self).unwrap_or(0);
        Self::CYCLE[(cur + 1) % Self::CYCLE.len()]
    }

    pub fn label(self) -> &'static str {
        match self {
            EqPreset::Flat => "Flat",
            EqPreset::BassBoost => "Bass",
            EqPreset::TrebleBoost => "Treble",
            EqPreset::Vocal => "Vocal",
            EqPreset::Rock => "Rock",
            EqPreset::Jazz => "Jazz",
            EqPreset::Custom => "Custom",
        }
    }

    /// The ten band gains (dB) for this preset. `Custom` has no intrinsic gains, so it
    /// reports flat — callers that support custom EQ read the stored band array instead.
    pub fn gains(self) -> [f64; BANDS] {
        // Bands:        31   62  125  250  500   1k   2k   4k   8k  16k
        match self {
            EqPreset::Flat | EqPreset::Custom => [0.0; BANDS],
            EqPreset::BassBoost => [6.0, 5.0, 4.0, 2.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            EqPreset::TrebleBoost => [0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 2.0, 4.0, 5.0, 6.0],
            EqPreset::Vocal => [-2.0, -1.0, 0.0, 1.0, 3.0, 3.0, 2.0, 1.0, 0.0, -1.0],
            EqPreset::Rock => [4.0, 3.0, 1.0, 0.0, -1.0, 0.0, 1.0, 2.0, 3.0, 3.0],
            EqPreset::Jazz => [3.0, 2.0, 1.0, 2.0, -1.0, -1.0, 0.0, 1.0, 2.0, 3.0],
        }
    }
}

/// Build the mpv `af` chain for the given band gains and normalization, or `None` when
/// nothing is active (all bands flat and normalization off) — in which case the caller
/// should clear `af` entirely.
///
/// When *any* band is non-zero, *all ten* labeled bands are emitted (even the flat ones)
/// so every `@eqN` label exists and the settings screen can later `af-command` it.
pub fn build_af_string(bands: &[f64; BANDS], normalize: bool) -> Option<String> {
    let mut filters: Vec<String> = Vec::new();
    if normalize {
        filters.push("dynaudnorm".to_owned());
    }
    if bands.iter().any(|g| g.abs() > f64::EPSILON) {
        for (i, (&freq, &gain)) in BAND_FREQS.iter().zip(bands.iter()).enumerate() {
            filters.push(format!(
                "@{}:equalizer=f={freq}:width_type=o:width=2:g={gain}",
                band_label(i)
            ));
        }
    }
    if filters.is_empty() {
        None
    } else {
        Some(filters.join(","))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flat_with_no_normalize_is_none() {
        assert!(build_af_string(&[0.0; BANDS], false).is_none());
    }

    #[test]
    fn normalize_only_emits_dynaudnorm() {
        assert_eq!(build_af_string(&[0.0; BANDS], true).as_deref(), Some("dynaudnorm"));
    }

    #[test]
    fn any_nonzero_band_emits_all_ten_labeled_bands() {
        let mut bands = [0.0; BANDS];
        bands[0] = 5.0;
        let af = build_af_string(&bands, false).unwrap();
        // All ten labels present so af-command can target any band later.
        for i in 0..BANDS {
            assert!(af.contains(&format!("@eq{i}:")), "missing label eq{i} in {af}");
        }
        assert!(af.contains("f=31:width_type=o:width=2:g=5"));
        assert!(af.contains("f=62:width_type=o:width=2:g=0"));
    }

    #[test]
    fn normalize_precedes_bands() {
        let mut bands = [0.0; BANDS];
        bands[9] = 3.0;
        let af = build_af_string(&bands, true).unwrap();
        assert!(af.starts_with("dynaudnorm,@eq0:"), "got {af}");
    }

    #[test]
    fn preset_cycle_wraps_and_skips_custom() {
        assert_eq!(EqPreset::Flat.cycled(), EqPreset::BassBoost);
        assert_eq!(EqPreset::Jazz.cycled(), EqPreset::Flat);
        // Custom isn't in the cycle; it folds back to Flat's successor.
        assert_eq!(EqPreset::Custom.cycled(), EqPreset::BassBoost);
    }

    #[test]
    fn flat_and_custom_gains_are_zero() {
        assert_eq!(EqPreset::Flat.gains(), [0.0; BANDS]);
        assert_eq!(EqPreset::Custom.gains(), [0.0; BANDS]);
        assert!(EqPreset::BassBoost.gains().iter().any(|&g| g > 0.0));
    }

    #[test]
    fn preset_round_trips_as_snake_case() {
        let json = serde_json::to_string(&EqPreset::BassBoost).unwrap();
        assert_eq!(json, "\"bass_boost\"");
        let back: EqPreset = serde_json::from_str("\"treble_boost\"").unwrap();
        assert_eq!(back, EqPreset::TrebleBoost);
    }
}
