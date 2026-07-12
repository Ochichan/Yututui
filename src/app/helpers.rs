//! Small cross-reducer helper functions.

use super::*;

/// Build the lyrics-fetch effect for `song`.
pub(in crate::app) fn fetch_lyrics_cmd(song: &Song) -> Cmd {
    Cmd::FetchLyrics {
        video_id: song.video_id.clone(),
        artist: song.artist.clone(),
        title: song.title.clone(),
    }
}

pub(in crate::app) fn song_label(song: &Song) -> String {
    if song.artist.trim().is_empty() {
        song.title.clone()
    } else {
        format!("{} — {}", song.title, song.artist)
    }
}

pub(in crate::app) fn rect_contains(rect: Rect, col: u16, row: u16) -> bool {
    col >= rect.x
        && col < rect.x.saturating_add(rect.width)
        && row >= rect.y
        && row < rect.y.saturating_add(rect.height)
}

pub(crate) use crate::util::browser::open_in_browser;

/// Spawn a borderless, always-on-top mpv overlay window for `url`, returning the child so the
/// caller can track and later close it. Stdio is nulled so mpv can't touch the TUI's terminal.
/// Cookies are forwarded to mpv's bundled yt-dlp (same option as the audio instance) when set;
/// `--no-config` is intentionally omitted so the user's own mpv config applies to the video.
/// With `ipc_path`, the window exposes a JSON IPC endpoint and gets `--keep-open=yes`, so a
/// natural end pauses on the last frame (observable, and re-loadable) instead of exiting.
pub(in crate::app) fn spawn_video_overlay(
    url: &str,
    cookies: Option<&std::path::Path>,
    layout: crate::config::VideoOverlay,
    ipc_path: Option<&str>,
) -> Option<crate::util::process_tree::OwnedProcessTree> {
    use std::process::Stdio;
    let mut cmd =
        process::std_command(&crate::tools::mpv_program(), process::ProcessProfile::Media);
    cmd.arg(url);
    // The audio instance already owns the OS media session; without this the
    // overlay mpv would register a second, duplicate entry (mpv >= 0.39 does so
    // by default even for a plain window). As a CLI option it wins over the
    // user's mpv config, which is the point - the override lever for the audio
    // instance (`YTM_MPV_EXTRA`) intentionally doesn't reach the overlay.
    if crate::player::mpv::media_controls_flag_supported() {
        cmd.arg("--media-controls=no");
    }
    for arg in layout.mpv_window_args() {
        cmd.arg(arg);
    }
    if let Some(path) = ipc_path {
        cmd.arg(format!("--input-ipc-server={path}"));
        cmd.arg("--keep-open=yes");
    }
    for arg in crate::tools::mpv_ytdl_raw_option_args(cookies) {
        cmd.arg(arg);
    }
    // Pin ytdl_hook to the selected yt-dlp (managed/override), like the audio
    // instance - but with `-append`: this spawn honors the user's mpv config, and
    // plain `--script-opts=` would wipe their other script options.
    if let Some(sel) = crate::tools::ytdlp_selection()
        && let Some(pin) = sel.pin_for_mpv()
    {
        let pin = pin.canonicalize().unwrap_or_else(|_| pin.to_path_buf());
        cmd.arg(format!(
            "--script-opts-append=ytdl_hook-ytdl_path={}",
            pin.display()
        ));
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        cmd.creation_flags(DETACHED_PROCESS);
    }
    cmd.spawn()
        .map(|child| {
            crate::util::process_tree::OwnedProcessTree::new(child, process::ProcessProfile::Media)
        })
        .ok()
}
