use std::cmp::Ordering;

use super::*;
use crate::config::{BEGINNER_TUTORIAL_VERSION, BeginnerTutorialProgress};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum BeginnerStep {
    #[default]
    Welcome,
    NavigationHelp,
    Player,
    Search,
    Library,
    DjGem,
    Settings,
    Finish,
}

impl BeginnerStep {
    const ORDER: [Self; 8] = [
        Self::Welcome,
        Self::NavigationHelp,
        Self::Player,
        Self::Search,
        Self::Library,
        Self::DjGem,
        Self::Settings,
        Self::Finish,
    ];
    pub const COUNT: usize = Self::ORDER.len();

    pub fn id(self) -> &'static str {
        match self {
            Self::Welcome => "welcome",
            Self::NavigationHelp => "navigation_help",
            Self::Search => "search",
            Self::Player => "player",
            Self::Library => "library",
            Self::DjGem => "dj_gem",
            Self::Settings => "settings",
            Self::Finish => "finish",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Some(match id {
            "welcome" => Self::Welcome,
            "navigation_help" => Self::NavigationHelp,
            "search" => Self::Search,
            "player" => Self::Player,
            "library" => Self::Library,
            "dj_gem" => Self::DjGem,
            "settings" => Self::Settings,
            "finish" => Self::Finish,
            _ => return None,
        })
    }

    pub fn number(self) -> usize {
        self.order_index() + 1
    }

    fn next(self) -> Option<Self> {
        Self::ORDER.get(self.order_index() + 1).copied()
    }

    fn previous(self) -> Option<Self> {
        self.order_index()
            .checked_sub(1)
            .and_then(|index| Self::ORDER.get(index))
            .copied()
    }

    fn order_index(self) -> usize {
        Self::ORDER
            .iter()
            .position(|step| *step == self)
            .expect("BeginnerStep::ORDER must contain every step")
    }
}

#[derive(Default)]
pub struct OnboardingState {
    pending: bool,
    armed: bool,
    step: BeginnerStep,
    target_reached: bool,
    guide_focused: bool,
    mini_guide_focused: bool,
    selected: usize,
    skip_confirmation: bool,
    guide_focused_before_skip: bool,
    mini_guide_focused_before_skip: bool,
    settings_tab_visited: bool,
    startup_persist_pending: bool,
}

impl OnboardingState {
    pub fn visible(&self) -> bool {
        self.pending && self.armed
    }

    pub fn active(&self) -> bool {
        self.visible()
    }

    pub fn step(&self) -> BeginnerStep {
        self.step
    }

    pub fn target_reached(&self) -> bool {
        self.target_reached
    }

    pub fn guide_focused(&self) -> bool {
        self.guide_focused
    }

    pub fn guide_focused_for(&self, mini: bool) -> bool {
        if mini {
            self.mini_guide_focused
        } else {
            self.guide_focused
        }
    }

    pub fn skip_confirmation(&self) -> bool {
        self.skip_confirmation
    }

    pub fn settings_tab_visited(&self) -> bool {
        self.settings_tab_visited
    }

    pub fn selected_action(&self) -> OnboardingAction {
        if self.skip_confirmation {
            return if self.selected == 0 {
                OnboardingAction::CancelSkip
            } else {
                OnboardingAction::ConfirmSkip
            };
        }
        match self.selected {
            0 => OnboardingAction::Back,
            1 => OnboardingAction::Primary,
            _ => OnboardingAction::Skip,
        }
    }

    fn primary_index(&self) -> usize {
        1
    }

    fn action_count(&self) -> usize {
        if self.skip_confirmation { 2 } else { 3 }
    }

    fn select_primary(&mut self) {
        self.selected = self.primary_index();
    }

    fn select_skip(&mut self) {
        self.selected = self.action_count().saturating_sub(1);
    }

    fn move_selection(&mut self, forward: bool) {
        let count = self.action_count().max(1);
        self.selected = if forward {
            (self.selected + 1) % count
        } else {
            (self.selected + count - 1) % count
        };
    }
}

#[derive(Clone, Copy)]
pub(in crate::app) struct BeginnerObservation {
    help_visible: bool,
    volume: i64,
}

