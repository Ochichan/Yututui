//! Track-scoped A–B loop policy shared by the interactive and headless playback owners.
//!
//! Both owners drive mpv's `ab-loop-a` / `ab-loop-b` properties and project the active loop
//! onto remote snapshots, so the transition rules, span bounds, and refusal reasons live here.

use serde::{Deserialize, Serialize};

/// Minimum distance between A and B points so a loop cannot degenerate into a tight seek storm.
pub const AB_LOOP_MIN_SPAN_SECS: f64 = 0.5;
/// Tolerance above B before activating an A–B loop also seeks back into `[a, b]`.
const PAST_B_SEEK_EPSILON_SECS: f64 = 0.1;
/// Minimum backward jump between consecutive `time-pos` ticks to treat as an automatic loop wrap.
const LOOP_WRAP_BACKWARD_EPSILON_SECS: f64 = 0.25;

/// User or remote intent targeting the current track's A–B loop.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum AbLoopAction {
    /// Advance `Off → PointA → Active → Off` using the current playhead position.
    Cycle,
    /// Set point A to `seconds` (or the current playhead when `None`).
    SetA {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        seconds: Option<f64>,
    },
    /// Set point B to `seconds` (or the current playhead when `None`).
    SetB {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        seconds: Option<f64>,
    },
    /// Turn off the A–B loop (idempotent when already off).
    Clear,
}

/// Active A–B loop state for the currently loaded track.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum AbLoopState {
    #[default]
    Off,
    PointA {
        a_secs: f64,
    },
    Active {
        a_secs: f64,
        b_secs: f64,
    },
}

impl AbLoopState {
    pub fn is_set(self) -> bool {
        !matches!(self, Self::Off)
    }

    pub fn is_active(self) -> bool {
        matches!(self, Self::Active { .. })
    }

    pub fn a_secs(self) -> Option<f64> {
        match self {
            Self::Off => None,
            Self::PointA { a_secs } | Self::Active { a_secs, .. } => Some(a_secs),
        }
    }

    pub fn b_secs(self) -> Option<f64> {
        match self {
            Self::Active { b_secs, .. } => Some(b_secs),
            Self::Off | Self::PointA { .. } => None,
        }
    }

    /// Wire-ready millisecond bounds (`None` when off). Rounded to whole milliseconds so
    /// snapshots stay integer-typed and `Eq`.
    pub fn to_ms(self) -> Option<(u64, Option<u64>)> {
        match self {
            Self::Off => None,
            Self::PointA { a_secs } => Some((secs_to_ms(a_secs), None)),
            Self::Active { a_secs, b_secs } => Some((secs_to_ms(a_secs), Some(secs_to_ms(b_secs)))),
        }
    }

    /// Raw `(ab-loop-a, ab-loop-b)` seconds for mpv property writes (`None` means `"no"`).
    pub fn mpv_bounds(self) -> (Option<f64>, Option<f64>) {
        match self {
            Self::Off => (None, None),
            Self::PointA { a_secs } => (Some(a_secs), None),
            Self::Active { a_secs, b_secs } => (Some(a_secs), Some(b_secs)),
        }
    }

    /// Detect an automatic mpv A–B loop wrap between consecutive `time-pos` ticks.
    ///
    /// mpv jumps back from B to A without emitting a seek command through either owner, so both
    /// owners use this predicate to bump `position_epoch` through their central helper.
    pub fn is_loop_wrap(self, prev_pos: Option<f64>, next_pos: f64) -> bool {
        let Self::Active { a_secs, b_secs } = self else {
            return false;
        };
        let Some(prev) = prev_pos else {
            return false;
        };
        prev.is_finite()
            && next_pos.is_finite()
            && prev >= a_secs
            && next_pos <= b_secs + PAST_B_SEEK_EPSILON_SECS
            && prev - next_pos >= LOOP_WRAP_BACKWARD_EPSILON_SECS
    }

