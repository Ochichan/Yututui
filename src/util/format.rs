//! Display formatting helpers.

/// Format a number of seconds as `M:SS` (or `H:MM:SS` past an hour).
pub fn time(secs: f64) -> String {
    let total = secs.max(0.0) as u64;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Filled fraction (`0.0..=1.0`) of the seekbar gauge for `pos`/`dur` seconds. An absent or
/// non-positive duration (track length not known yet) yields an empty bar; a position past
/// the end clamps to full. Takes the raw `Option`s straight off playback state so the
/// `None`-coalescing lives here (and is unit-tested) rather than at the call site.
pub fn seekbar_ratio(pos: Option<f64>, dur: Option<f64>) -> f64 {
    match dur {
        Some(d) if d > 0.0 => (pos.unwrap_or(0.0) / d).clamp(0.0, 1.0),
        _ => 0.0,
    }
}

/// The seekbar's `M:SS / M:SS` label. The right side shows `--:--` until the duration is
/// known (mpv reports position before length on a fresh load), matching the gauge's empty
/// bar from [`seekbar_ratio`].
pub fn seekbar_label(pos: Option<f64>, dur: Option<f64>) -> String {
    let right = match dur {
        Some(d) if d > 0.0 => time(d),
        _ => "--:--".to_owned(),
    };
    format!("{} / {right}", time(pos.unwrap_or(0.0)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_minutes_seconds() {
        assert_eq!(time(0.0), "0:00");
        assert_eq!(time(5.0), "0:05");
        assert_eq!(time(75.0), "1:15");
        assert_eq!(time(-3.0), "0:00");
    }

    #[test]
    fn formats_hours() {
        assert_eq!(time(3661.0), "1:01:01");
    }

    #[test]
    fn seekbar_ratio_handles_missing_and_extreme_inputs() {
        // Nothing playing yet: no position, no duration → empty bar (no div-by-zero).
        assert_eq!(seekbar_ratio(None, None), 0.0);
        // Position known but duration not (fresh load) → still empty, no div-by-zero.
        assert_eq!(seekbar_ratio(Some(30.0), None), 0.0);
        // A reported zero duration is treated the same as absent.
        assert_eq!(seekbar_ratio(Some(30.0), Some(0.0)), 0.0);
        // Normal midpoint.
        assert_eq!(seekbar_ratio(Some(30.0), Some(120.0)), 0.25);
        // Position past the end clamps to a full bar rather than overflowing.
        assert_eq!(seekbar_ratio(Some(200.0), Some(120.0)), 1.0);
        // A spurious negative position clamps to empty.
        assert_eq!(seekbar_ratio(Some(-5.0), Some(120.0)), 0.0);
    }

    #[test]
    fn seekbar_label_shows_dashes_until_duration_is_known() {
        assert_eq!(seekbar_label(None, None), "0:00 / --:--");
        assert_eq!(seekbar_label(Some(75.0), None), "1:15 / --:--");
        assert_eq!(seekbar_label(Some(75.0), Some(0.0)), "1:15 / --:--");
        assert_eq!(seekbar_label(Some(75.0), Some(200.0)), "1:15 / 3:20");
    }
}
