//! Admission-atomic dedicated Radio/Local mode switches.
//!
//! Switching modes swaps queues and may replace the active mpv file. The visible mode, theme,
//! queue stashes, and transient UI therefore belong to the same accepted transaction as
//! recorder teardown, `Stop`, and the optional replacement `Load`.

use super::*;

#[derive(Clone, Copy)]
pub(in crate::app) enum DedicatedModeSwitch {
    Radio(RadioModeConfirm),
    Local(LocalModeConfirm),
}

#[derive(Clone)]
pub(in crate::app) struct ModeSwitchPlan {
    kind: DedicatedModeSwitch,
    expected_radio_mode: bool,
    expected_local_mode: bool,
    expected_radio_confirm: Option<RadioModeConfirm>,
    expected_local_confirm: Option<LocalModeConfirm>,
    expected_local_confirm_token: Option<u64>,
    local_intent_token: Option<u64>,
    local_import_search_confirmation_token: Option<u64>,
    expected_theme: Vec<u8>,
    expected_target_theme: Option<Vec<u8>>,
    expected_target_queue: Vec<u8>,
    expected_video_generation: u64,
    expected_video_pause_owned: bool,
    outgoing_queue: QueueSnapshot,
}

impl App {
    pub(in crate::app) fn prepare_radio_mode_transition(
        &mut self,
        confirm: RadioModeConfirm,
    ) -> Vec<Cmd> {
        if matches!(confirm, RadioModeConfirm::Enter) == self.radio_dedicated_mode {
            self.radio_mode.pending_radio_mode_confirm = None;
            return Vec::new();
        }
        let target_queue = match confirm {
            RadioModeConfirm::Enter => self.radio_mode.radio_mode_queue.clone(),
            RadioModeConfirm::Exit => self.radio_mode.normal_mode_queue.clone(),
        };
        let target_theme = match confirm {
            RadioModeConfirm::Enter => self.radio_mode.radio_mode_theme.as_ref(),
            RadioModeConfirm::Exit => self.radio_mode.normal_mode_theme.as_ref(),
        };
        let plan = ModeSwitchPlan {
            kind: DedicatedModeSwitch::Radio(confirm),
            expected_radio_mode: self.radio_dedicated_mode,
            expected_local_mode: self.local_dedicated_mode,
            expected_radio_confirm: self.radio_mode.pending_radio_mode_confirm,
            expected_local_confirm: self.local_mode.pending_confirm,
            expected_local_confirm_token: self.local_mode.pending_confirm_token,
            local_intent_token: None,
            local_import_search_confirmation_token: None,
            expected_theme: theme_projection(Some(&self.theme)),
            expected_target_theme: Some(theme_projection(target_theme)),
            expected_target_queue: queue_projection(&target_queue),
            expected_video_generation: self.video.generation,
            expected_video_pause_owned: self.video.paused_audio,
            outgoing_queue: self.queue.snapshot(),
        };
        let mutation = self
            .queue
            .prepare_snapshot_restore(target_queue.unwrap_or_default());
        self.load_mode_switch_queue(mutation, plan)
    }

    pub(in crate::app) fn prepare_local_mode_transition(
        &mut self,
        confirm: LocalModeConfirm,
    ) -> Vec<Cmd> {
        if matches!(confirm, LocalModeConfirm::Enter) == self.local_dedicated_mode {
            self.local_mode.pending_confirm = None;
            self.local_mode.pending_confirm_token = None;
            self.local_mode.pending_intent_token = None;
            self.local_mode.pending_import_search = None;
            return Vec::new();
        }
        // The first Enter owns this confirmation until its player batch settles. Repeated Enter
        // must not create a second intent whose Busy rejection could clear the first intent's
        // single-use online-search continuation.
        if self.local_mode.pending_intent_token.is_some() {
            return Vec::new();
        }
        let intent_token = self.allocate_local_transition_token();
        self.local_mode.pending_intent_token = Some(intent_token);
        let target_queue = match confirm {
            LocalModeConfirm::Enter => self.local_mode.local_mode_queue.clone(),
            LocalModeConfirm::Exit => self.local_mode.normal_mode_queue.clone(),
        };
        let kind = DedicatedModeSwitch::Local(confirm);
        let import_search_confirmation_token = self
            .local_mode
            .pending_import_search
            .as_ref()
            .map(|pending| pending.confirmation_token)
            .filter(|token| Some(*token) == self.local_mode.pending_confirm_token);
        let plan = ModeSwitchPlan {
            kind,
            expected_radio_mode: self.radio_dedicated_mode,
            expected_local_mode: self.local_dedicated_mode,
            expected_radio_confirm: self.radio_mode.pending_radio_mode_confirm,
            expected_local_confirm: self.local_mode.pending_confirm,
            expected_local_confirm_token: self.local_mode.pending_confirm_token,
            local_intent_token: Some(intent_token),
            local_import_search_confirmation_token: import_search_confirmation_token,
            expected_theme: theme_projection(Some(&self.theme)),
            expected_target_theme: self.mode_switch_target_theme_projection(kind),
            expected_target_queue: queue_projection(&target_queue),
            expected_video_generation: self.video.generation,
            expected_video_pause_owned: self.video.paused_audio,
            outgoing_queue: self.queue.snapshot(),
        };
        let mutation = self
            .queue
            .prepare_snapshot_restore(target_queue.unwrap_or_default());
        self.load_mode_switch_queue(mutation, plan)
    }

