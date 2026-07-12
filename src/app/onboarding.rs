use std::cell::Cell;
use std::time::{Duration, Instant};

use super::*;

const SEARCH_ONBOARDING_TTL: Duration = Duration::from_secs(10);

#[derive(Default)]
pub struct OnboardingState {
    pending: bool,
    armed: bool,
    visible_elapsed: Cell<Duration>,
    visible_since: Cell<Option<Instant>>,
}

impl OnboardingState {
    /// Whether the one-shot hint is armed. The render pass separately records whether the
    /// current frame can actually show it so hidden time never consumes the TTL.
    pub fn visible(&self) -> bool {
        self.pending && self.armed
    }

    fn record_render_eligibility(&self, eligible: bool, now: Instant) {
        if self.visible() && eligible {
            if self.visible_since.get().is_none() {
                self.visible_since.set(Some(now));
            }
            return;
        }

        if let Some(started) = self.visible_since.take() {
            let elapsed = now.checked_duration_since(started).unwrap_or_default();
            self.visible_elapsed
                .set(self.visible_elapsed.get().saturating_add(elapsed));
        }
    }

    fn tick_expired(&self, now: Instant) -> bool {
        if !self.visible() {
            return false;
        }
        let current = self
            .visible_since
            .get()
            .and_then(|started| now.checked_duration_since(started))
            .unwrap_or_default();
        self.visible_elapsed.get().saturating_add(current) >= SEARCH_ONBOARDING_TTL
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
        if self.onboarding.pending && !self.onboarding.armed && self.tool_setup.is_none() {
            self.onboarding.armed = true;
            self.dirty = true;
        }
    }

    /// Record whether this frame can show the hint and return the same render verdict. The
    /// renderer owns the exact cell grid; App owns screen/modal state, so this is their single
    /// shared eligibility boundary.
    pub(crate) fn search_onboarding_render_eligible(&self, layout_sufficient: bool) -> bool {
        self.search_onboarding_render_eligible_at(layout_sufficient, Instant::now())
    }

    fn search_onboarding_render_eligible_at(&self, layout_sufficient: bool, now: Instant) -> bool {
        let covering_surface = self.art_overlay_mask() != 0
            || self.local_mode.pending_confirm.is_some()
            || self.overlays.spotify_picker.is_some()
            || self.overlays.recording_settings.is_some()
            || self.overlays.recordings_browser.is_some()
            || self.overlays.now_playing_overlay.is_some();
        let eligible = self.onboarding.visible()
            && self.mode == Mode::Player
            && layout_sufficient
            && !covering_surface;
        self.onboarding.record_render_eligibility(eligible, now);
        eligible
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
        let started = Instant::now();
        assert!(app.search_onboarding_render_eligible_at(true, started));
        let cmds = app.tick_search_onboarding(started + Duration::from_secs(10));
        assert!(!app.onboarding.visible());
        assert!(matches!(
            cmds.as_slice(),
            [Cmd::Persist(PersistCmd::Config(_))]
        ));
    }

    #[test]
    fn hidden_intervals_do_not_consume_the_onboarding_ttl() {
        let mut app = App::new(50);
        app.prepare_search_onboarding(true);
        let started = Instant::now();
        assert!(app.search_onboarding_render_eligible_at(true, started));

        app.mode = Mode::Library;
        assert!(!app.search_onboarding_render_eligible_at(true, started + Duration::from_secs(4)));
        assert!(
            app.tick_search_onboarding(started + Duration::from_secs(30))
                .is_empty()
        );

        app.mode = Mode::Player;
        assert!(app.search_onboarding_render_eligible_at(true, started + Duration::from_secs(30)));
        assert!(
            app.tick_search_onboarding(started + Duration::from_secs(35))
                .is_empty()
        );
        let cmds = app.tick_search_onboarding(started + Duration::from_secs(36));
        assert!(matches!(
            cmds.as_slice(),
            [Cmd::Persist(PersistCmd::Config(_))]
        ));
    }

    #[test]
    fn small_layouts_and_covering_modals_pause_the_onboarding_ttl() {
        let mut app = App::new(50);
        app.prepare_search_onboarding(true);
        let started = Instant::now();
        assert!(app.search_onboarding_render_eligible_at(true, started));

        assert!(!app.search_onboarding_render_eligible_at(false, started + Duration::from_secs(3)));
        assert!(
            app.tick_search_onboarding(started + Duration::from_secs(20))
                .is_empty()
        );

        assert!(app.search_onboarding_render_eligible_at(true, started + Duration::from_secs(20)));
        app.show_tool_setup(ToolSetupContext::Startup, vec!["mpv"]);
        assert!(!app.search_onboarding_render_eligible_at(true, started + Duration::from_secs(22)));
        assert!(
            app.tick_search_onboarding(started + Duration::from_secs(40))
                .is_empty()
        );

        app.tool_setup = None;
        assert!(app.search_onboarding_render_eligible_at(true, started + Duration::from_secs(40)));
        let cmds = app.tick_search_onboarding(started + Duration::from_secs(45));
        assert!(matches!(
            cmds.as_slice(),
            [Cmd::Persist(PersistCmd::Config(_))]
        ));
    }
}
