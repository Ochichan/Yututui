//! File logging via `tracing`. We never log to stdout/stderr — that would corrupt
//! the TUI — so everything goes to a non-blocking file appender. The returned
//! [`WorkerGuard`] must be kept alive for the program's lifetime; dropping it flushes
//! and stops the background writer.

use std::path::Path;
use std::sync::OnceLock;

use tracing_appender::non_blocking::{ErrorCounter, NonBlocking, NonBlockingBuilder, WorkerGuard};
use tracing_subscriber::EnvFilter;

/// The upstream default is 128,000 lines, which can retain a large burst indefinitely. Logging is
/// diagnostic and must never apply backpressure to the reducer, so keep a much smaller lossy queue.
const BUFFERED_LINES_LIMIT: usize = 8_192;

static DROPPED_LINES: OnceLock<ErrorCounter> = OnceLock::new();

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
    let (writer, guard) = non_blocking_writer(appender);
    let error_counter = writer.error_counter();

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let ok = tracing_subscriber::fmt()
        .with_writer(writer)
        .with_env_filter(filter)
        .with_ansi(false)
        .with_target(false)
        .try_init()
        .is_ok();

    if ok {
        let _ = DROPPED_LINES.set(error_counter);
        Some(guard)
    } else {
        None
    }
}

/// Total log lines dropped after the bounded lossy queue filled. This is intentionally a polling
/// diagnostic (used by `YTM_PERF`) rather than another log event, which could itself be dropped.
pub fn dropped_lines() -> usize {
    DROPPED_LINES.get().map_or(0, ErrorCounter::dropped_lines)
}

fn non_blocking_writer<T>(writer: T) -> (NonBlocking, WorkerGuard)
where
    T: std::io::Write + Send + 'static,
{
    NonBlockingBuilder::default()
        .buffered_lines_limit(BUFFERED_LINES_LIMIT)
        .lossy(true)
        .finish(writer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::{Arc, Condvar, Mutex, mpsc};
    use std::time::Duration;

    struct StalledWriter {
        entered: mpsc::SyncSender<()>,
        released: Arc<(Mutex<bool>, Condvar)>,
    }

    impl Write for StalledWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let _ = self.entered.try_send(());
            let (lock, ready) = &*self.released;
            let mut released = lock.lock().unwrap();
            while !*released {
                released = ready.wait(released).unwrap();
            }
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn stalled_writer_drops_after_bounded_queue_without_blocking_producer() {
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let released = Arc::new((Mutex::new(false), Condvar::new()));
        let (mut writer, guard) = non_blocking_writer(StalledWriter {
            entered: entered_tx,
            released: Arc::clone(&released),
        });
        let dropped = writer.error_counter();

        // Let the worker consume one line and stall inside the sink. The producer can then fill
        // the exact bounded channel and cross the limit without ever waiting on that sink.
        writer.write_all(b"worker-stalls-here\n").unwrap();
        entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("logging worker entered the stalled sink");
        for _ in 0..=BUFFERED_LINES_LIMIT {
            writer.write_all(b"queued\n").unwrap();
        }
        assert_eq!(dropped.dropped_lines(), 1);

        let (lock, ready) = &*released;
        *lock.lock().unwrap() = true;
        ready.notify_all();
        drop(guard);
    }
}