    pub(in crate::app) fn validate_mode_switch(&self, plan: &ModeSwitchPlan) {
        assert_eq!(
            self.radio_dedicated_mode, plan.expected_radio_mode,
            "radio mode changed before mode-switch commit"
        );
        assert_eq!(
            self.local_dedicated_mode, plan.expected_local_mode,
            "local mode changed before mode-switch commit"
        );
        assert_eq!(
            self.radio_mode.pending_radio_mode_confirm, plan.expected_radio_confirm,
            "radio confirmation changed before mode-switch commit"
        );
        assert_eq!(
            self.local_mode.pending_confirm, plan.expected_local_confirm,
            "local confirmation changed before mode-switch commit"
        );
        assert_eq!(
            self.local_mode.pending_confirm_token, plan.expected_local_confirm_token,
            "local confirmation token changed before mode-switch commit"
        );
        if let Some(token) = plan.local_intent_token {
            assert_eq!(
                self.local_mode.pending_intent_token,
                Some(token),
                "local intent token changed before mode-switch commit"
            );
        }
        assert_eq!(
            self.local_mode
                .pending_import_search
                .as_ref()
                .map(|pending| pending.confirmation_token),
            plan.local_import_search_confirmation_token,
            "local import-search continuation changed before mode-switch commit"
        );
        assert_eq!(
            self.video.generation, plan.expected_video_generation,
            "video generation changed before mode-switch commit"
        );
        assert_eq!(
            self.video.paused_audio, plan.expected_video_pause_owned,
            "video pause ownership changed before mode-switch commit"
        );
        assert_eq!(
            theme_projection(Some(&self.theme)),
            plan.expected_theme,
            "theme changed before mode-switch commit"
        );

        let (target_queue, target_theme) = match plan.kind {
            DedicatedModeSwitch::Radio(RadioModeConfirm::Enter) => (
                &self.radio_mode.radio_mode_queue,
                Some(self.radio_mode.radio_mode_theme.as_ref()),
            ),
            DedicatedModeSwitch::Radio(RadioModeConfirm::Exit) => (
                &self.radio_mode.normal_mode_queue,
                Some(self.radio_mode.normal_mode_theme.as_ref()),
            ),
            DedicatedModeSwitch::Local(LocalModeConfirm::Enter) => {
                (&self.local_mode.local_mode_queue, None)
            }
            DedicatedModeSwitch::Local(LocalModeConfirm::Exit) => {
                (&self.local_mode.normal_mode_queue, None)
            }
        };
        assert_eq!(
            queue_projection(target_queue),
            plan.expected_target_queue,
            "cached queue changed before mode-switch commit"
        );
        let actual_target_theme = match plan.kind {
            DedicatedModeSwitch::Radio(_) => Some(theme_projection(target_theme.flatten())),
            DedicatedModeSwitch::Local(_) => self.mode_switch_target_theme_projection(plan.kind),
        };
        assert_eq!(
            actual_target_theme, plan.expected_target_theme,
            "cached theme changed before mode-switch commit"
        );
    }

