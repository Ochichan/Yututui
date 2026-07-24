use super::{
    LONG_FORM_SEEK_OPTIMIZATION_CAPABILITY, PERSONAL_EXPORT_CAPABILITY,
    PERSONAL_STATE_V2_CAPABILITY, RETAINED_REQUEST_OUTCOMES_CAPABILITY,
};

pub(super) fn daemon_capabilities() -> Vec<String> {
    vec![
        "remote-control".to_string(),
        "status".to_string(),
        "queue-control".to_string(),
        RETAINED_REQUEST_OUTCOMES_CAPABILITY.to_string(),
        "headless-playback".to_string(),
        "session-resume".to_string(),
        "autoplay-streaming".to_string(),
        "search-playback".to_string(),
        // v8 sessions with live push (docs/gui/02 §10).
        "events-v8".to_string(),
        PERSONAL_EXPORT_CAPABILITY.to_string(),
        PERSONAL_STATE_V2_CAPABILITY.to_string(),
        LONG_FORM_SEEK_OPTIMIZATION_CAPABILITY.to_string(),
        // C6: the entire deferred v8 GUI command surface is dispatched (queue ops,
        // rating, video, library, playlists, downloads, AI, accounts, transfer,
        // keymap/theme) — advertising this dissolves the frontend's patch-bay gates.
        "v8-commands".to_string(),
    ]
}
