//! One source of truth for interactive terminal liveness and bounded-output policy.
//!
//! Vendored terminal crates cannot depend back on the application crate. Their matching parser
//! limits are recorded in each fork's `PATCHES.md` and protected by fork tests; application code
//! and `ytt doctor terminal --json` must use these values directly.

use std::time::Duration;

pub(crate) const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(500);
pub(crate) const OWNER_PROBE_INTERVAL: Duration = Duration::from_millis(2_500);
pub(crate) const OWNER_PROBE_TIMEOUT: Duration = Duration::from_millis(500);
pub(crate) const LIVENESS_OUTPUT_GATE_TIMEOUT: Duration = Duration::from_millis(1_000);
pub(crate) const CPR_TOTAL_TIMEOUT: Duration = Duration::from_millis(2_000);
pub(crate) const CPR_WRITE_TIMEOUT: Duration = Duration::from_millis(500);
pub(crate) const AMBIGUOUS_CONFIRMATIONS: u8 = 2;
pub(crate) const AMBIGUOUS_RETRY_INTERVAL: Duration = Duration::from_millis(250);
pub(crate) const HARD_WATCHDOG_TIMEOUT: Duration = Duration::from_millis(8_000);
pub(crate) const STARTUP_LIVENESS_REPORT_TIMEOUT: Duration = Duration::from_millis(8_250);

pub(crate) const OWNER_OUTPUT_TIMEOUT: Duration = Duration::from_millis(7_000);
pub const STARTUP_OUTPUT_TIMEOUT: Duration = Duration::from_millis(3_000);
pub(crate) const NORMAL_RESTORE_TIMEOUT: Duration = Duration::from_millis(1_000);
pub(crate) const EMERGENCY_RESTORE_TIMEOUT: Duration = Duration::from_millis(150);
pub(crate) const HARD_EXIT_QUERY_QUIESCE_TIMEOUT: Duration = Duration::from_millis(1_500);
// Kept as a literal because `Duration::add` is not const-stable on the minimum Rust toolchain.
// The policy test below protects the derived relationship.
pub(crate) const HARD_EXIT_TOTAL_TIMEOUT: Duration = Duration::from_millis(1_650);
pub(crate) const NOTIFICATION_OUTPUT_TIMEOUT: Duration = Duration::from_millis(1_000);

// Mirrors the protected values in the vendored crossterm Unix parser.
pub(crate) const GENERIC_PENDING_INPUT_IDLE: Duration = Duration::from_millis(1_000);
pub(crate) const ESCAPE_PENDING_INPUT_IDLE: Duration = Duration::from_millis(100);
pub(crate) const PASTE_PENDING_INPUT_IDLE: Duration = Duration::from_millis(3_000);
pub(crate) const PASTE_MAX_BYTES: usize = 16 * 1024 * 1024;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_report_deadline_covers_one_hard_window_and_retry() {
        assert_eq!(
            STARTUP_LIVENESS_REPORT_TIMEOUT,
            HARD_WATCHDOG_TIMEOUT + AMBIGUOUS_RETRY_INTERVAL
        );
    }

    #[test]
    fn repeat_signal_can_quiesce_a_probe_and_restore_before_forced_exit_bound() {
        assert_eq!(
            HARD_EXIT_TOTAL_TIMEOUT,
            HARD_EXIT_QUERY_QUIESCE_TIMEOUT + EMERGENCY_RESTORE_TIMEOUT
        );
        assert!(HARD_EXIT_TOTAL_TIMEOUT < Duration::from_secs(2));
    }

    #[test]
    fn vendored_unix_parser_limits_match_the_reported_policy() {
        let input = include_str!("../crates/crossterm/src/event/source/unix/input.rs");
        for marker in [
            "const GENERIC_PENDING_IDLE: Duration = Duration::from_secs(1);",
            "const ESC_PENDING_IDLE: Duration = Duration::from_millis(100);",
            "const PASTE_PENDING_IDLE: Duration = Duration::from_secs(3);",
            "const MAX_PASTE_BYTES: usize = 16 * 1024 * 1024;",
        ] {
            assert!(
                input.contains(marker),
                "vendored parser policy drift: {marker}"
            );
        }

        assert_eq!(GENERIC_PENDING_INPUT_IDLE, Duration::from_secs(1));
        assert_eq!(ESCAPE_PENDING_INPUT_IDLE, Duration::from_millis(100));
        assert_eq!(PASTE_PENDING_INPUT_IDLE, Duration::from_secs(3));
        assert_eq!(PASTE_MAX_BYTES, 16 * 1024 * 1024);
    }
}
