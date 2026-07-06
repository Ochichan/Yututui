//! Playback rules shared by the two playback owners — the interactive TUI [`crate::app`]
//! and the headless [`crate::daemon`] engine. The two keep **separate** playback-state
//! structs (`app::state::Playback` vs `daemon::engine::DaemonPlayback`) yet must apply the
//! same clamps and policies; anything hand-mirrored between them can silently drift when one
//! copy is edited and the other is forgotten. This module is the single home for that shared
//! logic, so a bound or rule changes in exactly one place.
//!
//! Everything here is pure and side-effect-free. Value normalizers guard the mpv/MPRIS
//! trust boundary: a `time-pos`, `duration`, or `volume` arriving from the player (or an OS
//! media widget) is untrusted floating-point — a NaN/±inf silently poisons every downstream
//! `clamp`/compare/sort and can panic ratatui's `Gauge::ratio` — so each owner routes the
//! raw value through these before storing it.

use std::time::Duration;

use crate::queue::Repeat;
use crate::util::finite_or;

/// Percentage points changed per volume step (keypress / OS widget up-down).
pub const VOLUME_STEP: i64 = 5;
/// Highest volume the UI/engine sets. mpv would allow more, but 100 is the v1 ceiling and
/// every volume path clamps to it.
pub const VOLUME_MAX: i64 = 100;

// --- Autoplay / streaming top-up policy -------------------------------------
// Shared by the interactive App reducer and the headless daemon engine, which each keep
// their own playback-state struct but must apply the SAME thresholds. Duplicated literals
// here previously drifted silently when one owner was edited and the other forgotten.

/// Queue length at or below which the autoplay/streaming hook tops up the queue.
pub const AUTOPLAY_THRESHOLD: usize = 3;
/// Minimum gap between autoplay top-up requests (avoids a request storm on a skip burst).
pub const AUTOPLAY_COOLDOWN: Duration = Duration::from_secs(60);
/// Consecutive empty streaming extends before autoplay disables itself (circuit breaker).
pub const AUTOPLAY_MAX_FAILURES: u8 = 3;
/// Size of the raw candidate pool fetched for the local streaming engine to rank. Larger
/// than the final pick count so scoring/diversity/cooldown have real choice.
pub const STREAMING_POOL_COUNT: usize = 40;
/// Number of related tracks to request from the non-DJ-Gem streaming fallback.
pub const STREAMING_FALLBACK_COUNT: usize = 8;
/// How many recent history artists feed the streaming cooldown window.
pub const STREAMING_RECENT_ARTISTS: usize = 12;
/// Consecutive unplayable tracks before auto-skip stops and surfaces the error, instead of
/// skip-storming the whole queue on a systemic failure (offline, bad cookie).
pub const MAX_CONSECUTIVE_PLAY_ERRORS: u8 = 3;

/// Cap on the per-track self-heal guard set (the `video_id`s already retried this session).
/// Both owners reset the set once it reaches this size so a very long session can't grow it
/// for the whole process lifetime; a reset costs at most one extra self-heal per track.
pub const HEAL_ATTEMPTED_MAX: usize = 512;

/// Resolve the mutually-exclusive "autoplay-streaming vs repeat" invariant when **seeding**
/// playback state from config. The two can never both be on; a legacy or hand-edited config
/// may still carry both flags, so the more deliberate `repeat` wins and streaming is
/// dropped. Returns the effective streaming flag. (Interactive set/cycle actions are guarded
/// separately by `Repeat::{set,cycle}_blocked_by_streaming`, which live on `Repeat`.)
pub fn streaming_enabled_with_repeat(autoplay_streaming: bool, repeat: Repeat) -> bool {
    autoplay_streaming && !repeat.is_on()
}

/// Normalize a playback **position** (or live-radio cache timestamp) in seconds coming
/// across the mpv IPC boundary: coalesce NaN/±inf to `0.0` and clamp negatives to `0.0`.
/// Keeps a bad `time-pos`/`cache-time` out of the position-interpolation math and the OS
/// media session, where a NaN would poison the elapsed clock and panic the seekbar gauge.
/// A valid position is returned unchanged.
#[inline]
pub fn norm_position(secs: f64) -> f64 {
    finite_or(secs, 0.0).max(0.0)
}

