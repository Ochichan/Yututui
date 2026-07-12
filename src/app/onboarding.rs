use std::time::{Duration, Instant};

use super::*;

const SEARCH_ONBOARDING_TTL: Duration = Duration::from_secs(10);

#[derive(Default)]
pub struct OnboardingState {
    pending: bool,
    started_at: Option<Instant>,
}

impl OnboardingState {
    pub fn visible(&self) -> bool {
        self.pending && self.started_at.is_some()
    }

    fn tick_expired(&self, now: Instant) -> bool {
        self.started_at
            .is_some_and(|started| now.duration_since(started) >= SEARCH_ONBOARDING_TTL)
    }
}

impl App {
    /// Arm the hint only for a writable, genuinely fresh profile. A tool-setup card takes
    /// precedence; dismissing it starts the ten-second clock.
    pub fn prepare_search_onboarding(&mut self, writable: bool) {
        self.onboarding.pending = writable && !self.config.search_onboarding_seen;
        self.arm_search_onboarding();
    }

    pub(in crate::app) fn arm_search_onboarding(&mut self) {
        if self.onboarding.pending
            && self.onboarding.started_at.is_none()
            && self.tool_setup.is_none()
        {
            self.onboarding.started_at = Some(Instant::now());
            self.dirty = true;
        }
    }

    pub(in crate::app) fn complete_search_onboarding(&mut self) -> Vec<Cmd> {
        if !self.onboarding.pending {
            return Vec::new();
        }
        self.onboarding = OnboardingState::default();
        self.config.search_onboarding_seen = true;
        self.dirty = true;
        vec![Cmd::Persist(PersistCmd::Config(Box::new(
            self.config.clone(),
        )))]
    }

    pub(in crate::app) fn tick_search_onboarding(&mut self, now: Instant) -> Vec<Cmd> {
        if self.onboarding.tick_expired(now) {
            self.complete_search_onboarding()
        } else {
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_entry_completes_and_persists_onboarding() {
        let mut app = App::new(50);
        app.prepare_search_onboarding(true);
        assert!(app.onboarding.visible());
        let cmds = app.complete_search_onboarding();
        assert!(app.config.search_onboarding_seen);
        assert!(matches!(
            cmds.as_slice(),
            [Cmd::Persist(PersistCmd::Config(_))]
        ));
    }

    #[test]
    fn search_onboarding_expires_after_ten_visible_seconds() {
        let mut app = App::new(50);
        app.prepare_search_onboarding(true);
        let started = app.onboarding.started_at.unwrap();
        let cmds = app.tick_search_onboarding(started + Duration::from_secs(10));
        assert!(!app.onboarding.visible());
        assert!(matches!(
            cmds.as_slice(),
            [Cmd::Persist(PersistCmd::Config(_))]
        ));
    }
}