impl App {
    /// Prepare an event-driven tutorial only for a writable profile whose persisted setting is
    /// explicitly on. Tool setup remains the higher-priority startup surface and arms the coach
    /// when it is dismissed.
    pub fn prepare_beginner_onboarding(&mut self, writable: bool) {
        self.onboarding = OnboardingState::default();
        if !writable || !self.config.beginner_mode {
            return;
        }

        let progress = &self.config.beginner_tutorial;
        match progress.content_version.cmp(&BEGINNER_TUTORIAL_VERSION) {
            Ordering::Greater => return,
            Ordering::Less => {
                self.config.beginner_tutorial = BeginnerTutorialProgress::welcome();
                self.onboarding.startup_persist_pending = true;
            }
            Ordering::Equal => {
                if BeginnerStep::from_id(&progress.next_step).is_none() {
                    self.config.beginner_tutorial = BeginnerTutorialProgress::welcome();
                    self.onboarding.startup_persist_pending = true;
                }
            }
        }

        let step = BeginnerStep::from_id(&self.config.beginner_tutorial.next_step)
            .unwrap_or(BeginnerStep::Welcome);
        self.onboarding.pending = true;
        self.onboarding.step = step;
        self.onboarding.target_reached = self.beginner_step_target_reached(step);
        self.onboarding.guide_focused = step == BeginnerStep::Welcome;
        self.onboarding.select_primary();
        self.arm_beginner_onboarding();
    }

    pub(crate) fn take_beginner_startup_persist(&mut self) -> Option<Cmd> {
        if !std::mem::take(&mut self.onboarding.startup_persist_pending) {
            return None;
        }
        Some(self.persist_beginner_config())
    }

    pub(in crate::app) fn arm_beginner_onboarding(&mut self) {
        if self.onboarding.pending && !self.onboarding.armed && self.tool_setup.is_none() {
            self.onboarding.armed = true;
            self.dirty = true;
        }
    }

    pub fn beginner_coach_visible(&self) -> bool {
        self.onboarding.visible() && !self.beginner_higher_overlay_open()
    }

    pub(in crate::app) fn beginner_higher_overlay_open(&self) -> bool {
        self.tool_setup.is_some()
            || self.queue_popup.open
            || self.search_filter.open
            || self.dropdowns.eq_open
            || self.dropdowns.streaming_open
            || self.dropdowns.search_source_open
            || self.overlays.help_visible
            || self.overlays.mouse_help_visible
            || self.overlays.about_visible
            || self.overlays.why_ai_visible
            || self.overlays.now_playing_overlay.is_some()
            || self.overlays.key_conflict.is_some()
            || self.overlays.pending_settings_confirm.is_some()
            || self.overlays.spotify_picker.is_some()
            || self.overlays.recording_settings.is_some()
            || self.overlays.recordings_browser.is_some()
            || self.overlays.context_menu.is_some()
            || self.radio_mode.pending_radio_mode_confirm.is_some()
            || self.local_mode.pending_confirm.is_some()
            || self.local_mode.pending_organize_confirm.is_some()
            || self.local_mode.pending_accept_all_confirm.is_some()
            || self.local_mode.pending_import_record_delete.is_some()
            || self.library_ui.create_input.is_some()
            || self.library_ui.confirm_playlist_delete.is_some()
            || self.library_ui.confirm_delete.is_some()
            || self.library_ui.confirm_download.is_some()
            || self.playlist_picker.is_some()
            || self
                .settings
                .as_ref()
                .is_some_and(|state| state.spotify_import_mode_dropdown.is_some())
    }

    pub(in crate::app) fn onboarding_observation(&self) -> BeginnerObservation {
        BeginnerObservation {
            help_visible: self.overlays.help_visible,
            volume: self.playback.volume,
        }
    }

    pub(in crate::app) fn observe_beginner_tutorial(
        &mut self,
        before: BeginnerObservation,
    ) -> Vec<Cmd> {
        if !self.onboarding.visible()
            || self.bridges.ui_tier.get() == crate::ui::layout::UiTier::Mini
        {
            return Vec::new();
        }

        if self.onboarding.step == BeginnerStep::NavigationHelp
            && !before.help_visible
            && self.overlays.help_visible
        {
            return self.advance_beginner_step();
        }

        if self.onboarding.step == BeginnerStep::Player && before.volume != self.playback.volume {
            self.onboarding_geometry_changed();
        }

        let reached = self.beginner_step_target_reached(self.onboarding.step);
        if reached && !self.onboarding.target_reached {
            self.onboarding.target_reached = true;
            self.onboarding_geometry_changed();
        }
        if self.onboarding.step == BeginnerStep::Settings
            && self
                .settings
                .as_ref()
                .is_some_and(|state| state.tab != SettingsTab::General)
            && !self.onboarding.settings_tab_visited
        {
            self.onboarding.settings_tab_visited = true;
            self.onboarding_geometry_changed();
        }
        Vec::new()
    }

