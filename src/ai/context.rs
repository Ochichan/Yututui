use crate::app::AiContext;

/// A compact, human-readable snapshot of player state for the model's first turn.
pub(super) fn context_summary(ctx: &AiContext) -> String {
    let mut summary = String::from("Current player state:\n");
    summary.push_str(&format!(
        "- Now playing: {}\n",
        ctx.current_track.as_deref().unwrap_or("nothing")
    ));
    if let Some(station) = &ctx.current_radio_station {
        summary.push_str(&format!("- Current radio station: {station}\n"));
        match &ctx.current_radio_now_playing {
            Some(track) => summary.push_str(&format!("- Current radio stream track: {track}\n")),
            None => summary.push_str(
                "- Current radio stream track: unavailable; this station has not exposed now-playing metadata yet\n",
            ),
        }
    }
    if !ctx.queue_upcoming.is_empty() {
        summary.push_str(&format!("- Up next: {}\n", ctx.queue_upcoming.join("; ")));
    }
    summary.push_str(&format!(
        "- Queue: {} track(s), {} remaining\n",
        ctx.queue_len, ctx.queue_remaining
    ));
    if !ctx.recent_history.is_empty() {
        summary.push_str(&format!(
            "- Recently played: {}\n",
            ctx.recent_history.join("; ")
        ));
    }
    if !ctx.favorites.is_empty() {
        summary.push_str(&format!("- Favorites: {}\n", ctx.favorites.join("; ")));
    }
    if !ctx.playlists.is_empty() {
        let playlists: Vec<String> = ctx
            .playlists
            .iter()
            .map(|playlist| format!("{} ({})", playlist.name, playlist.count))
            .collect();
        summary.push_str(&format!("- Playlists: {}\n", playlists.join("; ")));
    }
    summary.push_str(&format!(
        "- Autoplay streaming: {}\n",
        if ctx.autoplay_streaming { "on" } else { "off" }
    ));
    summary.push_str(&format!(
        "- Signed in: {}\n",
        if ctx.authenticated {
            "yes"
        } else {
            "no (anonymous)"
        }
    ));
    summary
}