    pub(in crate::app) fn mode_switch_is_current(&self, plan: &ModeSwitchPlan) -> bool {
        if self.radio_dedicated_mode != plan.expected_radio_mode
            || self.local_dedicated_mode != plan.expected_local_mode
            || self.radio_mode.pending_radio_mode_confirm != plan.expected_radio_confirm
            || self.local_mode.pending_confirm != plan.expected_local_confirm
            || self.local_mode.pending_confirm_token != plan.expected_local_confirm_token
            || plan
                .local_intent_token
                .is_some_and(|token| self.local_mode.pending_intent_token != Some(token))
            || self
                .local_mode
                .pending_import_search
                .as_ref()
                .map(|pending| pending.confirmation_token)
                != plan.local_import_search_confirmation_token
            || self.video.generation != plan.expected_video_generation
            || self.video.paused_audio != plan.expected_video_pause_owned
            || theme_projection(Some(&self.theme)) != plan.expected_theme
        {
            return false;
        }

        let (target_queue, target_theme) = match plan.kind {
            DedicatedModeSwitch::Radio(RadioModeConfirm::Enter) => (
                &self.radio_mode.radio_mode_queue,
                Some(self.radio_mode.radio_mode_theme.as_ref()),
            ),
            DedicatedModeSwitch::Radio(RadioModeConfirm::Exit) => (
                &self.radio_mode.normal_mode_queue,
                Some(self.radio_mode.normal_mode_theme.as_ref()),
            ),
            DedicatedModeSwitch::Local(LocalModeConfirm::Enter) => {
                (&self.local_mode.local_mode_queue, None)
            }
            DedicatedModeSwitch::Local(LocalModeConfirm::Exit) => {
                (&self.local_mode.normal_mode_queue, None)
            }
        };
        let actual_target_theme = match plan.kind {
            DedicatedModeSwitch::Radio(_) => Some(theme_projection(target_theme.flatten())),
            DedicatedModeSwitch::Local(_) => self.mode_switch_target_theme_projection(plan.kind),
        };
        queue_projection(target_queue) == plan.expected_target_queue
            && actual_target_theme == plan.expected_target_theme
    }

    /// Apply projections which affect track-load follow-ups before committing the prepared load.
    /// In particular, `streaming_active()` must already observe the target dedicated mode when
    /// the track commit decides whether to refill the queue.
    pub(in crate::app) fn commit_mode_switch_before_track(&mut self, plan: &ModeSwitchPlan) {
        // The accepted player batch already carries the absolute unpause needed when this
        // overlay owned audio. Retire the old process and its ownership in the same reducer
        // commit as the queue/mode swap; a rejected batch leaves all three untouched.
        self.close_video();
        self.video.paused_audio = false;

        match plan.kind {
            DedicatedModeSwitch::Radio(RadioModeConfirm::Enter) => {
                self.radio_mode.normal_mode_theme = Some(self.theme.clone());
                self.radio_mode.normal_mode_queue = Some(plan.outgoing_queue.clone());
                self.activate_radio_dedicated_mode_ui();
                self.radio_mode.radio_mode_queue = None;
                self.radio_mode.pending_radio_mode_confirm = None;
            }
            DedicatedModeSwitch::Radio(RadioModeConfirm::Exit) => {
                self.radio_mode.radio_mode_theme = Some(self.theme.clone());
                self.radio_mode.radio_mode_queue = Some(plan.outgoing_queue.clone());
                self.radio_dedicated_mode = false;
                self.theme = self
                    .radio_mode
                    .normal_mode_theme
                    .take()
                    .unwrap_or_else(|| self.config.effective_theme());
                if !self.library_tab_available(self.library_ui.tab) {
                    self.library_ui.tab = LibraryTab::All;
                    self.clear_library_filter();
                }
                let search = self.search_config_for_mode();
                self.search.source =
                    search.normalized_source(self.config.effective_search().source);
                self.search.searching = false;
                self.search.results.clear();
                self.search.selected = 0;
                self.dropdowns.search_source_open = false;
                self.radio_mode.normal_mode_queue = None;
                self.radio_mode.pending_radio_mode_confirm = None;
            }
            DedicatedModeSwitch::Local(LocalModeConfirm::Enter) => {
                self.local_mode.normal_mode_theme = Some(self.theme.clone());
                self.local_mode.normal_mode_queue = Some(plan.outgoing_queue.clone());
                self.activate_local_dedicated_mode_ui();
                self.local_mode.local_mode_queue = None;
                self.local_mode.pending_confirm = None;
                self.local_mode.pending_confirm_token = None;
                self.local_mode.pending_intent_token = None;
            }
            DedicatedModeSwitch::Local(LocalModeConfirm::Exit) => {
                self.local_mode.local_mode_theme = Some(self.theme.clone());
                self.local_mode.local_mode_queue = Some(plan.outgoing_queue.clone());
                self.local_dedicated_mode = false;
                self.theme = self
                    .local_mode
                    .normal_mode_theme
                    .take()
                    .unwrap_or_else(|| self.config.effective_theme());
                self.local_mode.pending_confirm = None;
                self.local_mode.pending_confirm_token = None;
                self.local_mode.pending_intent_token = None;
                self.local_mode.find.clear_transient_for_exit();
                self.bridges.library_scroll.reset();
                self.bridges.local_find_scroll.reset();
                self.local_mode.normal_mode_queue = None;
            }
        }

        self.queue_popup.open = false;
        self.queue_popup.cursor = 0;
        self.queue_popup.anchor = 0;
        self.search_filter.close();
        self.cancel_pending_streaming_recommendation();
        self.ai.thinking = false;
        self.art.force_clear_next_frame = true;
        self.dirty = true;
    }