    pub(in crate::app) fn on_key_beginner(&mut self, key: KeyEvent) -> Option<Vec<Cmd>> {
        if !self.beginner_coach_visible() {
            return None;
        }
        let chord = Chord::from(key);
        if matches!(self.keymap.global_action(chord), Some(Action::Quit)) {
            return Some(self.quit_app());
        }
        let mini = self.bridges.ui_tier.get() == crate::ui::layout::UiTier::Mini;
        if self.onboarding.skip_confirmation {
            return Some(match key.code {
                KeyCode::Esc => self.activate_onboarding(OnboardingAction::CancelSkip),
                KeyCode::Left | KeyCode::Up => {
                    self.onboarding.move_selection(false);
                    self.dirty = true;
                    Vec::new()
                }
                KeyCode::Right | KeyCode::Down => {
                    self.onboarding.move_selection(true);
                    self.dirty = true;
                    Vec::new()
                }
                KeyCode::Enter => {
                    let action = self.onboarding.selected_action();
                    self.activate_onboarding(action)
                }
                _ => Vec::new(),
            });
        }
        if mini {
            if key.code == KeyCode::F(6) {
                self.onboarding.guide_focused = true;
                self.onboarding.mini_guide_focused = true;
                self.onboarding.select_skip();
                self.dirty = true;
                return Some(Vec::new());
            }
            if self.onboarding.mini_guide_focused {
                return Some(match key.code {
                    KeyCode::Enter => self.activate_onboarding(OnboardingAction::Skip),
                    KeyCode::Esc => {
                        self.onboarding.guide_focused = false;
                        self.onboarding.mini_guide_focused = false;
                        self.dirty = true;
                        Vec::new()
                    }
                    _ => Vec::new(),
                });
            }
            return None;
        }

        if !self.onboarding.guide_focused {
            if key.code == KeyCode::F(6) {
                self.onboarding.guide_focused = true;
                self.onboarding.mini_guide_focused = false;
                self.onboarding.select_primary();
                self.dirty = true;
                return Some(Vec::new());
            }
            return None;
        }

        Some(match key.code {
            KeyCode::F(6) => {
                self.onboarding.guide_focused = false;
                self.onboarding.mini_guide_focused = false;
                self.dirty = true;
                Vec::new()
            }
            KeyCode::Esc if self.onboarding.skip_confirmation => {
                self.activate_onboarding(OnboardingAction::CancelSkip)
            }
            KeyCode::Esc => {
                self.onboarding.guide_focused = false;
                self.onboarding.mini_guide_focused = false;
                self.dirty = true;
                Vec::new()
            }
            KeyCode::Left | KeyCode::Up => {
                self.onboarding.move_selection(false);
                self.dirty = true;
                Vec::new()
            }
            KeyCode::Right | KeyCode::Down => {
                self.onboarding.move_selection(true);
                self.dirty = true;
                Vec::new()
            }
            KeyCode::Enter => self.activate_onboarding(self.onboarding.selected_action()),
            _ => Vec::new(),
        })
    }

    pub(in crate::app) fn activate_onboarding(&mut self, action: OnboardingAction) -> Vec<Cmd> {
        if !self.onboarding.visible() {
            return Vec::new();
        }
        match action {
            OnboardingAction::Noop => Vec::new(),
            OnboardingAction::Primary => self.activate_beginner_primary(),
            OnboardingAction::Back => self.back_beginner_step(),
            OnboardingAction::Skip => {
                self.onboarding.guide_focused_before_skip = self.onboarding.guide_focused;
                self.onboarding.mini_guide_focused_before_skip = self.onboarding.mini_guide_focused;
                self.onboarding.skip_confirmation = true;
                self.onboarding.guide_focused = true;
                self.onboarding.mini_guide_focused = true;
                self.onboarding.selected = 0;
                self.onboarding_geometry_changed();
                Vec::new()
            }
            OnboardingAction::CancelSkip => {
                self.onboarding.skip_confirmation = false;
                self.onboarding.guide_focused = self.onboarding.guide_focused_before_skip;
                self.onboarding.mini_guide_focused = self.onboarding.mini_guide_focused_before_skip;
                self.onboarding.select_primary();
                self.onboarding_geometry_changed();
                Vec::new()
            }
            OnboardingAction::ConfirmSkip => self.skip_beginner_tutorial(),
        }
    }

