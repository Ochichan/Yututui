//! App construction and config application.

use super::*;

impl App {
    pub fn new(volume: i64) -> Self {
        Self {
            should_quit: false,
            dirty: true,
            mode: Mode::Player,
            authenticated: false,
            keymap: KeyMap::default(),
            mousemap: MouseMap::default(),
            theme: ThemeConfig::default(),
            radio_dedicated_mode: false,
            radio_mode: RadioMode::default(),
            local_dedicated_mode: false,
            local_mode: LocalMode::default(),
            overlays: Overlays::default(),
            transfer_running: false,
            personal_export: PersonalDataExportState::default(),
            playback: Playback {
                volume: volume.clamp(0, VOLUME_MAX),
                speed: 1.0,
                ..Default::default()
            },
            recorder: crate::recorder::RecorderState::default(),
            media_art: None,
            queue: Queue::default(),
            status: Status::default(),
            status_text_prev: String::new(),
            video: Video::default(),
            anim: Animation::default(),
            fx: FxState::new(volume.clamp(0, VOLUME_MAX)),
            audio: AudioEq::default(),
            audio_devices: AudioDeviceRuntime::default(),
            autoplay_streaming: false,
            dropdowns: Dropdowns::default(),
            queue_popup: QueuePopup::default(),
            search_filter: SearchFilterPopup::default(),
            config: Config::default(),
            onboarding: OnboardingState::default(),
            tool_setup: None,
            runtime_tool_checks: false,
            settings: None,
            ai: AiState {
                available: false,
                model: GeminiModel::default(),
                messages: Vec::new(),
                transcript_revision: 0,
                transcript_cache_token: Arc::new(()),
                input: String::new(),
                select_all: false,
                thinking: false,
                suggestions: Vec::new(),
                suggestions_selected: 0,
                focus: AiFocus::Input,
            },
            romanization: RomanizationRuntime::default(),
            streaming: StreamingRuntime::default(),
            consecutive_play_errors: 0,
            playlists: Playlists::default(),
            station: StationStore::default(),
            search: SearchState {
                input: String::new(),
                source: SearchSource::Youtube,
                select_all: false,
                focus: SearchFocus::Input,
                results: Vec::new(),
                selected: 0,
                anchor: 0,
                picked: BTreeSet::new(),
                kind: SearchKind::default(),
                searching: false,
                request_id: 0,
            },
            library: Library::default(),
            signals: Signals::default(),
            session: Session::default(),
            library_ui: LibraryView::default(),
            library_rows_cache: RefCell::new(None),
            all_count_cache: Cell::new(None),
            yid_scan_memo: RefCell::new(None),
            playlist_picker: None,
            interaction: Interaction::default(),
            lyrics: Lyrics::default(),
            art: ArtState::default(),
            downloads: Downloads::default(),
            download_store: DownloadStore::default(),
            prefetch: Prefetch::default(),
            heal: YtdlpHeal::default(),
            bridges: RenderBridges::default(),
            hits: HitMap::default(),
            radio_resync_at: None,
            focused: true,
            zoom: crate::zoom::ZoomHandle::default(),
        }
    }

