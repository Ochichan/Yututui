use super::engine::DaemonEngine;
use super::{DaemonEvent, RemoteEvent};

/// Expensive post-event owner projections are unnecessary for ordinary playback clocks. Keep the
/// scrobble heartbeat independent so a progress-only workload still advances listening time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ObserverPlan {
    pub(super) project_state: bool,
    pub(super) drive_scrobble_heartbeat: bool,
}

impl ObserverPlan {
    const INERT: Self = Self {
        project_state: false,
        drive_scrobble_heartbeat: false,
    };
    const PROGRESS: Self = Self {
        project_state: false,
        drive_scrobble_heartbeat: true,
    };
    const PROJECTED: Self = Self {
        project_state: true,
        drive_scrobble_heartbeat: true,
    };
}

impl DaemonEvent {
    pub(super) fn observer_plan(&self) -> ObserverPlan {
        match self {
            // Live mpv IPC wraps file-bound events (`TimePos`, `CacheTime`, `Duration`, etc.) in
            // `PlayerEvent::FileScoped`; classify through `unscoped()` so progress turns are not
            // misclassified as full projections.
            DaemonEvent::Player(event) => match event.unscoped() {
                crate::player::PlayerEvent::TimePos(_)
                | crate::player::PlayerEvent::CacheTime(_) => ObserverPlan::PROGRESS,
                crate::player::PlayerEvent::AudioDeviceList(_)
                | crate::player::PlayerEvent::AudioDeviceRefreshFailed(_)
                | crate::player::PlayerEvent::AudioDeviceChanged(_)
                | crate::player::PlayerEvent::CurrentAudioOutput(_)
                | crate::player::PlayerEvent::AudioDeviceSelectionResult { .. } => {
                    ObserverPlan::INERT
                }
                _ => ObserverPlan::PROJECTED,
            },
            // A lyrics result never changes player/queue/media state — skip projections.
            DaemonEvent::Remote(RemoteEvent::SessionSubscribe { .. })
            | DaemonEvent::Lyrics(_)
            | DaemonEvent::SponsorBlock(_)
            | DaemonEvent::Signal
            | DaemonEvent::TelemetryWake => ObserverPlan::INERT,
            _ => ObserverPlan::PROJECTED,
        }
    }

    pub(super) fn observer_context(
        &self,
        engine: &DaemonEngine,
    ) -> (ObserverPlan, bool, Option<u64>) {
        let plan = self.observer_plan();
        let position_turn = matches!(
            self,
            DaemonEvent::Player(event)
                if matches!(event.unscoped(), crate::player::PlayerEvent::TimePos(_))
        );
        let fingerprint = plan.project_state.then(|| engine.media_fingerprint());
        (plan, position_turn, fingerprint)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::daemon::engine::EngineState;
    use crate::player::PlayerEvent;

    fn test_engine() -> DaemonEngine {
        DaemonEngine::with_state(
            EngineState {
                config: crate::config::Config::default(),
                station: crate::station::StationStore::default(),
                library: crate::library::Library::default(),
                playlists: crate::playlists::Playlists::default(),
                signals: crate::signals::Signals::default(),
            },
            Arc::new(|_| {}),
        )
    }

    #[test]
    fn progress_events_skip_owner_projections_but_keep_scrobble_heartbeat() {
        let engine = test_engine();
        for (event, expect_position_turn) in [
            (DaemonEvent::Player(PlayerEvent::TimePos(7.0)), true),
            (
                DaemonEvent::Player(PlayerEvent::file_scoped(1, PlayerEvent::TimePos(7.0))),
                true,
            ),
            (
                DaemonEvent::Player(PlayerEvent::CacheTime(Some(9.0))),
                false,
            ),
            (
                DaemonEvent::Player(PlayerEvent::file_scoped(
                    1,
                    PlayerEvent::CacheTime(Some(9.0)),
                )),
                false,
            ),
        ] {
            let (plan, position_turn, fingerprint) = event.observer_context(&engine);
            assert!(!plan.project_state, "progress must skip projection");
            assert!(
                plan.drive_scrobble_heartbeat,
                "progress must retain the 1 Hz scrobble clock"
            );
            assert_eq!(
                position_turn, expect_position_turn,
                "TimePos must arm media position rebasing even when file-scoped"
            );
            assert!(
                fingerprint.is_none(),
                "progress turns must not hash media state before dispatch"
            );
        }

        for duration in [
            DaemonEvent::Player(PlayerEvent::Duration(Some(180.0))),
            DaemonEvent::Player(PlayerEvent::file_scoped(
                1,
                PlayerEvent::Duration(Some(180.0)),
            )),
        ] {
            let (plan, position_turn, fingerprint) = duration.observer_context(&engine);
            assert_eq!(
                plan,
                ObserverPlan::PROJECTED,
                "a real media facet still runs media/remote observers"
            );
            assert!(!position_turn);
            assert!(fingerprint.is_some());
        }

        assert_eq!(
            DaemonEvent::Player(PlayerEvent::AudioDeviceList(Vec::new())).observer_plan(),
            ObserverPlan::INERT
        );
        assert_eq!(DaemonEvent::Signal.observer_plan(), ObserverPlan::INERT);
        assert_eq!(
            DaemonEvent::Lyrics(crate::lyrics::LyricsEvent::Result {
                video_id: "vid".to_owned(),
                lines: Default::default(),
            })
            .observer_plan(),
            ObserverPlan::INERT
        );
    }
}
