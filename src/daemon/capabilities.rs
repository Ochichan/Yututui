use super::{
    LONG_FORM_SEEK_OPTIMIZATION_CAPABILITY, PERSONAL_EXPORT_CAPABILITY,
    PERSONAL_STATE_V2_CAPABILITY, RETAINED_REQUEST_OUTCOMES_CAPABILITY, WEB_DAV_SYNC_CAPABILITY,
};
use crate::remote::OPEN_SUBSONIC_CAPABILITY;

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
        // v8 sessions with live push.
        "events-v8".to_string(),
        PERSONAL_EXPORT_CAPABILITY.to_string(),
        PERSONAL_STATE_V2_CAPABILITY.to_string(),
        WEB_DAV_SYNC_CAPABILITY.to_string(),
        OPEN_SUBSONIC_CAPABILITY.to_string(),
        LONG_FORM_SEEK_OPTIMIZATION_CAPABILITY.to_string(),
    ]
}
