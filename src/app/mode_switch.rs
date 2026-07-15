//! Dedicated-mode switching and playback-mode persistence helpers.

use super::*;

impl App {
    pub fn library_tabs(&self) -> &'static [LibraryTab] {
        if self.radio_dedicated_mode {
            &LibraryTab::RADIO_MODE
        } else {
            &LibraryTab::NORMAL
        }
    }

    pub fn library_tab_available(&self, tab: LibraryTab) -> bool {
        self.library_tabs().contains(&tab)
    }

    pub(in crate::app) fn next_library_tab(
        &self,
        current: LibraryTab,
        forward: bool,
    ) -> LibraryTab {
        let tabs = self.library_tabs();
        let i = tabs.iter().position(|&tab| tab == current).unwrap_or(0);
        let n = tabs.len();
        if n == 0 {
            return LibraryTab::All;
        }
        let j = if forward {
            (i + 1) % n
        } else {
            (i + n - 1) % n
        };
        tabs[j]
    }

    pub(in crate::app) fn ensure_radio_mode_constraints(&mut self) {
        if !self.library_tab_available(self.library_ui.tab) {
            self.library_ui.tab = self.library_tabs()[0];
            self.reset_playlist_ui_state();
            self.clear_library_filter();
        }
        let search = self.search_config_for_mode();
        self.search.source = search.normalized_source(self.search.source);
        if self.radio_dedicated_mode {
            self.dropdowns.search_source_open = false;
            self.overlays.why_ai_visible = false;
        }
    }

    pub(in crate::app) fn request_radio_mode_switch(&mut self) -> Vec<Cmd> {
        if !self.radio_dedicated_mode && self.local_dedicated_mode {
            self.status.kind = StatusKind::Error;
            self.status.text = t!(
                "Leave Local Player before entering Radio mode.",
                "라디오 모드로 들어가기 전에 로컬 플레이어를 먼저 나가세요."
            )
            .to_owned();
            self.dirty = true;
            return Vec::new();
        }
        self.radio_mode.pending_radio_mode_confirm = Some(if self.radio_dedicated_mode {
            RadioModeConfirm::Exit
        } else {
            RadioModeConfirm::Enter
        });
        self.dropdowns.eq_open = false;
        self.dropdowns.streaming_open = false;
        self.dropdowns.search_source_open = false;
        self.queue_popup.open = false;
        self.search_filter.close();
        self.dirty = true;
        Vec::new()
    }

    pub(in crate::app) fn apply_radio_mode_confirm(
        &mut self,
        confirm: RadioModeConfirm,
    ) -> Vec<Cmd> {
        self.prepare_radio_mode_transition(confirm)
    }

    pub(in crate::app) fn activate_radio_dedicated_mode_ui(&mut self) {
        self.radio_dedicated_mode = true;
        self.theme = self
            .radio_mode
            .radio_mode_theme
            .clone()
            .unwrap_or_else(ThemeConfig::radio);
        self.search.source = SearchSource::RadioBrowser;
        self.search.searching = false;
        self.search.results.clear();
        self.search.selected = 0;
        self.collapse_search_selection();
        self.search.focus = SearchFocus::Input;
        self.search.input_cursor = TextCursor::at_end(&self.search.input);
        self.bridges.search_scroll.reset();
        self.library_ui.tab = LibraryTab::RadioFavorites;
        self.clear_library_filter();
        self.dropdowns.eq_open = false;
        self.dropdowns.streaming_open = false;
        self.dropdowns.search_source_open = false;
        self.ensure_radio_mode_constraints();
        self.dirty = true;
    }

    pub(in crate::app) fn sync_playback_modes_to_config(&mut self) {
        self.config.shuffle = Some(self.queue.shuffle);
        self.config.repeat = self.queue.repeat;
        self.config.autoplay_streaming = Some(self.autoplay_streaming);
    }

    pub(in crate::app) fn save_playback_modes_cmd(&mut self) -> Cmd {
        self.sync_playback_modes_to_config();
        Cmd::Persist(PersistCmd::Config(Box::new(self.config.clone())))
    }
}
