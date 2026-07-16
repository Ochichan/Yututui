//! Playback rules shared by yututui's interactive and headless playback owners.
//!
//! The owners keep separate playback-state structs but must apply the same clamps and policies;
//! anything hand-mirrored between them can silently drift. Everything here is pure and
//! side-effect-free. Value normalizers guard the player and OS-media trust boundaries: a NaN or
//! infinity silently poisons downstream clamps and comparisons, so each owner routes raw values
//! through these functions before storing them.

use std::time::Duration;

use crate::Repeat;

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

// --- Playback-mode transitions ----------------------------------------------

/// The owner-neutral portion of playback mode state.
///
/// The App and daemon deliberately keep separate playback owners. Passing only these two
/// values through a pure transition keeps their shared repeat/streaming rule in one place while
/// leaving persistence, localized notices, responses, and refill effects with the owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaybackModeState {
    pub repeat: Repeat,
    /// The saved music-mode preference, not mode-projected effective streaming. Local/Radio
    /// guards remain owner policy outside this core.
    pub autoplay_streaming: bool,
}

impl PlaybackModeState {
    pub const fn new(repeat: Repeat, autoplay_streaming: bool) -> Self {
        Self {
            repeat,
            autoplay_streaming,
        }
    }

    /// Apply one playback-mode request without performing owner side effects.
    ///
    /// Rejection is action-specific so this extraction preserves the shipped recovery semantics:
    /// cycling is blocked only for the enabling `Off` → `All` step; an already-invalid legacy
    /// `All + streaming` state may still cycle to `One`, then to `Off`. Explicit set requests
    /// reject an on-target while the other mode is on, and either mode can still be disabled.
    pub fn transition(
        self,
        action: PlaybackModeAction,
    ) -> Result<PlaybackModeTransition, PlaybackModeTransitionError> {
        let state = match action {
            PlaybackModeAction::CycleRepeat => {
                if self.repeat == Repeat::Off && self.autoplay_streaming {
                    return Err(PlaybackModeTransitionError::IncompatiblePlaybackModes);
                }
                Self::new(self.repeat.cycled(), self.autoplay_streaming)
            }
            PlaybackModeAction::SetRepeat(repeat) => {
                if repeat.is_on() && self.autoplay_streaming {
                    return Err(PlaybackModeTransitionError::IncompatiblePlaybackModes);
                }
                Self::new(repeat, self.autoplay_streaming)
            }
            PlaybackModeAction::SetStreaming(streaming) => {
                if streaming && self.repeat.is_on() {
                    return Err(PlaybackModeTransitionError::IncompatiblePlaybackModes);
                }
                Self::new(self.repeat, streaming)
            }
        };

        Ok(PlaybackModeTransition {
            changed: state != self,
            state,
        })
    }
}

// Config seeding/session restoration and the empty-stream circuit breaker are recovery paths,
// not user/API playback-mode actions. They intentionally stay outside this transition model:
// seeding uses `streaming_enabled_with_repeat`, while a breaker may force autoplay off without
// producing action-owned responses, toasts, persistence, or refill effects here.

/// A user/API request that can change the mutually-exclusive playback modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackModeAction {
    CycleRepeat,
    SetRepeat(Repeat),
    SetStreaming(bool),
}

/// The accepted result of a pure playback-mode transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaybackModeTransition {
    pub state: PlaybackModeState,
    pub changed: bool,
}

/// Why a playback-mode transition was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackModeTransitionError {
    IncompatiblePlaybackModes,
}

/// Resolve the mutually-exclusive "autoplay-streaming vs repeat" invariant when **seeding**
/// playback state from config. The two can never both be on; a legacy or hand-edited config
/// may still carry both flags, so the more deliberate `repeat` wins and streaming is
/// dropped. Returns the effective streaming flag. Interactive set/cycle actions go through
/// [`PlaybackModeState::transition`]; the similarly named `Repeat` queries are compatibility
/// wrappers around that canonical transition.
pub fn streaming_enabled_with_repeat(autoplay_streaming: bool, repeat: Repeat) -> bool {
    autoplay_streaming && !repeat.is_on()
}

