//! Main-WebView subscription contract layered on the process-wide v8 gateway.

use crate::remote::proto::Topic;

use super::gateway::GatewayHandle;

/// Topics installed by the main Svelte frontend in `gui/src/main.ts`.
///
/// A WebView rebuild happens inside the same v8 session, whose server-side subscriptions are
/// intentionally idempotent. Re-sending an ordinary `sub` would therefore not replay snapshots
/// for an already-subscribed topic; the ready page must refresh this complete set instead.
pub const MAIN_FRONTEND_TOPICS: &[Topic] = &[
    Topic::Player,
    Topic::Queue,
    Topic::Lyrics,
    Topic::Search,
    Topic::Library,
    Topic::Playlists,
    Topic::Ai,
    Topic::Downloads,
    Topic::Transfer,
    Topic::Accounts,
    Topic::Settings,
    Topic::System,
];

/// Refresh every main-window topic after the current WebView generation has completed its
/// `FrontendReady` handshake. While connecting/offline, its preceding ordinary subscription is
/// retained by the gateway and becomes part of the next session's initial snapshot baseline.
pub fn refresh_ready_main_frontend(gateway: Option<&GatewayHandle>) {
    let Some(gateway) = gateway.filter(|gateway| gateway.is_online()) else {
        return;
    };
    if let Err(error) = gateway.refresh_topics(MAIN_FRONTEND_TOPICS) {
        tracing::debug!(
            target: "ytt_desktop",
            reason = error.code(),
            "could not refresh ready main frontend topics"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_topics_match_the_boot_subscription_contract() {
        assert_eq!(
            MAIN_FRONTEND_TOPICS
                .iter()
                .map(|topic| topic.wire_str())
                .collect::<Vec<_>>(),
            vec![
                "player",
                "queue",
                "lyrics",
                "search",
                "library",
                "playlists",
                "ai",
                "downloads",
                "transfer",
                "accounts",
                "settings",
                "system",
            ]
        );
    }
}