    fn activate_beginner_primary(&mut self) -> Vec<Cmd> {
        match self.onboarding.step {
            BeginnerStep::Welcome => self.advance_beginner_step(),
            BeginnerStep::NavigationHelp => {
                self.overlays.help_visible = true;
                self.bridges.help_scroll.reset();
                self.dirty = true;
                Vec::new()
            }
            BeginnerStep::Search if self.onboarding.target_reached => self.advance_beginner_step(),
            BeginnerStep::Search => self.navigate_to(Mode::Search),
            BeginnerStep::Player if self.onboarding.target_reached => self.advance_beginner_step(),
            BeginnerStep::Player => self.navigate_to(Mode::Player),
            BeginnerStep::Library if self.onboarding.target_reached => self.advance_beginner_step(),
            BeginnerStep::Library => self.navigate_to(Mode::Library),
            BeginnerStep::DjGem if self.onboarding.target_reached => self.advance_beginner_step(),
            BeginnerStep::DjGem => self.navigate_to(Mode::Ai),
            BeginnerStep::Settings => self.activate_beginner_settings_primary(),
            BeginnerStep::Finish => self.activate_beginner_finish_primary(),
        }
    }

    fn activate_beginner_settings_primary(&mut self) -> Vec<Cmd> {
        if self.mode != Mode::Settings {
            return self.navigate_to(Mode::Settings);
        }
        let Some(tab) = self.settings.as_ref().map(|state| state.tab) else {
            return Vec::new();
        };
        if !self.onboarding.settings_tab_visited {
            self.settings_select_tab(SettingsTab::Playback.index());
            return Vec::new();
        }
        if tab != SettingsTab::General {
            self.settings_select_tab(SettingsTab::General.index());
            return Vec::new();
        }
        let cmds = self.advance_beginner_step();
        self.focus_beginner_mode_setting();
        cmds
    }

    fn activate_beginner_finish_primary(&mut self) -> Vec<Cmd> {
        if self.mode != Mode::Settings {
            return self.navigate_to(Mode::Settings);
        }
        let ready = self.settings.as_ref().is_some_and(|state| {
            state.tab == SettingsTab::General && state.current_field() == Some(Field::BeginnerMode)
        });
        if !ready {
            self.focus_beginner_mode_setting();
            return Vec::new();
        }
        if self
            .settings
            .as_ref()
            .is_some_and(|state| !state.draft.beginner_mode)
        {
            return self.close_settings();
        }
        self.settings_change(1)
    }

    fn focus_beginner_mode_setting(&mut self) {
        let Some(state) = self.settings.as_mut() else {
            return;
        };
        state.tab = SettingsTab::General;
        state.row = SettingsTab::General
            .fields()
            .iter()
            .position(|field| *field == Field::BeginnerMode)
            .unwrap_or(0);
        state.editing_text = false;
        state.capturing = None;
        self.bridges.reset_settings_scroll();
        self.onboarding.target_reached = true;
        self.onboarding_geometry_changed();
    }

    fn advance_beginner_step(&mut self) -> Vec<Cmd> {
        let Some(next) = self.onboarding.step.next() else {
            return Vec::new();
        };
        self.set_beginner_step(next);
        vec![self.persist_beginner_config()]
    }

    fn back_beginner_step(&mut self) -> Vec<Cmd> {
        let Some(previous) = self.onboarding.step.previous() else {
            self.onboarding.select_primary();
            self.set_status_info(t!("This is the first step", "첫 번째 단계입니다"));
            return Vec::new();
        };
        if self.onboarding.step == BeginnerStep::Finish
            && let Some(settings) = self.settings.as_mut()
        {
            settings.draft.beginner_mode = true;
            settings.draft.restart_beginner_tutorial = false;
        }
        self.set_beginner_step(previous);
        vec![self.persist_beginner_config()]
    }

