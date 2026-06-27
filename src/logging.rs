//! File logging via `tracing`. We never log to stdout/stderr — that would corrupt
//! the TUI — so everything goes to a non-blocking file appender. The returned
//! [`WorkerGuard`] must be kept alive for the program's lifetime; dropping it flushes
//! and stops the background writer.

use std::path::Path;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;

/// Initialise the global tracing subscriber, writing `ytm-tui.log` into `dir`.
/// Level is controlled by `RUST_LOG` (defaults to `info`). Returns the flush guard.
pub fn init(dir: &Path) -> Option<WorkerGuard> {
    let appender = tracing_appender::rolling::never(dir, "ytm-tui.log");
    let (writer, guard) = tracing_appender::non_blocking(appender);

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let ok = tracing_subscriber::fmt()
        .with_writer(writer)
        .with_env_filter(filter)
        .with_ansi(false)
        .with_target(false)
        .try_init()
        .is_ok();

    ok.then_some(guard)
}
