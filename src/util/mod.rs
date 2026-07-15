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
pub mod background_task;
pub mod backpressure;
pub mod blocking;
pub mod browser;
pub(crate) mod command_barrier;
pub mod delivery;
pub mod event_policy;
pub mod format;
pub mod github;
pub mod http;
pub mod io;
pub mod process;
pub(crate) mod process_guard;
pub mod process_tree;
pub mod query;
pub mod runtime;
pub mod safe_fs;
pub mod sanitize;
pub mod text_edit;
