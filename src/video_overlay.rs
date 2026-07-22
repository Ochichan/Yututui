//! The external mpv video-overlay spawn, shared by both owners: the TUI's
//! admission-atomic transition (`src/app/video_transition.rs`) and the daemon's
//! `PlayVideo` remote command host both launch the same window.

fn video_overlay_base_args(url: &str, volume_percent: i64) -> Vec<String> {
    let volume_percent = volume_percent.clamp(0, crate::playback_policy::VOLUME_MAX);
    vec![url.to_owned(), format!("--volume={volume_percent}")]
}

/// Spawn a borderless, always-on-top mpv overlay window for `url`, returning the child so the
/// caller can track and later close it. Stdio is nulled so mpv can't touch the TUI's terminal.
/// Cookies are forwarded to mpv's bundled yt-dlp (same option as the audio instance) when set;
/// `--no-config` is intentionally omitted so the user's own mpv config applies to the video.
/// `volume_percent` overrides only its startup volume with the owner's live music volume.
/// With `ipc_path`, the window exposes a JSON IPC endpoint and gets `--keep-open=yes`, so a
/// natural end pauses on the last frame (observable, and re-loadable) instead of exiting.
pub fn spawn_video_overlay(
    url: &str,
    cookies: Option<&std::path::Path>,
    layout: crate::config::VideoOverlay,
    volume_percent: i64,
    ipc_path: Option<&str>,
) -> Option<crate::util::process_tree::OwnedProcessTree> {
    if let Err(error) = crate::player::lifetime::ensure_media_start_allowed() {
        tracing::warn!(%error, "refusing video overlay spawn during shutdown");
        return None;
    }
    if let Err(error) = crate::player::mpv::ensure_lifeline_supported() {
        tracing::warn!(%error, "refusing to spawn an unprotected video overlay");
        return None;
    }
    let mut args = video_overlay_base_args(url, volume_percent);
    // The audio instance already owns the OS media session; without this the
    // overlay mpv would register a second, duplicate entry (mpv >= 0.39 does so
    // by default even for a plain window). As a CLI option it wins over the
    // user's mpv config, which is the point - the override lever for the audio
    // instance (`YTM_MPV_EXTRA`) intentionally doesn't reach the overlay.
    if crate::player::mpv::media_controls_flag_supported() {
        args.push("--media-controls=no".to_owned());
    }
    for arg in layout.mpv_window_args() {
        args.push(arg);
    }
    if let Some(path) = ipc_path {
        args.push(format!("--input-ipc-server={path}"));
        args.push("--keep-open=yes".to_owned());
    }
    for arg in crate::tools::mpv_ytdl_raw_option_args(cookies) {
        args.push(arg);
    }
    // Pin ytdl_hook to the selected yt-dlp (managed/override), like the audio
    // instance - but with `-append`: this spawn honors the user's mpv config, and
    // plain `--script-opts=` would wipe their other script options.
    if let Some(sel) = crate::tools::ytdlp_selection()
        && let Some(pin) = sel.pin_for_mpv()
    {
        let pin = pin.canonicalize().unwrap_or_else(|_| pin.to_path_buf());
        args.push(format!(
            "--script-opts-append=ytdl_hook-ytdl_path={}",
            pin.display()
        ));
    }
    crate::player::guardian::spawn(&crate::tools::mpv_program(), args, true)
        .map(crate::util::process_tree::OwnedProcessTree::new_guarded)
        .inspect_err(|error| tracing::warn!(%error, "protected video overlay spawn failed"))
        .ok()
}

#[cfg(test)]
mod tests {
    use super::video_overlay_base_args;

    #[test]
    fn base_args_inherit_live_volume_once_and_clamp() {
        for (input, expected) in [(-1, 0), (0, 0), (37, 37), (100, 100), (101, 100)] {
            let args = video_overlay_base_args("https://example.test/watch", input);
            let expected = format!("--volume={expected}");

            assert_eq!(args, ["https://example.test/watch".to_owned(), expected]);
        }
    }
}