/// Return `value` only when it is finite (not NaN / ±inf), else `default`.
#[inline]
fn finite_or(value: f64, default: f64) -> f64 {
    if value.is_finite() { value } else { default }
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

/// Upper bound on an absolute seek target, in seconds. A day dwarfs any real track or
/// podcast, so it never bites a legitimate seek; it exists only to keep an absurd remote/GUI
/// `seek-to` (e.g. `1e18`) out of mpv when the current track's duration is unknown (live
/// stream, not-yet-probed file) and so can't provide a tighter clamp.
pub const MAX_SEEK_SECONDS: f64 = 24.0 * 3600.0;

/// Clamp an absolute seek target (seconds). Coalesces NaN/±inf/negatives to `0.0`, caps at
/// [`MAX_SEEK_SECONDS`], and — when the duration is known (`Some(d)`, `d > 0`) — additionally
/// clamps within the track. Both playback owners route `seek-to` through this so the bound
/// can't drift between them.
#[inline]
pub fn clamp_seek_target(pos: f64, duration: Option<f64>) -> f64 {
    let mut t = norm_position(pos).min(MAX_SEEK_SECONDS);
    if let Some(d) = duration {
        let d = norm_duration(d);
        if d > 0.0 {
            t = t.min(d);
        }
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Expected {
        Accepted(PlaybackModeState, bool),
        Incompatible,
    }

    #[test]
    fn playback_mode_transition_truth_table_is_exhaustive() {
        use PlaybackModeAction::{CycleRepeat, SetRepeat, SetStreaming};
        use Repeat::{All, Off, One};

        const fn accepted(repeat: Repeat, streaming: bool, changed: bool) -> Expected {
            Expected::Accepted(PlaybackModeState::new(repeat, streaming), changed)
        }

        // Each row covers every action for one of the 3 repeat × 2 streaming input states.
        // Keeping this table explicit makes legacy-invalid inputs part of the contract too.
        let rows = [
            (
                PlaybackModeState::new(Off, false),
                [
                    accepted(All, false, true),
                    accepted(Off, false, false),
                    accepted(All, false, true),
                    accepted(One, false, true),
                    accepted(Off, false, false),
                    accepted(Off, true, true),
                ],
            ),
            (
                PlaybackModeState::new(All, false),
                [
                    accepted(One, false, true),
                    accepted(Off, false, true),
                    accepted(All, false, false),
                    accepted(One, false, true),
                    accepted(All, false, false),
                    Expected::Incompatible,
                ],
            ),
            (
                PlaybackModeState::new(One, false),
                [
                    accepted(Off, false, true),
                    accepted(Off, false, true),
                    accepted(All, false, true),
                    accepted(One, false, false),
                    accepted(One, false, false),
                    Expected::Incompatible,
                ],
            ),
            (
                PlaybackModeState::new(Off, true),
                [
                    Expected::Incompatible,
                    accepted(Off, true, false),
                    Expected::Incompatible,
                    Expected::Incompatible,
                    accepted(Off, false, true),
                    accepted(Off, true, false),
                ],
            ),
            (
                PlaybackModeState::new(All, true),
                [
                    accepted(One, true, true),
                    accepted(Off, true, true),
                    Expected::Incompatible,
                    Expected::Incompatible,
                    accepted(All, false, true),
                    Expected::Incompatible,
                ],
            ),
            (
                PlaybackModeState::new(One, true),
                [
                    accepted(Off, true, true),
                    accepted(Off, true, true),
                    Expected::Incompatible,
                    Expected::Incompatible,
                    accepted(One, false, true),
                    Expected::Incompatible,
                ],
            ),
        ];
        let actions = [
            CycleRepeat,
            SetRepeat(Off),
            SetRepeat(All),
            SetRepeat(One),
            SetStreaming(false),
            SetStreaming(true),
        ];

        for (state, expected) in rows {
            for (action, expected) in actions.into_iter().zip(expected) {
                let actual = match state.transition(action) {
                    Ok(result) => Expected::Accepted(result.state, result.changed),
                    Err(PlaybackModeTransitionError::IncompatiblePlaybackModes) => {
                        Expected::Incompatible
                    }
                };
                assert_eq!(actual, expected, "state={state:?}, action={action:?}");
            }
        }
    }

    #[test]
    fn clamp_seek_target_bounds_unknown_duration_and_respects_known() {
        // Unknown duration: a finite-but-absurd value caps at the day ceiling; a non-finite
        // (inf/NaN) or negative value coalesces to 0.0 — either way mpv never sees garbage.
        assert_eq!(clamp_seek_target(1e18, None), MAX_SEEK_SECONDS);
        assert_eq!(clamp_seek_target(f64::INFINITY, None), 0.0);
        assert_eq!(clamp_seek_target(-5.0, None), 0.0);
        assert_eq!(clamp_seek_target(f64::NAN, None), 0.0);
        assert_eq!(clamp_seek_target(90.0, None), 90.0);
        // Known duration clamps tighter; a zero/unknown duration does not pin the target to 0.
        assert_eq!(clamp_seek_target(90.0, Some(180.0)), 90.0);
        assert_eq!(clamp_seek_target(999.0, Some(180.0)), 180.0);
        assert_eq!(clamp_seek_target(500.0, Some(0.0)), 500.0);
    }

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
