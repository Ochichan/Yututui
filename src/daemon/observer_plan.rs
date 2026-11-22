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

/// Pre-dispatch snapshot captured for the daemon owner loop's post-turn observer decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ObserverContext {
    pub(super) plan: ObserverPlan,
    pub(super) position_turn: bool,
    fingerprint_before: Option<u64>,
    position_epoch_before: Option<u64>,
}

impl ObserverContext {
    pub(super) fn position_epoch_changed(&self, engine: &DaemonEngine) -> bool {
        self.position_epoch_before
            .is_some_and(|before| before != engine.media_position_epoch())
    }

    pub(super) fn media_changed(&self, engine: &DaemonEngine) -> bool {
        self.fingerprint_before
            .is_some_and(|before| before != engine.media_fingerprint())
            || self.position_epoch_changed(engine)
    }
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

    pub(super) fn observer_context(&self, engine: &DaemonEngine) -> ObserverContext {
        let plan = self.observer_plan();
        let position_turn = matches!(
            self,
            DaemonEvent::Player(event)
                if matches!(event.unscoped(), crate::player::PlayerEvent::TimePos(_))
        );
        let fingerprint_before = plan.project_state.then(|| engine.media_fingerprint());
        // Track the u64 discontinuity epoch on fingerprint-free progress turns so a
        // SponsorBlock skip (`note_seek`) still triggers a full media snapshot publish
        // without hashing metadata on ordinary progress samples.
        let position_epoch_before =
            (!plan.project_state && position_turn).then(|| engine.media_position_epoch());
        ObserverContext {
            plan,
            position_turn,
            fingerprint_before,
            position_epoch_before,
        }
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
            let ctx = event.observer_context(&engine);
            assert!(!ctx.plan.project_state, "progress must skip projection");
            assert!(
                ctx.plan.drive_scrobble_heartbeat,
                "progress must retain the 1 Hz scrobble clock"
            );
            assert_eq!(
                ctx.position_turn, expect_position_turn,
                "TimePos must arm media position rebasing even when file-scoped"
            );
            assert!(
                ctx.fingerprint_before.is_none(),
                "progress turns must not hash media state before dispatch"
            );
            assert_eq!(ctx.position_epoch_before.is_some(), expect_position_turn);
            assert!(!ctx.media_changed(&engine));
        }

        for duration in [
            DaemonEvent::Player(PlayerEvent::Duration(Some(180.0))),
            DaemonEvent::Player(PlayerEvent::file_scoped(
                1,
                PlayerEvent::Duration(Some(180.0)),
            )),
        ] {
            let ctx = duration.observer_context(&engine);
            assert_eq!(
                ctx.plan,
                ObserverPlan::PROJECTED,
                "a real media facet still runs media/remote observers"
            );
            assert!(!ctx.position_turn);
            assert!(ctx.fingerprint_before.is_some());
            assert!(ctx.position_epoch_before.is_none());
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

    #[tokio::test]
    async fn progress_turn_marks_media_changed_only_when_sponsorblock_seek_succeeds() {
        let mut engine = crate::daemon::engine::tests::engine_with_queue(&["dQw4w9WgXcQ"]);
        let mut player_rx = engine.install_seek_parity_player("dQw4w9WgXcQ", 10.0, 180.0);
        engine.install_sponsorblock_parity_state(&[(60.0, 90.0)]);

        // Ordinary progress samples before the segment stay fingerprint-free and unchanged.
        for pos in [57.0, 58.0, 59.0] {
            let ev = DaemonEvent::Player(PlayerEvent::file_scoped(0, PlayerEvent::TimePos(pos)));
            let ctx = ev.observer_context(&engine);
            assert!(ctx.fingerprint_before.is_none());
            let DaemonEvent::Player(player_ev) = ev else {
                unreachable!()
            };
            let _ = engine.handle_player_event(player_ev).await;
            assert!(!ctx.position_epoch_changed(&engine));
            assert!(!ctx.media_changed(&engine));
        }

        // Entering the segment applies `note_seek` -> epoch bumps -> media publish required.
        let epoch_before = engine.media_position_epoch();
        let skip_ev = DaemonEvent::Player(PlayerEvent::file_scoped(0, PlayerEvent::TimePos(60.4)));
        let ctx = skip_ev.observer_context(&engine);
        assert!(ctx.fingerprint_before.is_none());
        let DaemonEvent::Player(player_ev) = skip_ev else {
            unreachable!()
        };
        let _ = engine.handle_player_event(player_ev).await;
        assert!(player_rx.try_recv().is_ok(), "seek command should be sent");
        assert!(ctx.position_epoch_changed(&engine));
        assert!(ctx.media_changed(&engine));
        assert_eq!(engine.media_snapshot().position_epoch, epoch_before + 1);

        // When the seek fails before `note_seek` (closed player command channel), state stays clean.
        let mut failing_engine = crate::daemon::engine::tests::engine_with_queue(&["dQw4w9WgXcQ"]);
        let failing_rx = failing_engine.install_seek_parity_player("dQw4w9WgXcQ", 10.0, 180.0);
        failing_engine.install_sponsorblock_parity_state(&[(60.0, 90.0)]);
        for pos in [57.0, 58.0, 59.0] {
            let _ = failing_engine
                .handle_player_event(PlayerEvent::file_scoped(0, PlayerEvent::TimePos(pos)))
                .await;
        }
        drop(failing_rx);
        let failed_ev =
            DaemonEvent::Player(PlayerEvent::file_scoped(0, PlayerEvent::TimePos(60.4)));
        let ctx = failed_ev.observer_context(&failing_engine);
        let DaemonEvent::Player(player_ev) = failed_ev else {
            unreachable!()
        };
        let _ = failing_engine.handle_player_event(player_ev).await;
        assert!(!ctx.position_epoch_changed(&failing_engine));
        assert!(!ctx.media_changed(&failing_engine));
    }
}
