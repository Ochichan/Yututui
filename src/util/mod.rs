//! Small, dependency-free helpers shared across the app.

/// Return `x` only when it is finite (not NaN / ±inf), else `default`. The single guard for
/// floats crossing a trust boundary (mpv IPC, MPRIS control, persisted play-logs, LRC
/// timestamps) before they reach math that assumes a real number: a NaN silently poisons
/// `clamp`, comparisons, and `total_cmp` sorts, and can panic ratatui's `Gauge::ratio`.
#[inline]
pub fn finite_or(x: f64, default: f64) -> f64 {
    if x.is_finite() { x } else { default }
}

/// [`finite_or`] for `f32` (streaming feature vectors).
#[inline]
pub fn finite_or_f32(x: f32, default: f32) -> f32 {
    if x.is_finite() { x } else { default }
}

pub mod art;
pub mod backpressure;
pub mod browser;
pub mod format;
pub mod http;
pub mod io;
pub mod process;
pub mod runtime;
pub mod safe_fs;
pub mod sanitize;
