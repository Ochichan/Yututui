//! Shared bounded writes for the audio and video mpv JSON IPC clients.

use std::io;

use interprocess::local_socket::tokio::Stream;
use tokio::io::AsyncWriteExt;
use tokio::time::Duration;

const WRITE_TIMEOUT: Duration = Duration::from_secs(2);

pub(in crate::player) async fn write_json(conn: &Stream, json: &str) -> io::Result<()> {
    tokio::time::timeout(WRITE_TIMEOUT, async {
        let mut writer = conn;
        writer.write_all(json.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "mpv IPC write timed out"))?
}
