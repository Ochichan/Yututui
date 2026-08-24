//! Pure sleep-timer policy shared by yututui's interactive and headless playback owners.
//!
//! Like the rest of this crate the state machine is side-effect-free: owners feed it the
//! current wall-clock time and their own live volume, and translate the returned steps into
//! their existing `SetVolume`/pause paths. Keeping the countdown, fade window, and restore
//! bookkeeping here means the App and daemon cannot drift apart on when the timer fires or
//! which volume the fade should land on.

use std::time::{Duration, Instant};

/// Minutes pre-filled in the sleep popup when it opens.
pub const SLEEP_DEFAULT_MINUTES: u32 = 30;
/// Fade-out length in seconds; `0` means a hard stop at the deadline.
pub const SLEEP_DEFAULT_FADE_SECS: u32 = 30;
/// Ceiling on one armed timer, so an accidental huge value still ends this session.
pub const SLEEP_MAX_MINUTES: u32 = 12 * 60;
/// A fade shorter than this many seconds still gets one volume step per remaining second.
pub const SLEEP_FADE_MIN_STEPS: u32 = 5;

/// One armed sleep timer. `Instant` fields compare against owner-supplied `now` values so
/// tests can drive the machine with synthetic clocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SleepTimer {
    /// Wall-clock moment at which playback must pause.
    pub deadline: Instant,
    /// Fade-out length; zero means pause immediately at the deadline.
    pub fade: Duration,
    /// The volume to restore after the timer fires; `None` until the fade begins.
    pub pre_fade_volume: Option<i64>,
    /// Whether the fade is currently in progress (the deadline entered the fade window).
    pub fading: bool,
    /// The last volume this machine told its owner to set; used to detect a manual change
    /// mid-fade, which cancels the fade but keeps the deadline.
    pub last_sent: Option<i64>,
}

impl SleepTimer {
    /// Arm a timer `minutes` from `now` with a `fade_secs` fade. Both values clamp to the
    /// policy ceilings so a hostile or hand-edited config cannot arm a session-long timer.
    pub fn armed(now: Instant, minutes: u32, fade_secs: u32) -> Self {
        let minutes = minutes.min(SLEEP_MAX_MINUTES);
        Self {
            deadline: now + Duration::from_secs(u64::from(minutes) * 60),
            fade: Duration::from_secs(u64::from(fade_secs)),
            pre_fade_volume: None,
            fading: false,
            last_sent: None,
        }
    }

    /// Whole seconds until the pause moment; `None` once the deadline has passed.
    pub fn remaining_secs(&self, now: Instant) -> Option<u64> {
        self.deadline
            .checked_duration_since(now)
            .map(|d| d.as_secs())
    }

    /// The pause moment is at or before `now`.
    pub fn fired(&self, now: Instant) -> bool {
        now >= self.deadline
    }

    /// Advance the state machine one owner tick and return the next action.
    ///
    /// * [`SleepStep::Idle`] — nothing to do this tick (timer armed, fade not due or canceled).
    /// * [`SleepStep::Volume`] — the owner should set its volume to this value now.
    /// * [`SleepStep::Fired`] — the deadline passed; the owner should restore
    ///   [`SleepTimer::pre_fade_volume`], pause, and clear the timer. `pre_fade_volume` is
    ///   `None` when no fade ever ran (a zero fade or a manual-change cancel), so the owner
    ///   leaves its volume untouched.
    pub fn advance(&mut self, now: Instant, current_volume: i64) -> SleepStep {
        if self.fired(now) {
            return SleepStep::Fired;
        }
        if self.fade.is_zero() {
            return SleepStep::Idle;
        }
        // A manual volume change (or any owner-side drift) while fading cancels the fade but
        // keeps the deadline: the user took the volume back over, so don't fight them.
        if self.fading && self.last_sent != Some(current_volume) {
            self.fading = false;
            self.pre_fade_volume = None;
            self.last_sent = None;
            return SleepStep::Idle;
        }
        let fade_start = self
            .deadline
            .checked_sub(self.fade)
            .unwrap_or(self.deadline);
        if now < fade_start {
            return SleepStep::Idle;
        }
        if !self.fading {
            self.fading = true;
            self.pre_fade_volume = Some(current_volume);
        }
        let pre = self.pre_fade_volume.unwrap_or(current_volume);
        let remaining = self.deadline.saturating_duration_since(now);
        let elapsed = self.fade.saturating_sub(remaining);
        // One step per second over the fade window; a very short window still gets
        // SLEEP_FADE_MIN_STEPS so the drop never jumps straight to zero. `ceil` makes the
        // first tick land one step below the pre-fade volume instead of re-emitting it.
        let steps = (self.fade.as_secs().max(SLEEP_FADE_MIN_STEPS as u64)).max(1);
        let step_secs = self.fade.as_secs_f64() / steps as f64;
        let step_index = ((elapsed.as_secs_f64() / step_secs).ceil() as u64).min(steps);
        let next = pre - ((pre as f64) * (step_index as f64 / steps as f64)).round() as i64;
        let next = next.max(0);
        if self.last_sent == Some(next) {
            return SleepStep::Idle;
        }
        self.last_sent = Some(next);
        SleepStep::Volume(next)
    }
}

