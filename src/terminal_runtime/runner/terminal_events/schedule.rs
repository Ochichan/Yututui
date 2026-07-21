use std::time::{Duration, Instant};

use super::AMBIGUOUS_RETRY_INTERVAL;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LivenessAction {
    Heartbeat,
    OwnerProbe,
}

pub(super) struct LivenessSchedule {
    next_heartbeat: Instant,
    next_owner_probe: Instant,
    heartbeat_interval: Duration,
    owner_probe_interval: Duration,
}

impl LivenessSchedule {
    pub(super) fn new(
        now: Instant,
        heartbeat_interval: Duration,
        owner_probe_interval: Duration,
    ) -> Self {
        Self {
            next_heartbeat: now + heartbeat_interval,
            next_owner_probe: now + owner_probe_interval,
            heartbeat_interval,
            owner_probe_interval,
        }
    }

    pub(super) fn due(&self, now: Instant) -> Option<LivenessAction> {
        let heartbeat_due = now >= self.next_heartbeat;
        let owner_probe_due = now >= self.next_owner_probe;
        match (heartbeat_due, owner_probe_due) {
            (false, false) => None,
            (true, false) => Some(LivenessAction::Heartbeat),
            (false, true) => Some(LivenessAction::OwnerProbe),
            (true, true) if self.next_owner_probe <= self.next_heartbeat => {
                Some(LivenessAction::OwnerProbe)
            }
            (true, true) => Some(LivenessAction::Heartbeat),
        }
    }

    pub(super) fn completed(&mut self, action: LivenessAction, now: Instant) {
        match action {
            LivenessAction::Heartbeat => self.next_heartbeat = now + self.heartbeat_interval,
            LivenessAction::OwnerProbe => {
                self.next_owner_probe = now + self.owner_probe_interval;
            }
        }
    }

    pub(super) fn retry(&mut self, action: LivenessAction, now: Instant) {
        match action {
            LivenessAction::Heartbeat => {
                self.next_heartbeat = now + AMBIGUOUS_RETRY_INTERVAL;
            }
            LivenessAction::OwnerProbe => {
                self.next_owner_probe = now + AMBIGUOUS_RETRY_INTERVAL;
            }
        }
    }

    pub(super) fn force_heartbeat(&mut self, now: Instant) {
        self.next_heartbeat = now;
    }

    pub(super) fn until_next(&self, now: Instant) -> Duration {
        self.next_heartbeat
            .min(self.next_owner_probe)
            .saturating_duration_since(now)
    }
}
