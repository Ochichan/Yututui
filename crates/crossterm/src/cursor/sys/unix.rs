use std::{
    io::{self, Write},
    time::Duration,
};

use crate::{
    cursor::CursorPositionProbe,
    event::{probe_cursor_position_with, CursorPositionQuery},
    terminal::{disable_raw_mode, enable_raw_mode, sys::is_raw_mode_enabled},
};

const DEFAULT_POSITION_TIMEOUT: Duration = Duration::from_secs(2);

/// Returns the cursor position (column, row).
///
/// The top left cell is represented as `(0, 0)`.
pub fn position() -> io::Result<(u16, u16)> {
    let mut stdout = io::stdout();
    legacy_position_result(probe_position_with(&mut stdout, DEFAULT_POSITION_TIMEOUT)?)
}

fn legacy_position_result(probe: CursorPositionProbe) -> io::Result<(u16, u16)> {
    match probe {
        CursorPositionProbe::Position(column, row) => Ok((column, row)),
        CursorPositionProbe::DeferredForPendingInput => Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "cursor-position query deferred while terminal input is incomplete",
        )),
        CursorPositionProbe::DeferredForRecentInput => Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "cursor-position query deferred after recent terminal input",
        )),
    }
}

/// Queries the cursor position using the caller's writer and one absolute timeout.
///
/// The query is not written while the Unix event parser has recent incomplete input. A writer
/// used here should itself provide bounded writes; the timeout is checked before and immediately
/// after the write and governs lock acquisition plus response polling.
pub fn probe_position_with<W: Write>(
    writer: &mut W,
    timeout: Duration,
) -> io::Result<CursorPositionProbe> {
    if is_raw_mode_enabled() {
        return probe_position_raw(writer, timeout);
    }

    enable_raw_mode()?;
    let result = probe_position_raw(writer, timeout);
    let restore = disable_raw_mode();
    match result {
        Ok(probe) => {
            restore?;
            Ok(probe)
        }
        Err(error) => {
            let _ = restore;
            Err(error)
        }
    }
}

fn probe_position_raw<W: Write>(
    writer: &mut W,
    timeout: Duration,
) -> io::Result<CursorPositionProbe> {
    // yututui patch: propagate poll/read errors and use one absolute query deadline rather than
    // restarting a two-second wait after each ignored error (crossterm PR #1067).
    match probe_cursor_position_with(writer, timeout)? {
        CursorPositionQuery::Position(column, row) => {
            Ok(CursorPositionProbe::Position(column, row))
        }
        CursorPositionQuery::DeferredForPendingInput => {
            Ok(CursorPositionProbe::DeferredForPendingInput)
        }
        CursorPositionQuery::DeferredForRecentInput => {
            Ok(CursorPositionProbe::DeferredForRecentInput)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_position_reports_recent_pending_input_as_would_block() {
        let error =
            legacy_position_result(CursorPositionProbe::DeferredForPendingInput).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    }
}