/// Normalize a track **duration** in seconds from mpv. NaN/±inf/negative collapse to `0.0`,
/// which every seekbar/label path already treats as "length not known yet" (empty bar,
/// `--:--`), so a hostile/garbage duration reads as unknown instead of poisoning ratios.
#[inline]
pub fn norm_duration(secs: f64) -> f64 {
    norm_position(secs)
}

/// Map an OS-media-widget volume (MPRIS `Volume`, nominally `0.0..=1.0`) to an integer
/// percent in `0..=VOLUME_MAX`. Returns `None` for a **non-finite** write so the caller
/// ignores it rather than silently muting (a raw `NaN.clamp(0,1)*100` rounds to `0`);
/// finite out-of-range values clamp into the valid band.
#[inline]
pub fn volume_percent_from_unit(unit: f64) -> Option<i64> {
    if !unit.is_finite() {
        return None;
    }
    Some((unit.clamp(0.0, 1.0) * 100.0).round() as i64)
}

/// Normalize an mpv `volume` property **event** (already a percent) to `0..=VOLUME_MAX`.
/// Returns `None` for a non-finite report so the caller leaves the current volume untouched
/// instead of muting (raw `NaN.round() as i64` is `0`) or storing a garbage level
/// (`inf.round() as i64` saturates to `i64::MAX`).
#[inline]
pub fn norm_volume_event(percent: f64) -> Option<i64> {
    if !percent.is_finite() {
        return None;
    }
    Some((percent.round() as i64).clamp(0, VOLUME_MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions_and_durations_reject_non_finite_and_negative() {
        assert_eq!(norm_position(42.5), 42.5);
        assert_eq!(norm_position(0.0), 0.0);
        assert_eq!(norm_position(-3.0), 0.0);
        assert_eq!(norm_position(f64::NAN), 0.0);
        assert_eq!(norm_position(f64::INFINITY), 0.0);
        assert_eq!(norm_position(f64::NEG_INFINITY), 0.0);
        // Duration shares the rule; a garbage length collapses to "unknown" (0).
        assert_eq!(norm_duration(180.0), 180.0);
        assert_eq!(norm_duration(f64::NAN), 0.0);
    }

    #[test]
    fn unit_volume_maps_clamps_and_ignores_nan() {
        assert_eq!(volume_percent_from_unit(0.0), Some(0));
        assert_eq!(volume_percent_from_unit(0.5), Some(50));
        assert_eq!(volume_percent_from_unit(1.0), Some(100));
        // Out-of-range finite values clamp into the band.
        assert_eq!(volume_percent_from_unit(1.5), Some(100));
        assert_eq!(volume_percent_from_unit(-0.2), Some(0));
        // Non-finite is ignored (no silent mute).
        assert_eq!(volume_percent_from_unit(f64::NAN), None);
        assert_eq!(volume_percent_from_unit(f64::INFINITY), None);
    }

    #[test]
    fn config_seed_tiebreak_drops_streaming_when_repeat_is_on() {
        use crate::queue::Repeat;
        // Streaming survives only when repeat is fully off.
        assert!(streaming_enabled_with_repeat(true, Repeat::Off));
        // Repeat wins the tie in either repeat mode.
        assert!(!streaming_enabled_with_repeat(true, Repeat::All));
        assert!(!streaming_enabled_with_repeat(true, Repeat::One));
        // No streaming preference → stays off regardless of repeat.
        assert!(!streaming_enabled_with_repeat(false, Repeat::Off));
        assert!(!streaming_enabled_with_repeat(false, Repeat::All));
    }

    #[test]
    fn volume_event_clamps_and_ignores_non_finite() {
        assert_eq!(norm_volume_event(73.0), Some(73));
        assert_eq!(norm_volume_event(73.4), Some(73));
        assert_eq!(norm_volume_event(73.6), Some(74));
        assert_eq!(norm_volume_event(150.0), Some(VOLUME_MAX));
        assert_eq!(norm_volume_event(-5.0), Some(0));
        assert_eq!(norm_volume_event(f64::NAN), None);
        assert_eq!(norm_volume_event(f64::INFINITY), None);
    }
}