    fn set_beginner_step(&mut self, step: BeginnerStep) {
        self.config.beginner_tutorial.content_version = BEGINNER_TUTORIAL_VERSION;
        self.config.beginner_tutorial.next_step = step.id().to_owned();
        self.onboarding.step = step;
        self.onboarding.target_reached = self.beginner_step_target_reached(step);
        self.onboarding.settings_tab_visited = false;
        self.onboarding.skip_confirmation = false;
        self.onboarding.guide_focused = step == BeginnerStep::Welcome;
        self.onboarding.mini_guide_focused = false;
        self.onboarding.guide_focused_before_skip = false;
        self.onboarding.mini_guide_focused_before_skip = false;
        self.onboarding.select_primary();
        self.onboarding_geometry_changed();
    }

    fn beginner_step_target_reached(&self, step: BeginnerStep) -> bool {
        match step {
            BeginnerStep::Welcome => true,
            BeginnerStep::NavigationHelp => false,
            BeginnerStep::Search => self.mode == Mode::Search,
            BeginnerStep::Player => self.mode == Mode::Player,
            BeginnerStep::Library => self.mode == Mode::Library,
            BeginnerStep::DjGem => self.mode == Mode::Ai,
            BeginnerStep::Settings | BeginnerStep::Finish => self.mode == Mode::Settings,
        }
    }

    fn skip_beginner_tutorial(&mut self) -> Vec<Cmd> {
        self.config.beginner_mode = false;
        self.config.beginner_tutorial = BeginnerTutorialProgress::welcome();
        if let Some(settings) = self.settings.as_mut() {
            settings.draft.beginner_mode = false;
            settings.draft.restart_beginner_tutorial = false;
        }
        self.onboarding = OnboardingState::default();
        self.set_status_info(t!(
            "Beginner Mode is off · turn it on in Settings to see the tour again",
            "비기너 모드를 껐어요 · 설정에서 다시 켜면 튜토리얼을 볼 수 있어요"
        ));
        self.request_native_image_clear();
        vec![self.persist_beginner_config()]
    }

    pub(in crate::app) fn apply_beginner_mode_settings_transition(
        &mut self,
        old_enabled: bool,
        restart_requested: bool,
    ) {
        let tutorial_was_active = self.onboarding.active();
        if !self.config.beginner_mode {
            if old_enabled || tutorial_was_active {
                // A newer build's suppressed cursor is opaque to this binary. Turning the
                // display mode off must not downgrade that progress; only a tour this build
                // actually ran owns the Welcome reset.
                if tutorial_was_active
                    || self.config.beginner_tutorial.content_version <= BEGINNER_TUTORIAL_VERSION
                {
                    self.config.beginner_tutorial = BeginnerTutorialProgress::welcome();
                }
                self.onboarding = OnboardingState::default();
                self.set_status_info(t!(
                    "Beginner Mode is off · onboarding complete",
                    "비기너 모드를 껐어요 · 온보딩을 마쳤습니다"
                ));
                self.request_native_image_clear();
            }
        } else if !old_enabled || restart_requested {
            self.config.beginner_tutorial = BeginnerTutorialProgress::welcome();
            self.onboarding = OnboardingState::default();
            self.set_status_info(t!(
                "Beginner Mode is on · the tour starts next launch",
                "비기너 모드를 켰어요 · 다음 실행 때 튜토리얼이 시작됩니다"
            ));
        }
    }

    fn persist_beginner_config(&self) -> Cmd {
        Cmd::Persist(PersistCmd::Config(Box::new(self.config.clone())))
    }