    /// Compute the next loop state and whether the owner should also seek back to A.
    pub fn transition(
        self,
        action: AbLoopAction,
        current_pos: f64,
        duration: Option<f64>,
    ) -> Result<AbLoopTransition, AbLoopRefusal> {
        match action {
            AbLoopAction::Clear => Ok(AbLoopTransition {
                state: Self::Off,
                seek_to: None,
            }),
            AbLoopAction::Cycle => match self {
                Self::Off => {
                    let a_secs = validate_point(current_pos, duration)?;
                    Ok(AbLoopTransition {
                        state: Self::PointA { a_secs },
                        seek_to: None,
                    })
                }
                Self::PointA { a_secs } => {
                    let b_candidate = validate_point(current_pos, duration)?;
                    activate(a_secs, b_candidate, current_pos)
                }
                Self::Active { .. } => Ok(AbLoopTransition {
                    state: Self::Off,
                    seek_to: None,
                }),
            },
            AbLoopAction::SetA { seconds } => {
                let a_secs = validate_point(seconds.unwrap_or(current_pos), duration)?;
                match self {
                    Self::Active { b_secs, .. } if b_secs - a_secs >= AB_LOOP_MIN_SPAN_SECS => {
                        let seek_to =
                            (current_pos > b_secs + PAST_B_SEEK_EPSILON_SECS).then_some(a_secs);
                        Ok(AbLoopTransition {
                            state: Self::Active { a_secs, b_secs },
                            seek_to,
                        })
                    }
                    _ => Ok(AbLoopTransition {
                        state: Self::PointA { a_secs },
                        seek_to: None,
                    }),
                }
            }
            AbLoopAction::SetB { seconds } => {
                let Some(a_secs) = self.a_secs() else {
                    return Err(AbLoopRefusal::MissingPointA);
                };
                let b_candidate = validate_point(seconds.unwrap_or(current_pos), duration)?;
                activate(a_secs, b_candidate, current_pos)
            }
        }
    }
}

/// Pure result of an accepted A–B loop action.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AbLoopTransition {
    pub state: AbLoopState,
    /// When activating an explicit loop whose end B is already behind the playhead, owners
    /// seek to `a_secs` so playback enters the loop immediately.
    pub seek_to: Option<f64>,
}

/// Why an owner refused an A–B loop action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AbLoopRefusal {
    NothingPlaying,
    NotSeekable,
    InvalidPosition,
    MissingPointA,
    SpanTooShort,
}

impl AbLoopRefusal {
    /// Stable `reason` string returned on remote errors.
    pub fn reason(self) -> &'static str {
        match self {
            Self::NothingPlaying => "nothing_playing",
            Self::NotSeekable => "not_seekable",
            Self::InvalidPosition => "invalid_position",
            Self::MissingPointA => "ab_loop_missing_a",
            Self::SpanTooShort => "ab_loop_too_short",
        }
    }
}

fn validate_point(seconds: f64, duration: Option<f64>) -> Result<f64, AbLoopRefusal> {
    if !seconds.is_finite() || seconds < 0.0 {
        return Err(AbLoopRefusal::InvalidPosition);
    }
    let clamped = match duration {
        Some(duration) if duration.is_finite() && duration > 0.0 => seconds.min(duration),
        _ => seconds,
    };
    Ok(clamped)
}

fn activate(a: f64, b: f64, current_pos: f64) -> Result<AbLoopTransition, AbLoopRefusal> {
    let (a_secs, b_secs) = if a <= b { (a, b) } else { (b, a) };
    if b_secs - a_secs < AB_LOOP_MIN_SPAN_SECS {
        return Err(AbLoopRefusal::SpanTooShort);
    }
    let seek_to = (current_pos.is_finite() && current_pos > b_secs + PAST_B_SEEK_EPSILON_SECS)
        .then_some(a_secs);
    Ok(AbLoopTransition {
        state: AbLoopState::Active { a_secs, b_secs },
        seek_to,
    })
}

