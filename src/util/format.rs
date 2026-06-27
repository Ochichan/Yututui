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
}