    pub(in crate::app) fn commit_mode_switch_after_track(
        &mut self,
        plan: ModeSwitchPlan,
    ) -> Vec<Cmd> {
        let mut effects = match plan.kind {
            DedicatedModeSwitch::Local(LocalModeConfirm::Enter) => self.ensure_local_index_ready(),
            _ => Vec::new(),
        };
        self.status.kind = StatusKind::Info;
        self.status.text = match plan.kind {
            DedicatedModeSwitch::Radio(RadioModeConfirm::Enter) => {
                t!("Radio mode enabled", "라디오 모드 켜짐", "ラジオモード有効")
            }
            DedicatedModeSwitch::Radio(RadioModeConfirm::Exit) => {
                t!(
                    "Radio mode disabled",
                    "라디오 모드 꺼짐",
                    "ラジオモード無効"
                )
            }
            DedicatedModeSwitch::Local(LocalModeConfirm::Enter) => {
                t!(
                    "Local Player mode enabled",
                    "로컬 플레이어 모드 켜짐",
                    "ローカルプレイヤーモード有効"
                )
            }
            DedicatedModeSwitch::Local(LocalModeConfirm::Exit) => {
                t!(
                    "Local Player mode disabled",
                    "로컬 플레이어 모드 꺼짐",
                    "ローカルプレイヤーモード無効"
                )
            }
        }
        .to_owned();
        if matches!(
            plan.kind,
            DedicatedModeSwitch::Local(LocalModeConfirm::Exit)
        ) {
            effects.extend(self.complete_local_import_search_continuation(
                plan.local_import_search_confirmation_token,
            ));
        }
        self.dirty = true;
        effects
    }

    fn mode_switch_target_theme_projection(&self, kind: DedicatedModeSwitch) -> Option<Vec<u8>> {
        let theme = match kind {
            DedicatedModeSwitch::Radio(RadioModeConfirm::Enter) => {
                return Some(theme_projection(self.radio_mode.radio_mode_theme.as_ref()));
            }
            DedicatedModeSwitch::Radio(RadioModeConfirm::Exit) => {
                return Some(theme_projection(self.radio_mode.normal_mode_theme.as_ref()));
            }
            DedicatedModeSwitch::Local(LocalModeConfirm::Enter) => self
                .local_mode
                .local_mode_theme
                .clone()
                .unwrap_or_else(|| self.config.effective_local_theme()),
            DedicatedModeSwitch::Local(LocalModeConfirm::Exit) => self
                .local_mode
                .normal_mode_theme
                .clone()
                .unwrap_or_else(|| self.config.effective_theme()),
        };
        Some(theme_projection(Some(&theme)))
    }
}

impl ModeSwitchPlan {
    pub(in crate::app) const fn releases_video_pause(&self) -> bool {
        self.expected_video_pause_owned
    }

    pub(in crate::app) const fn local_intent_token(&self) -> Option<u64> {
        self.local_intent_token
    }

    pub(in crate::app) const fn local_import_search_confirmation_token(&self) -> Option<u64> {
        self.local_import_search_confirmation_token
    }
}

fn queue_projection(snapshot: &Option<QueueSnapshot>) -> Vec<u8> {
    serde_json::to_vec(snapshot).expect("mode-switch queue snapshot must serialize")
}

fn theme_projection(theme: Option<&ThemeConfig>) -> Vec<u8> {
    serde_json::to_vec(&theme).expect("mode-switch theme must serialize")
}
