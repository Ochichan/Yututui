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
            DaemonEvent::Player(
                crate::player::PlayerEvent::TimePos(_) | crate::player::PlayerEvent::CacheTime(_),
            ) => ObserverPlan::PROGRESS,
            // A lyrics result never changes player/queue/media state — skip projections.
            DaemonEvent::Remote(RemoteEvent::SessionSubscribe { .. })
            | DaemonEvent::Lyrics(_)
            | DaemonEvent::Download(_)
            | DaemonEvent::Transfer(_)
            | DaemonEvent::Ai(
                crate::ai::AiEvent::Thinking(_)
                | crate::ai::AiEvent::Chat(_)
                | crate::ai::AiEvent::Error(_)
                | crate::ai::AiEvent::Suggestions(_),
            )
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
            DaemonEvent::Player(crate::player::PlayerEvent::TimePos(_))
        );
        let fingerprint = plan.project_state.then(|| engine.media_fingerprint());
        (plan, position_turn, fingerprint)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_events_skip_owner_projections_but_keep_scrobble_heartbeat() {
        for event in [
            DaemonEvent::Player(crate::player::PlayerEvent::TimePos(7.0)),
            DaemonEvent::Player(crate::player::PlayerEvent::CacheTime(Some(9.0))),
        ] {
            let plan = event.observer_plan();
            assert!(!plan.project_state, "progress must skip projection");
            assert!(
                plan.drive_scrobble_heartbeat,
                "progress must retain the 1 Hz scrobble clock"
            );
        }

        assert_eq!(
            DaemonEvent::Player(crate::player::PlayerEvent::Duration(Some(180.0))).observer_plan(),
            ObserverPlan::PROJECTED,
            "a real media facet still runs media/remote observers"
        );
        assert_eq!(DaemonEvent::Signal.observer_plan(), ObserverPlan::INERT);
        assert_eq!(
            DaemonEvent::Transfer(crate::transfer::actor::TransferEvent::AuthError(
                "failed".to_owned()
            ))
            .observer_plan(),
            ObserverPlan::INERT
        );
    }

    #[test]
    fn ai_chat_projection_is_inert_but_actions_are_projected() {
        assert_eq!(
            DaemonEvent::Ai(crate::ai::AiEvent::Chat("hello".to_owned())).observer_plan(),
            ObserverPlan::INERT
        );
        assert_eq!(
            DaemonEvent::Ai(crate::ai::AiEvent::Enqueue(Vec::new())).observer_plan(),
            ObserverPlan::PROJECTED
        );
    }
}
