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
        // Coalesce a non-finite position to 0: `NaN.clamp(..)` stays NaN, which panics
        // ratatui's `Gauge::ratio`. (A non-finite/≤0 duration already yields an empty bar.)
        Some(d) if d > 0.0 => (super::finite_or(pos.unwrap_or(0.0), 0.0) / d).clamp(0.0, 1.0),
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

/// The nominal timeshift depth one full seekbar width represents on a live radio stream.
/// The real depth is whatever fits mpv's back-buffer; this only scales the *rendered*
/// backoff so a few seconds behind reads as a sliver and minutes behind as a clear gap.
const RADIO_RENDER_WINDOW_SECS: f64 = 600.0;

/// `(ratio, label)` of the seekbar for a live radio stream, where a duration-based gauge
/// is meaningless. At (or presumed at) the live edge the bar is full and labeled `LIVE`;
/// a timeshifted playhead backs the bar off the right edge proportionally to how far
/// behind it sits and labels the gap (`-Ns`). With no position at all this falls back to
/// the ordinary unknown-duration label so a connecting stream looks like today.
pub fn radio_seekbar(pos: Option<f64>, behind: Option<f64>, synced: Option<bool>) -> (f64, String) {
    let elapsed = time(pos.unwrap_or(0.0));
    match (pos, behind, synced) {
        (_, Some(b), Some(false)) => {
            // Coalesce a non-finite `behind` (mirrors the app's other ratio paths) so a NaN
            // can't reach the Gauge ratio and panic ratatui — here it was only safe by accident.
            let b = crate::util::finite_or(b, 0.0);
            let ratio = (1.0 - b / RADIO_RENDER_WINDOW_SECS).clamp(0.05, 1.0);
            (ratio, format!("{elapsed} · -{}s", b as i64))
        }
        (_, Some(_), _) | (Some(_), None, _) => (1.0, format!("{elapsed} · LIVE")),
        (None, None, _) => (0.0, seekbar_label(pos, None)),
    }
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
        // A non-finite position coalesces to empty rather than a NaN ratio (Gauge panics on NaN).
        assert_eq!(seekbar_ratio(Some(f64::NAN), Some(120.0)), 0.0);
        assert_eq!(seekbar_ratio(Some(f64::INFINITY), Some(120.0)), 0.0);
    }

    #[test]
    fn seekbar_label_shows_dashes_until_duration_is_known() {
        assert_eq!(seekbar_label(None, None), "0:00 / --:--");
        assert_eq!(seekbar_label(Some(75.0), None), "1:15 / --:--");
        assert_eq!(seekbar_label(Some(75.0), Some(0.0)), "1:15 / --:--");
        assert_eq!(seekbar_label(Some(75.0), Some(200.0)), "1:15 / 3:20");
    }

    #[test]
    fn radio_seekbar_full_bar_and_live_label_at_the_edge() {
        assert_eq!(
            radio_seekbar(Some(75.0), Some(3.0), Some(true)),
            (1.0, "1:15 · LIVE".to_owned())
        );
        // Unknown sync state but a running position: presume live (the glyph carries
        // the uncertainty; cache-less streams are effectively always at the edge).
        assert_eq!(
            radio_seekbar(Some(75.0), None, None),
            (1.0, "1:15 · LIVE".to_owned())
        );
    }

    #[test]
    fn radio_seekbar_backs_off_proportionally_when_behind() {
        let (ratio, label) = radio_seekbar(Some(75.0), Some(60.0), Some(false));
        assert_eq!(label, "1:15 · -60s");
        assert!((ratio - 0.9).abs() < 1e-9);
        // A huge timeshift clamps to a visible sliver rather than an empty/negative bar.
        let (ratio, label) = radio_seekbar(Some(75.0), Some(100_000.0), Some(false));
        assert_eq!(label, "1:15 · -100000s");
        assert_eq!(ratio, 0.05);
    }

    #[test]
    fn radio_seekbar_connecting_stream_matches_todays_empty_state() {
        assert_eq!(
            radio_seekbar(None, None, None),
            (0.0, "0:00 / --:--".to_owned())
        );
    }
}
