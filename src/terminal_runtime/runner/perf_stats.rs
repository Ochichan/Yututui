use std::time::{Duration, Instant};

use crate::{app::App, logging};

pub(super) struct PerfStats {
    pub(super) enabled: bool,
    pub(super) last_log: Instant,
    pub(super) frames: u64,
    pub(super) ime_fast_scrubs: u64,
    pub(super) draw_total: Duration,
    pub(super) draw_max: Duration,
    pub(super) art_resizes: u64,
}

impl PerfStats {
    pub(super) fn from_env() -> Self {
        let enabled = std::env::var_os("YTM_PERF").is_some();
        Self {
            enabled,
            last_log: Instant::now(),
            frames: 0,
            ime_fast_scrubs: 0,
            draw_total: Duration::ZERO,
            draw_max: Duration::ZERO,
            art_resizes: 0,
        }
    }

    pub(super) fn record_draw(&mut self, elapsed: Duration) {
        if !self.enabled {
            return;
        }
        self.frames += 1;
        self.draw_total += elapsed;
        self.draw_max = self.draw_max.max(elapsed);
    }

    pub(super) fn record_art_resize(&mut self) {
        if self.enabled {
            self.art_resizes += 1;
        }
    }

    pub(super) fn record_ime_fast_scrub(&mut self) {
        if self.enabled {
            self.ime_fast_scrubs += 1;
        }
    }

    pub(super) fn maybe_log(&mut self, app: &App) {
        if !self.enabled || self.last_log.elapsed() < Duration::from_secs(5) {
            return;
        }
        let avg_draw_ms = if self.frames == 0 {
            0.0
        } else {
            self.draw_total.as_secs_f64() * 1000.0 / self.frames as f64
        };
        let a = app.animations();
        let active_effects = [
            a.title,
            a.heart,
            a.seekbar,
            a.spinner,
            a.eq_bars,
            a.controls,
            a.border,
            a.rain,
            a.donut,
            a.visualizer,
            a.starfield,
            a.bounce,
        ]
        .into_iter()
        .filter(|on| *on)
        .count();
        tracing::info!(
            target: "ytt::perf",
            full_frames = self.frames,
            ime_fast_scrubs = self.ime_fast_scrubs,
            avg_draw_ms,
            max_draw_ms = self.draw_max.as_secs_f64() * 1000.0,
            art_resizes = self.art_resizes,
            dropped_log_lines = logging::dropped_lines(),
            active_effects,
            tick_fps = app.animation_tick_fps(),
            draw_fps = app.animation_draw_fps(),
            dirty = app.dirty,
            "perf window"
        );
        self.last_log = Instant::now();
        self.frames = 0;
        self.ime_fast_scrubs = 0;
        self.draw_total = Duration::ZERO;
        self.draw_max = Duration::ZERO;
        self.art_resizes = 0;
    }
}
