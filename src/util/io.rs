//! Shared low-level async I/O helpers.
//!
//! One canonical bounded line reader for every newline-framed control stream we consume
//! (the remote one-shot/session protocol and the mpv JSON IPC socket). Byte-at-a-time so a
//! hostile or buggy peer can never make us buffer more than the cap before a `\n` arrives;
//! cheap in practice because every caller wraps the stream in a `BufReader`, so each read
//! is a buffer copy, not a syscall.

use std::io;

use tokio::io::{AsyncRead, AsyncReadExt};

/// Outcome of [`read_bounded_line`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoundedLine {
    /// A full `\n`-terminated line was appended to the buffer (the `\n` is included).
    Line,
    /// The buffer reached `max_bytes` before a newline arrived — treat the peer as
    /// misbehaving and tear the connection down. The buffer holds the oversized prefix.
    TooLarge,
    /// The stream hit EOF before a newline (zero or more bytes may already be buffered).
    Eof,
}

/// Read one `\n`-terminated line into `line`, refusing to grow it past `max_bytes`.
///
/// `line` is NOT cleared — callers reuse the allocation and clear between reads. Returns an
/// `io::Error` only for a genuine transport error; a clean EOF is [`BoundedLine::Eof`], not
/// an error, so each caller decides whether EOF is normal (mpv closing) or fatal (a remote
/// peer vanishing mid-request).
pub async fn read_bounded_line<R: AsyncRead + Unpin>(
    reader: &mut R,
    line: &mut Vec<u8>,
    max_bytes: usize,
) -> io::Result<BoundedLine> {
    let mut byte = [0u8; 1];
    loop {
        let n = reader.read(&mut byte).await?;
        if n == 0 {
            return Ok(BoundedLine::Eof);
        }
        line.push(byte[0]);
        if line.len() > max_bytes {
            return Ok(BoundedLine::TooLarge);
        }
        if byte[0] == b'\n' {
            return Ok(BoundedLine::Line);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reads_a_line_including_newline() {
        let mut src = &b"hello\nworld\n"[..];
        let mut line = Vec::new();
        assert_eq!(
            read_bounded_line(&mut src, &mut line, 64).await.unwrap(),
            BoundedLine::Line
        );
        assert_eq!(line, b"hello\n");
    }

    #[tokio::test]
    async fn flags_oversized_line_without_unbounded_growth() {
        let mut src = &b"xxxxxxxxxx"[..]; // 10 bytes, no newline
        let mut line = Vec::new();
        assert_eq!(
            read_bounded_line(&mut src, &mut line, 4).await.unwrap(),
            BoundedLine::TooLarge
        );
        // Stopped one byte past the cap, never buffered the whole stream.
        assert_eq!(line.len(), 5);
    }

    #[tokio::test]
    async fn reports_eof_as_outcome_not_error() {
        let mut src = &b"partial"[..]; // no trailing newline
        let mut line = Vec::new();
        assert_eq!(
            read_bounded_line(&mut src, &mut line, 64).await.unwrap(),
            BoundedLine::Eof
        );
        assert_eq!(line, b"partial");
    }
}