fn secs_to_ms(secs: f64) -> u64 {
    if !secs.is_finite() || secs <= 0.0 {
        0
    } else {
        (secs * 1000.0).round() as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cycle_progresses_off_to_a_to_active_to_off() {
        let s0 = AbLoopState::Off;
        let t1 = s0
            .transition(AbLoopAction::Cycle, 30.0, Some(180.0))
            .unwrap();
        assert_eq!(t1.state, AbLoopState::PointA { a_secs: 30.0 });
        assert_eq!(t1.seek_to, None);

        let t2 = t1
            .state
            .transition(AbLoopAction::Cycle, 65.5, Some(180.0))
            .unwrap();
        assert_eq!(
            t2.state,
            AbLoopState::Active {
                a_secs: 30.0,
                b_secs: 65.5
            }
        );
        assert_eq!(t2.seek_to, None);

        let t3 = t2
            .state
            .transition(AbLoopAction::Cycle, 42.0, Some(180.0))
            .unwrap();
        assert_eq!(t3.state, AbLoopState::Off);
        assert_eq!(t3.seek_to, None);
    }

    #[test]
    fn reversed_b_before_a_normalizes_and_short_span_refuses() {
        let a = AbLoopState::PointA { a_secs: 60.0 };
        let short = a.transition(AbLoopAction::Cycle, 60.2, Some(180.0));
        assert_eq!(short, Err(AbLoopRefusal::SpanTooShort));

        let swapped = a
            .transition(AbLoopAction::Cycle, 40.0, Some(180.0))
            .unwrap();
        assert_eq!(
            swapped.state,
            AbLoopState::Active {
                a_secs: 40.0,
                b_secs: 60.0
            }
        );
        assert_eq!(swapped.seek_to, None);
    }

    #[test]
    fn explicit_b_behind_playhead_requests_seek_to_a() {
        let a = AbLoopState::PointA { a_secs: 10.0 };
        let t = a
            .transition(
                AbLoopAction::SetB {
                    seconds: Some(25.0),
                },
                50.0,
                Some(180.0),
            )
            .unwrap();
        assert_eq!(
            t.state,
            AbLoopState::Active {
                a_secs: 10.0,
                b_secs: 25.0
            }
        );
        assert_eq!(t.seek_to, Some(10.0));
    }

    #[test]
    fn set_b_without_a_refuses_and_set_a_past_b_resets_to_point_a() {
        assert_eq!(
            AbLoopState::Off.transition(
                AbLoopAction::SetB {
                    seconds: Some(20.0)
                },
                10.0,
                None
            ),
            Err(AbLoopRefusal::MissingPointA)
        );
        let active = AbLoopState::Active {
            a_secs: 10.0,
            b_secs: 30.0,
        };
        let moved_past = active
            .transition(
                AbLoopAction::SetA {
                    seconds: Some(40.0),
                },
                40.0,
                Some(180.0),
            )
            .unwrap();
        assert_eq!(moved_past.state, AbLoopState::PointA { a_secs: 40.0 });
    }

    #[test]
    fn clear_is_idempotent_and_invalid_points_refuse() {
        assert_eq!(
            AbLoopState::Off
                .transition(AbLoopAction::Clear, 0.0, None)
                .unwrap()
                .state,
            AbLoopState::Off
        );
        assert_eq!(
            AbLoopState::Off.transition(
                AbLoopAction::SetA {
                    seconds: Some(-1.0)
                },
                0.0,
                None
            ),
            Err(AbLoopRefusal::InvalidPosition)
        );
        assert_eq!(
            AbLoopState::Off.transition(
                AbLoopAction::SetA {
                    seconds: Some(f64::NAN)
                },
                0.0,
                None
            ),
            Err(AbLoopRefusal::InvalidPosition)
        );
    }

    #[test]
    fn loop_wrap_detects_backward_jump_only_while_active() {
        let active = AbLoopState::Active {
            a_secs: 30.0,
            b_secs: 45.0,
        };
        assert!(active.is_loop_wrap(Some(44.8), 30.1));
        assert!(!active.is_loop_wrap(Some(35.0), 35.5));
        assert!(!active.is_loop_wrap(Some(35.0), 34.9));
        assert!(!active.is_loop_wrap(None, 30.1));
        assert!(!AbLoopState::Off.is_loop_wrap(Some(44.8), 30.1));
        assert!(!AbLoopState::PointA { a_secs: 30.0 }.is_loop_wrap(Some(44.8), 30.1));
    }
}
