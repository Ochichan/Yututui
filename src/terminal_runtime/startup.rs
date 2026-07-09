use std::time::{Duration, Instant};

pub struct StartupTrace {
    enabled: bool,
    start: Instant,
    events: Vec<(&'static str, Duration)>,
    flushed: usize,
    logging_ready: bool,
}

impl StartupTrace {
    pub fn from_env() -> Self {
        Self {
            enabled: std::env::var_os("YTM_STARTUP_TRACE").is_some(),
            start: Instant::now(),
            events: Vec::new(),
            flushed: 0,
            logging_ready: false,
        }
    }

    pub fn mark(&mut self, label: &'static str) {
        if !self.enabled {
            return;
        }
        self.events.push((label, self.start.elapsed()));
        self.flush();
    }

    pub(crate) fn enable_logging(&mut self) {
        self.logging_ready = true;
        self.flush();
    }

    fn flush(&mut self) {
        if !self.enabled || !self.logging_ready {
            return;
        }
        for (label, elapsed) in &self.events[self.flushed..] {
            tracing::info!(
                target: "ytt::startup",
                label,
                elapsed_ms = elapsed.as_secs_f64() * 1000.0,
                "startup"
            );
        }
        self.flushed = self.events.len();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_trace_buffers_until_logging_is_ready() {
        let mut disabled = StartupTrace {
            enabled: false,
            start: Instant::now(),
            events: Vec::new(),
            flushed: 0,
            logging_ready: false,
        };
        disabled.mark("ignored");
        assert!(disabled.events.is_empty());
        assert_eq!(disabled.flushed, 0);

        let mut enabled = StartupTrace {
            enabled: true,
            start: Instant::now(),
            events: Vec::new(),
            flushed: 0,
            logging_ready: false,
        };
        enabled.mark("config_loaded");
        enabled.mark("runtime_built");
        assert_eq!(enabled.events.len(), 2);
        assert_eq!(enabled.flushed, 0, "not flushed before logging is ready");

        enabled.enable_logging();
        assert_eq!(enabled.flushed, 2);
        enabled.mark("first_draw");
        assert_eq!(enabled.flushed, 3);
    }
}
