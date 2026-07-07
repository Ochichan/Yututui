//! File logging via `tracing`. We never log to stdout/stderr — that would corrupt
//! the TUI — so everything goes to a non-blocking file appender. The returned
//! [`WorkerGuard`] must be kept alive for the program's lifetime; dropping it flushes
//! and stops the background writer.

use std::path::Path;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;

pub fn init(dir: &Path) -> Option<WorkerGuard> {
    init_named(dir, "yututui.log")
}

/// Initialise the global tracing subscriber, writing `file_name` into `dir`.
/// Level is controlled by `RUST_LOG` (defaults to `info`). Returns the flush guard.
///
/// Rotates daily and keeps the 7 most recent files (`<file_name>.<date>`), so a
/// long-running install's log can't grow without bound (the previous single `never` file
/// did). A build error (unwritable dir) yields `None`, same as a failed subscriber init.
pub fn init_named(dir: &Path, file_name: &str) -> Option<WorkerGuard> {
    let appender = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix(file_name)
        .max_log_files(7)
        .build(dir)
        .ok()?;
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