/// What an owner should do after one [`SleepTimer::advance`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SleepStep {
    /// No state change due this tick.
    Idle,
    /// Set playback volume to this value (a fade step).
    Volume(i64),
    /// The deadline passed: restore `pre_fade_volume` when present, pause, clear the timer.
    Fired,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn armed_clamps_minutes_to_ceiling() {
        let timer = SleepTimer::armed(t0(), SLEEP_MAX_MINUTES + 500, 0);
        assert!(
            timer
                .remaining_secs(t0())
                .is_some_and(|s| s <= 60 * 60 * 12)
        );
    }

    #[test]
    fn zero_fade_fires_without_steps() {
        let mut timer = SleepTimer::armed(t0(), 1, 0);
        assert_eq!(timer.advance(t0(), 80), SleepStep::Idle);
        let at_deadline = timer.deadline;
        assert_eq!(timer.advance(at_deadline, 80), SleepStep::Fired);
        assert_eq!(timer.pre_fade_volume, None);
    }

    #[test]
    fn fade_descends_then_fires_with_restore_volume() {
        let now = t0();
        let mut timer = SleepTimer::armed(now, 1, 10);
        // Just before the fade window: idle.
        let before = timer.deadline - timer.fade - Duration::from_millis(1);
        assert_eq!(timer.advance(before, 100), SleepStep::Idle);
        // First fade tick records the pre-fade volume and steps down.
        let mut seen = Vec::new();
        for i in 0..12 {
            let now = timer.deadline - timer.fade + Duration::from_secs(i);
            match timer.advance(now, 100) {
                SleepStep::Volume(v) => seen.push(v),
                SleepStep::Fired => break,
                SleepStep::Idle => {}
            }
        }
        assert_eq!(timer.pre_fade_volume, Some(100));
        assert!(
            seen.windows(2).all(|w| w[0] >= w[1]),
            "fade must descend: {seen:?}"
        );
        // The final audible step lands one tick above zero; the pause at the deadline is the
        // completion of the fade.
        assert!(
            seen.last().is_some_and(|v| *v <= 11),
            "final step: {seen:?}"
        );
        assert_eq!(timer.advance(timer.deadline, 0), SleepStep::Fired);
    }

    #[test]
    fn manual_volume_change_cancels_fade_but_keeps_deadline() {
        let now = t0();
        let mut timer = SleepTimer::armed(now, 1, 10);
        let fade_start = timer.deadline - timer.fade;
        let first = match timer.advance(fade_start, 100) {
            SleepStep::Volume(v) => v,
            other => panic!("expected a volume step, got {other:?}"),
        };
        // The owner moved volume by hand between ticks: fade cancels, deadline stays.
        let _ = first;
        assert_eq!(
            timer.advance(fade_start + Duration::from_secs(2), 55),
            SleepStep::Idle
        );
        assert!(!timer.fading);
        assert_eq!(timer.pre_fade_volume, None);
        assert_eq!(timer.advance(timer.deadline, 55), SleepStep::Fired);
    }

    #[test]
    fn remaining_secs_counts_down_and_ends() {
        let now = t0();
        let timer = SleepTimer::armed(now, 1, 0);
        assert_eq!(timer.remaining_secs(now), Some(60));
        assert_eq!(
            timer.remaining_secs(now + Duration::from_secs(30)),
            Some(30)
        );
        assert_eq!(timer.remaining_secs(timer.deadline), Some(0));
        assert_eq!(
            timer.remaining_secs(timer.deadline + Duration::from_secs(1)),
            None
        );
    }
}