    /// Push persisted playback/EQ settings into the app after construction. Kept separate
    /// from `new` (whose `volume`-only signature many tests rely on) so `main` can apply
    /// the full config without churning those call sites.
    pub fn apply_config(&mut self, cfg: &Config) {
        let animations_were_on = self.animations().master;
        self.audio.preset = cfg.eq_preset;
        self.audio.bands = cfg.effective_eq_bands();
        self.audio.normalize = cfg.effective_normalize();
        self.playback.speed = cfg.effective_speed();
        self.audio.seek_seconds = cfg.effective_seek_seconds();
        self.queue.set_shuffle(cfg.effective_shuffle());
        self.queue.repeat = cfg.effective_repeat();
        // Music-mode invariant (single-sourced with the daemon): streaming and repeat can't
        // both be on; a legacy/hand-edited config carrying both drops streaming, keeping the
        // more deliberate repeat. `self.queue.repeat` was just set from the same config above.
        self.autoplay_streaming = crate::playback_policy::streaming_enabled_with_repeat(
            cfg.effective_autoplay_streaming(),
            self.queue.repeat,
        );
        self.ai.available = cfg.effective_ai_key().is_some();
        self.ai.model = cfg.effective_gemini_model();
        self.keymap = KeyMap::from_config(cfg);
        self.mousemap = MouseMap::from_config(cfg);
        let normal_theme = cfg.effective_theme();
        // Seed the radio-mode theme stash from its persisted slot. Guarded so a config
        // without one never clobbers a theme picked live earlier in this session.
        if let Some(radio_theme) = cfg.effective_radio_theme() {
            self.radio_mode.radio_mode_theme = Some(radio_theme);
        }
        // Seed the Local-Deck stash from its persisted slot. A missing slot is a real default
        // (Local Launch), but do not overwrite a live in-process pick when a legacy config is
        // reapplied while the stash already exists.
        if cfg.local_theme.is_some() || self.local_mode.local_mode_theme.is_none() {
            self.local_mode.local_mode_theme = Some(cfg.effective_local_theme());
        }
        if self.radio_dedicated_mode {
            self.radio_mode.normal_mode_theme = Some(normal_theme);
            self.theme = self
                .radio_mode
                .radio_mode_theme
                .clone()
                .unwrap_or_else(ThemeConfig::radio);
        } else if self.local_dedicated_mode {
            self.local_mode.normal_mode_theme = Some(normal_theme);
            self.theme = self
                .local_mode
                .local_mode_theme
                .clone()
                .unwrap_or_else(ThemeConfig::local_launch);
        } else {
            self.theme = normal_theme;
        }
        let search =
            Self::search_config_for_radio_mode(cfg.effective_search(), self.radio_dedicated_mode);
        self.search.source = search.normalized_source(search.source);
        // Keep the process-wide UI language in sync with the applied config (this is the
        // central apply path, called at startup and after a settings save).
        crate::i18n::set_language(cfg.effective_language());
        // Same for the DJ Gem reply language (resolved: retro -> English, `Auto` -> the UI
        // language). The AI actor reads this global when building its system prompt.
        crate::i18n::set_dj_gem_language(cfg.effective_dj_gem_language());
        // Restore the persisted text zoom, but only on terminals with a working zoom
        // mechanism - a config written under kitty must not garble a later tmux session.
        // (`set` snaps to the mode's levels, so kitty's 150% reads as 200% on a
        // double-size-line terminal rather than getting lost.)
        if self.zoom.supported() {
            self.zoom.set(cfg.effective_text_zoom());
        }
        // Keep the full config so the settings screen can persist the whole file.
        self.config = cfg.clone();
        self.ensure_radio_mode_constraints();
        if animations_were_on && !self.animations().master {
            self.fx.cancel();
        }
    }

    pub fn search_config_for_mode(&self) -> SearchConfig {
        Self::search_config_for_radio_mode(
            self.config.effective_search(),
            self.radio_dedicated_mode,
        )
    }

    fn search_config_for_radio_mode(
        mut search: SearchConfig,
        streaming_mode: bool,
    ) -> SearchConfig {
        if streaming_mode {
            search.youtube = false;
            search.soundcloud = false;
            search.audius = false;
            search.jamendo = false;
            search.internet_archive = false;
            search.radio_browser = true;
            search.source = SearchSource::RadioBrowser;
        } else {
            search.radio_browser = false;
            if search.source == SearchSource::RadioBrowser {
                search.source = SearchSource::Youtube;
            }
        }
        search.normalized()
    }

    /// Live retro-mode flag. While Settings is open, the draft is what the user is looking at,
    /// so render from it before the value is committed to config on close.
    pub fn retro_mode(&self) -> bool {
        self.settings.as_ref().map_or_else(
            || self.config.effective_retro_mode(),
            |s| s.draft.retro_mode,
        )
    }

    /// Live Beginner Mode flag. Settings previews the explanatory labels immediately, while the
    /// persisted value controls whether a walkthrough is eligible on the next writable launch.
    pub fn beginner_mode(&self) -> bool {
        self.settings
            .as_ref()
            .map_or(self.config.beginner_mode, |settings| {
                settings.draft.beginner_mode
            })
    }

    /// Whether expanded, explanatory control labels should render on the current frame.
    pub fn beginner_labels_enabled(&self) -> bool {
        self.beginner_mode()
    }

    /// Live player-bar position. Same draft-first rule as [`Self::retro_mode`]: while
    /// Settings is open the user is looking at the draft, so cycling the row previews the
    /// layout immediately, before it's committed on close.
    pub fn player_bar_position(&self) -> crate::config::PlayerBarPosition {
        self.settings.as_ref().map_or_else(
            || self.config.effective_player_bar_position(),
            |s| s.draft.player_bar_position,
        )
    }

    /// Whether the docked control box occupies rows on the current screen: Bottom mode,
    /// and either the Player screen (it IS the player, never collapsible) or the collapse
    /// toggle off. Gates both the per-view row reservation and the mouse targets the box
    /// publishes — a control that isn't rendered must never take clicks.
    pub fn control_box_active(&self) -> bool {
        self.player_bar_position() == crate::config::PlayerBarPosition::Bottom
            && (self.mode == Mode::Player || !self.config.control_box_collapsed())
    }
}