    fn onboarding_geometry_changed(&mut self) {
        if self.native_art_active() {
            self.request_native_image_clear();
        }
        self.dirty = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn beginner_app(step: BeginnerStep) -> App {
        let mut app = App::new(50);
        app.config.beginner_mode = true;
        app.config.beginner_tutorial.next_step = step.id().to_owned();
        app.prepare_beginner_onboarding(true);
        app
    }

    #[test]
    fn fresh_enabled_profile_starts_and_legacy_safe_default_does_not() {
        let mut disabled = App::new(50);
        disabled.prepare_beginner_onboarding(true);
        assert!(!disabled.onboarding.visible());

        let enabled = beginner_app(BeginnerStep::Welcome);
        assert!(enabled.onboarding.visible());
        assert_eq!(enabled.onboarding.step(), BeginnerStep::Welcome);
        assert!(enabled.onboarding.guide_focused());
    }

    #[test]
    fn ordered_steps_keep_numbers_and_forward_back_navigation_in_lockstep() {
        let expected = [
            BeginnerStep::Welcome,
            BeginnerStep::NavigationHelp,
            BeginnerStep::Player,
            BeginnerStep::Search,
            BeginnerStep::Library,
            BeginnerStep::DjGem,
            BeginnerStep::Settings,
            BeginnerStep::Finish,
        ];
        assert_eq!(BeginnerStep::ORDER, expected);
        assert_eq!(BeginnerStep::COUNT, expected.len());

        for (index, step) in expected.iter().copied().enumerate() {
            assert_eq!(step.number(), index + 1);
            assert_eq!(step.previous(), index.checked_sub(1).map(|i| expected[i]));
            assert_eq!(step.next(), expected.get(index + 1).copied());
        }
    }

    #[test]
    fn steps_persist_once_and_resume_the_last_incomplete_step() {
        let mut app = beginner_app(BeginnerStep::Welcome);
        let cmds = app.activate_onboarding(OnboardingAction::Primary);
        assert_eq!(app.config.beginner_tutorial.next_step, "navigation_help");
        assert!(matches!(
            cmds.as_slice(),
            [Cmd::Persist(PersistCmd::Config(_))]
        ));

        let mut resumed = App::new(50);
        resumed.config = app.config.clone();
        resumed.prepare_beginner_onboarding(true);
        assert_eq!(resumed.onboarding.step(), BeginnerStep::NavigationHelp);
    }

    #[test]
    fn help_open_advances_navigation_step_only_once() {
        let mut app = beginner_app(BeginnerStep::NavigationHelp);
        let before = app.onboarding_observation();
        app.overlays.help_visible = true;
        let cmds = app.observe_beginner_tutorial(before);
        assert_eq!(app.onboarding.step(), BeginnerStep::Player);
        assert!(app.onboarding.target_reached());
        assert_eq!(cmds.len(), 1);
        assert!(
            app.observe_beginner_tutorial(app.onboarding_observation())
                .is_empty()
        );
    }

    #[test]
    fn fresh_player_step_continues_to_search_without_leaving_player_first() {
        let mut app = beginner_app(BeginnerStep::Player);
        assert_eq!(app.mode, Mode::Player);
        assert!(app.onboarding.target_reached());

        let cmds = app.activate_onboarding(OnboardingAction::Primary);

        assert_eq!(app.mode, Mode::Player);
        assert_eq!(app.onboarding.step(), BeginnerStep::Search);
        assert!(!app.onboarding.target_reached());
        assert_eq!(app.config.beginner_tutorial.next_step, "search");
        assert!(matches!(
            cmds.as_slice(),
            [Cmd::Persist(PersistCmd::Config(_))]
        ));
    }

    #[test]
    fn skip_disables_mode_resets_progress_and_mirrors_settings() {
        let mut app = beginner_app(BeginnerStep::Player);
        app.open_settings();
        let cmds = app.activate_onboarding(OnboardingAction::ConfirmSkip);
        assert!(!app.config.beginner_mode);
        assert_eq!(
            app.config.beginner_tutorial,
            BeginnerTutorialProgress::welcome()
        );
        assert!(!app.settings.as_ref().unwrap().draft.beginner_mode);
        assert!(!app.onboarding.visible());
        assert!(matches!(
            cmds.as_slice(),
            [Cmd::Persist(PersistCmd::Config(_))]
        ));
    }

    #[test]
    fn old_progress_normalizes_but_future_progress_is_preserved_and_suppressed() {
        let mut old = App::new(50);
        old.config.beginner_mode = true;
        old.config.beginner_tutorial.content_version = 1;
        old.config.beginner_tutorial.next_step = "library".to_owned();
        old.prepare_beginner_onboarding(true);
        assert!(old.onboarding.visible());
        assert_eq!(
            old.config.beginner_tutorial,
            BeginnerTutorialProgress::welcome()
        );
        assert!(old.take_beginner_startup_persist().is_some());

        let mut future = App::new(50);
        future.config.beginner_mode = true;
        future.config.beginner_tutorial.content_version = BEGINNER_TUTORIAL_VERSION + 1;
        future.config.beginner_tutorial.next_step = "future_step".to_owned();
        future.prepare_beginner_onboarding(true);
        assert!(!future.onboarding.visible());
        assert_eq!(future.config.beginner_tutorial.next_step, "future_step");
        assert!(future.take_beginner_startup_persist().is_none());
    }

    #[test]
    fn readonly_instance_never_arms_or_normalizes() {
        let mut app = App::new(50);
        app.config.beginner_mode = true;
        app.config.beginner_tutorial.content_version = 0;
        app.prepare_beginner_onboarding(false);
        assert!(!app.onboarding.visible());
        assert_eq!(app.config.beginner_tutorial.content_version, 0);
    }

    #[test]
    fn tool_setup_defers_the_tour_until_the_setup_card_closes() {
        let mut app = App::new(50);
        app.config.beginner_mode = true;
        app.show_tool_setup(ToolSetupContext::Startup, vec!["mpv"]);
        app.prepare_beginner_onboarding(true);
        assert!(!app.onboarding.visible());

        app.activate_tool_setup(MouseTarget::ToolSetupLater);
        assert!(app.onboarding.visible());
    }

    #[test]
    fn mini_tier_pauses_observation_without_losing_the_step() {
        let mut app = beginner_app(BeginnerStep::Search);
        app.bridges.ui_tier.set(crate::ui::layout::UiTier::Mini);
        app.mode = Mode::Search;
        let before = app.onboarding_observation();
        assert!(app.observe_beginner_tutorial(before).is_empty());
        assert_eq!(app.onboarding.step(), BeginnerStep::Search);
        assert!(!app.onboarding.target_reached());

        app.bridges.ui_tier.set(crate::ui::layout::UiTier::Full);
        app.observe_beginner_tutorial(before);
        assert!(app.onboarding.target_reached());
    }

    #[test]
    fn skip_confirmation_keeps_keyboard_focus_until_cancel_or_confirm() {
        let mut app = beginner_app(BeginnerStep::Welcome);
        app.activate_onboarding(OnboardingAction::Skip);
        assert!(app.onboarding.skip_confirmation());

        app.on_key_beginner(KeyEvent::new(KeyCode::F(6), KeyModifiers::NONE));
        assert!(app.onboarding.guide_focused());
        app.on_key_beginner(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        let cmds = app
            .on_key_beginner(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        assert!(!app.config.beginner_mode);
        assert!(matches!(
            cmds.as_slice(),
            [Cmd::Persist(PersistCmd::Config(_))]
        ));
    }

    #[test]
    fn mini_welcome_keeps_app_keys_live_until_f6_explicitly_focuses_skip() {
        let mut app = beginner_app(BeginnerStep::Welcome);
        app.bridges.ui_tier.set(crate::ui::layout::UiTier::Mini);
        let ordinary = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
        assert!(app.on_key_beginner(ordinary).is_none());

        assert!(
            app.on_key_beginner(KeyEvent::new(KeyCode::F(6), KeyModifiers::NONE))
                .is_some()
        );
        assert!(app.onboarding.guide_focused_for(true));
        assert!(app.on_key_beginner(ordinary).is_some());
        app.on_key_beginner(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!app.onboarding.guide_focused_for(true));
        assert!(app.on_key_beginner(ordinary).is_none());
    }

    #[test]
    fn cancelling_mouse_skip_restores_the_previous_surface_focus() {
        let mut app = beginner_app(BeginnerStep::Search);
        assert!(!app.onboarding.guide_focused());
        app.activate_onboarding(OnboardingAction::Skip);
        app.activate_onboarding(OnboardingAction::CancelSkip);
        assert!(!app.onboarding.guide_focused());

        app.onboarding.guide_focused = true;
        app.activate_onboarding(OnboardingAction::Skip);
        app.activate_onboarding(OnboardingAction::CancelSkip);
        assert!(app.onboarding.guide_focused());
    }

    #[test]
    fn finish_primary_can_toggle_the_setting_before_saving() {
        let mut app = beginner_app(BeginnerStep::Finish);
        app.open_settings();
        app.focus_beginner_mode_setting();
        assert!(app.settings.as_ref().unwrap().draft.beginner_mode);
        assert!(
            app.activate_onboarding(OnboardingAction::Primary)
                .is_empty()
        );
        assert!(!app.settings.as_ref().unwrap().draft.beginner_mode);
        assert!(app.onboarding.active());
    }
}
