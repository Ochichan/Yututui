//! Artwork/animation accessors, split out of the monolithic `app.rs` (behaviour-preserving).

use super::*;

const ART_REFRESH_OVERLAY_CLEAR_FRAMES: u8 = 3;

pub(in crate::app) const ART_OVERLAY_EQ_BIT: u32 = 1 << 0;
pub(in crate::app) const ART_OVERLAY_STREAMING_BIT: u32 = 1 << 1;
pub(in crate::app) const ART_OVERLAY_QUEUE_BIT: u32 = 1 << 2;
pub(in crate::app) const ART_OVERLAY_HELP_BIT: u32 = 1 << 3;
pub(in crate::app) const ART_OVERLAY_ABOUT_BIT: u32 = 1 << 4;
pub(in crate::app) const ART_OVERLAY_WHY_AI_BIT: u32 = 1 << 5;
pub(in crate::app) const ART_OVERLAY_KEY_CONFLICT_BIT: u32 = 1 << 6;
pub(in crate::app) const ART_OVERLAY_RADIO_CONFIRM_BIT: u32 = 1 << 7;
pub(in crate::app) const ART_OVERLAY_SETTINGS_CONFIRM_BIT: u32 = 1 << 8;
pub(in crate::app) const ART_OVERLAY_LIBRARY_CONFIRM_BIT: u32 = 1 << 9;
pub(in crate::app) const ART_OVERLAY_NOT_PLAYER_BIT: u32 = 1 << 10;
pub(in crate::app) const ART_OVERLAY_MOUSE_HELP_BIT: u32 = 1 << 11;
pub(in crate::app) const ART_OVERLAY_CREATE_PLAYLIST_BIT: u32 = 1 << 12;
pub(in crate::app) const ART_OVERLAY_DELETE_PLAYLIST_BIT: u32 = 1 << 13;
pub(in crate::app) const ART_OVERLAY_PLAYLIST_PICKER_BIT: u32 = 1 << 14;
pub(in crate::app) const ART_OVERLAY_SEARCH_FILTER_BIT: u32 = 1 << 15;
pub(in crate::app) const ART_OVERLAY_CONTEXT_MENU_BIT: u32 = 1 << 16;

// INVARIANT(ART-MASK-001): every art-covering surface owns a unique u32 bit; check the risk
// map before replacing, sharing, or widening any allocation.
#[cfg(test)]
pub(in crate::app) const ART_OVERLAY_BITS: &[(&str, u32)] = &[
    ("eq", ART_OVERLAY_EQ_BIT),
    ("streaming", ART_OVERLAY_STREAMING_BIT),
    ("queue", ART_OVERLAY_QUEUE_BIT),
    ("help", ART_OVERLAY_HELP_BIT),
    ("about", ART_OVERLAY_ABOUT_BIT),
    ("why_ai", ART_OVERLAY_WHY_AI_BIT),
    ("key_conflict", ART_OVERLAY_KEY_CONFLICT_BIT),
    ("radio_confirm", ART_OVERLAY_RADIO_CONFIRM_BIT),
    ("settings_confirm", ART_OVERLAY_SETTINGS_CONFIRM_BIT),
    ("library_confirm", ART_OVERLAY_LIBRARY_CONFIRM_BIT),
    ("not_player", ART_OVERLAY_NOT_PLAYER_BIT),
    ("mouse_help", ART_OVERLAY_MOUSE_HELP_BIT),
    ("create_playlist", ART_OVERLAY_CREATE_PLAYLIST_BIT),
    ("delete_playlist", ART_OVERLAY_DELETE_PLAYLIST_BIT),
    ("playlist_picker", ART_OVERLAY_PLAYLIST_PICKER_BIT),
    ("search_filter", ART_OVERLAY_SEARCH_FILTER_BIT),
    ("context_menu", ART_OVERLAY_CONTEXT_MENU_BIT),
];

const fn flag(on: bool, bit: u32) -> u32 {
    if on { bit } else { 0 }
}

impl App {
    pub fn set_art_resize_tx(&mut self, tx: tokio::sync::mpsc::Sender<ResizeRequest>) {
        self.art.resize_tx = Some(tx);
    }

    /// Whether album art should drive the layout: the feature is on, a protocol was
    /// detected, and a decoded image is ready for the current track.
    pub fn art_active(&self) -> bool {
        !self.radio_dedicated_mode
            && self.config.effective_album_art()
            && self.art.picker.is_some()
            && self.art.protocol.borrow().is_some()
            && !self.zoom_suppresses_native_art()
    }

    /// Text zoom renders on a scaled virtual grid, which pixel-protocol art can't join:
    /// its placeholder/anchor cells are forwarded unscaled, so a zoomed placement would
    /// stripe across the scaled rows. Native art is therefore hidden while zoomed —
    /// "zoom the text" literally — and the freed space goes to lyrics/track info.
    /// Halfblocks and retro ASCII art are text, so they keep rendering (and scale).
    fn zoom_suppresses_native_art(&self) -> bool {
        self.zoom.scale() > 1 && self.native_image_protocol_selected()
    }

    /// Whether rendering will actually ship a native terminal image this frame. Retro mode
    /// always draws text (ASCII art), so no native-image clear/resync work is ever needed
    /// there, even when a native protocol was detected at startup.
    fn native_image_protocol_selected(&self) -> bool {
        !self.retro_mode()
            && self.art.picker.as_ref().is_some_and(|picker| {
                picker.protocol_type() != ratatui_image::picker::ProtocolType::Halfblocks
            })
    }

    /// The held decoded art and its owning track id, for renderers that draw the image
    /// themselves (retro ASCII art) instead of through the terminal-graphics protocol.
    pub fn art_source_image(&self) -> Option<(&str, &DynamicImage)> {
        match (self.art.video_id.as_deref(), self.art.source.as_ref()) {
            (Some(id), Some(img)) => Some((id, img)),
            _ => None,
        }
    }

    fn native_art_active(&self) -> bool {
        self.art_active() && self.native_image_protocol_selected()
    }

    fn native_about_icon_touched(&self, previous: u32, next: u32) -> bool {
        ((previous | next) & ART_OVERLAY_ABOUT_BIT) != 0 && self.native_image_protocol_selected()
    }

    /// Whether the per-frame animation clock should run right now. True when we're on the
    /// player view (master switch + a visible continuous effect, a track loaded, not paused),
    /// radio mode has its built-in radio art motion enabled, when the DJ Gem start-screen
    /// mascot wants to groove (see [`Self::ai_mascot_active`]), while a one-shot feedback
    /// effect is mid-flight (see [`Self::fx_active`]), or while an ambient UI effect has
    /// something on screen to animate (see [`Self::ambient_animation_running`]).
    /// The main loop arms its ~30 fps tick on this; when it is false the tick never fires, so the
    /// app behaves byte-for-byte like today (the lightweight path).
    ///
    /// One additional gate suppresses the clock even when an effect is logically running:
    /// **Focus** — while `pause_unfocused` is on and the terminal has lost focus (minimized or
    /// behind another window), there's nothing to see, so we park the tick. Defaults make this
    /// a no-op on terminals that don't report focus (`focused` stays `true`). Overlays do not
    /// park the animation; they draw above the scene, matching the queue popup behavior.
    pub fn animation_active(&self) -> bool {
        let a = self.animations();
        let player_running = matches!(self.mode, Mode::Player)
            && !self.playback.paused
            && self.queue.current().is_some();
        // The radio set piece sways on `master` alone (no per-effect flag), but only while
        // it is actually on screen — the album-art toggle hides it (`render_radio_filler`).
        let radio_art_running = player_running
            && self.radio_dedicated_mode
            && self.config.effective_album_art()
            && a.master;
        let player_effect_running = player_running && self.player_animation_config_active(a);
        let running = player_effect_running
            || radio_art_running
            || self.ai_mascot_active()
            || self.fx_active()
            || self.ambient_animation_running()
            // A clipped selected row is marqueeing. Deliberately independent of both
            // animation masters (reading a row shouldn't require eye-candy); set by the
            // render pass, so it lags a frame at most.
            || self.bridges.marquee_ran.get();
        running && (!a.pause_unfocused || self.focused)
    }

    /// Continuous effects that can currently paint the player. Event feedback wakes the clock
    /// through `fx_active()` only; contextual UI effects use `ambient_animation_running()`.
    fn player_animation_config_active(&self, a: crate::config::AnimationsConfig) -> bool {
        a.master
            && (a.title
                || a.heart
                || a.seekbar
                || a.spinner
                || a.eq_bars
                || a.controls
                || a.border
                || (!self.lyrics.visible
                    && (a.rain || a.donut || a.visualizer || a.starfield || a.bounce)))
    }

    /// Whether an enabled one-shot feedback effect is still inside its window. While true the clock
    /// keeps ticking even where it would otherwise sleep (paused playback, non-player views) so
    /// the effect gets to finish; the deadline is comparison-based, so once passed the stale
    /// start frames cost nothing. Armed only through [`Self::fx_arm`], which is gated per-flag,
    /// so with every toggle off this is permanently false.
    pub fn fx_active(&self) -> bool {
        self.animations().master && self.anim.anim_frame < self.fx.until
    }

    /// Whether a *continuous* UI effect currently has something on screen to animate outside
    /// the player's own gate: a blinking caret in a visible text input, the breathing selection
    /// bar of the focused list, animated activity dots, the About card's sparkles, or the
    /// player's lyrics glow. Checked every run-loop iteration, so everything here is a plain
    /// field read — no row formatting or allocation.
    fn ambient_animation_running(&self) -> bool {
        let a = self.animations();
        if !a.master {
            return false;
        }
        if a.about_fx && self.overlays.about_visible {
            return true;
        }
        if a.caret && self.text_input_caret_visible() {
            return true;
        }
        // Breathing selection bars inside mode-independent popups (the add-to-playlist picker
        // and the search-source dropdown float over whichever screen opened them).
        if a.selection && (self.playlist_picker.is_some() || self.dropdowns.search_source_open) {
            return true;
        }
        match self.mode {
            Mode::Player => {
                // The element effects already run via the player gate while playing; the two
                // ambient extras are contextual selections and activity indicators. They retain
                // the player's pause/track gate while avoiding idle redraws.
                let playing = !self.playback.paused && self.queue.current().is_some();
                let downloading = self.queue.current().is_some_and(|song| {
                    matches!(
                        self.downloads.active.get(&song.video_id),
                        Some(DownloadState::Running(_))
                    )
                });
                (playing
                    && a.selection
                    && (self.queue_popup.open
                        || self.dropdowns.eq_open
                        || self.dropdowns.streaming_open))
                    || (playing && a.lyrics && self.lyrics.visible)
                    || (playing
                        && a.activity
                        && ((self.lyrics.visible && self.lyrics.loading) || downloading))
            }
            Mode::Search => {
                (a.activity && self.search.searching)
                    || (a.selection
                        && self.search.focus == SearchFocus::Results
                        && !self.search.results.is_empty())
            }
            Mode::Library => a.selection,
            // The settings cursor is fg-only (no selection bar) — nothing there breathes, so
            // the selection flag must not spin the clock; the caret case is handled above.
            Mode::Settings => false,
            Mode::Ai => {
                (a.activity && self.ai.thinking)
                    || (a.selection
                        && self.ai.focus == AiFocus::Suggestions
                        && !self.ai.suggestions.is_empty())
            }
        }
    }

    /// Whether some text input with a caret is on screen right now (the search box, the DJ Gem
    /// prompt, the library filter, a playlist-name entry, or a settings text field). Drives the
    /// caret-blink clock and the render helper, so the two can't drift.
    pub fn text_input_caret_visible(&self) -> bool {
        if self
            .playlist_picker
            .as_ref()
            .is_some_and(|p| p.naming.is_some())
        {
            return true;
        }
        // The results-filter popup's query input is always live while it's open.
        if self.search_filter.open {
            return true;
        }
        match self.mode {
            Mode::Search => self.search.focus == SearchFocus::Input,
            Mode::Ai => self.ai.focus == AiFocus::Input,
            Mode::Library => {
                self.library_ui.filter_editing || self.library_ui.create_input.is_some()
            }
            Mode::Settings => self.settings.as_ref().is_some_and(|s| s.editing_text),
            Mode::Player => false,
        }
    }

    /// Logical animation tick rate. This remains the configured FPS so frame-based animation phases
    /// keep their existing timing even when the renderer skips expensive intermediate frames.
    pub fn animation_tick_fps(&self) -> u16 {
        self.config.animations.effective_fps()
    }

    /// Actual redraw cadence for the active animation mix. One-shot feedback effects and cheap
    /// element effects keep the configured FPS; full-cell canvas effects cap repaint work;
    /// ambient UI effects (caret blink, selection breathing, activity dots, About sparkles) are
    /// slow breathers that look identical at ~12 fps; the DJ Gem mascot only needs to redraw
    /// when its pose can change.
    pub fn animation_draw_fps(&self) -> u16 {
        let fps = self.animation_tick_fps();
        let a = self.animations();
        if self.fx_active() {
            // One-shots are short and motion-dense; let them draw at the full tick rate.
            return fps;
        }
        let player_running = matches!(self.mode, Mode::Player)
            && !self.playback.paused
            && self.queue.current().is_some();
        if player_running
            && a.master
            && !self.lyrics.visible
            && (a.rain || a.donut || a.visualizer || a.starfield)
        {
            return fps.min(20);
        }
        if player_running
            && (self.player_animation_config_active(a)
                || (self.radio_dedicated_mode && self.config.effective_album_art() && a.master))
        {
            return fps;
        }
        if self.ambient_animation_running() {
            return fps.min(12);
        }
        if self.ai_mascot_active() {
            return (fps / 10).max(1);
        }
        if self.bridges.marquee_ran.get() {
            // Only the marquee is awake (both masters may be off): it advances one column
            // every 6 ticks, so redraw only on the steps instead of at the full tick rate.
            return (fps / 6).max(1);
        }
        fps
    }

    pub fn reset_animation_cadence(&mut self) {
        self.anim.anim_draw_credit = 0;
        self.anim.anim_last_draw_fps = 0;
    }

    pub(in crate::app) fn advance_animation(&mut self) {
        self.anim.anim_frame = self.anim.anim_frame.wrapping_add(1);

        let tick_fps = self.animation_tick_fps().max(1);
        let draw_fps = self.animation_draw_fps().clamp(1, tick_fps);
        if self.anim.anim_last_draw_fps != draw_fps {
            self.anim.anim_last_draw_fps = draw_fps;
            self.anim.anim_draw_credit = 0;
        }
        if draw_fps >= tick_fps {
            self.dirty = true;
            return;
        }

        self.anim.anim_draw_credit = self.anim.anim_draw_credit.saturating_add(draw_fps);
        if self.anim.anim_draw_credit >= tick_fps {
            self.anim.anim_draw_credit -= tick_fps;
            self.dirty = true;
        }
    }

    /// Frames the animation clock needs to cover `ms` milliseconds at the configured tick rate
    /// (never zero). One-shot effect windows are defined in wall-clock terms and converted
    /// through this, so they feel the same length at 5 fps and at 60 fps.
    pub fn anim_ms_frames(&self, ms: u64) -> u64 {
        (u64::from(self.animation_tick_fps()) * ms)
            .div_ceil(1000)
            .max(1)
    }

    /// Start a one-shot effect window `ms` long: returns the start frame for the effect's slot
    /// and extends [`FxState::until`] so [`Self::fx_active`] keeps the clock awake to the end.
    fn fx_arm(&mut self, ms: u64) -> u64 {
        let start = self.anim.anim_frame;
        self.fx.until = self.fx.until.max(start + self.anim_ms_frames(ms) + 1);
        self.dirty = true;
        start
    }

    /// Overlay-open bitmask for popup fade-in detection: the art overlay mask's popup bits
    /// (minus its bit 10, which is "not on the player screen", not a popup) plus the two
    /// overlays that mask doesn't track. A bit turning on means "a popup just opened".
    fn fx_popup_mask(&self) -> u32 {
        (self.art_overlay_mask() & !ART_OVERLAY_NOT_PLAYER_BIT)
            | ((self.overlays.spotify_picker.is_some() as u32) << 17)
            | ((self.dropdowns.search_source_open as u32) << 18)
    }

    /// Central one-shot trigger detection, called once per [`App::update`] turn after the
    /// reducer ran. Diffs the interesting state against the anchors in [`FxState`] and arms
    /// the matching effect windows. The anchors are refreshed unconditionally (so enabling a
    /// flag later can't replay a stale backlog of changes); the *triggers* are gated per-flag
    /// under `master`, so with everything off this never arms the clock.
    pub(in crate::app) fn detect_fx(&mut self, status_changed: bool, seeked: bool) {
        use crate::ui::anim::fx_window as w;
        let a = self.animations();
        let on = |flag: bool| a.master && flag;

        // Track change → title intro. Also remembered so this turn's liked-flag flip (a
        // *different* track being current) can't masquerade as a fresh like below.
        let current_id = self.queue.current().map(|s| s.video_id.as_str());
        let track_changed = current_id != self.fx.last_track_id.as_deref();
        if track_changed {
            self.fx.last_track_id = current_id.map(ToOwned::to_owned);
            self.fx.last_lyric_index = None;
            if on(a.track_intro) && self.fx.last_track_id.is_some() {
                self.fx.track_intro = Some(self.fx_arm(w::TRACK_INTRO_MS));
            }
        }

        // Neutral/dislike → liked (same track) → heart burst.
        let liked_now = self
            .queue
            .current()
            .is_some_and(|s| self.library.is_favorite(&s.video_id));
        if liked_now != self.fx.last_liked {
            self.fx.last_liked = liked_now;
            if liked_now && !track_changed && on(a.like_burst) {
                self.fx.like = Some(self.fx_arm(w::LIKE_MS));
            }
        }

        // Volume nudge (keys, mouse wheel, remote) → transient gauge.
        if self.playback.volume != self.fx.last_volume {
            self.fx.last_volume = self.playback.volume;
            if on(a.volume_flash) {
                self.fx.volume = Some(self.fx_arm(w::VOLUME_MS));
            }
        }

        // A seek command went out this turn → ripple at the seekbar head.
        if seeked && on(a.seek_flash) {
            self.fx.seek = Some(self.fx_arm(w::SEEK_MS));
        }

        // A fresh status message → typewriter reveal, window scaled to the text length.
        if status_changed && !self.status.text.is_empty() && on(a.toast) {
            let cols = unicode_width::UnicodeWidthStr::width(self.status.text.as_str());
            self.fx.toast = Some(self.fx_arm(w::toast_ms(cols)));
        }

        // Screen switch → nav-tab pop + the new view's list cascade.
        if self.mode != self.fx.last_mode {
            self.fx.last_mode = self.mode;
            if on(a.tabs) {
                self.fx.switch = Some((self.fx_arm(w::SWITCH_MS), self.mode));
            }
            if on(a.stagger) {
                self.fx.list = Some((self.fx_arm(w::LIST_MS), self.mode));
            }
        }

        // Library tab / opened playlist / Settings tab changes → tab pop + list cascade.
        if self.library_ui.tab != self.fx.last_library_tab {
            self.fx.last_library_tab = self.library_ui.tab;
            if on(a.tabs) {
                self.fx.tabbar = Some(self.fx_arm(w::SWITCH_MS));
            }
            if on(a.stagger) {
                self.fx.list = Some((self.fx_arm(w::LIST_MS), Mode::Library));
            }
        }
        if self.library_ui.open_playlist != self.fx.last_open_playlist {
            self.fx.last_open_playlist = self.library_ui.open_playlist.clone();
            if on(a.stagger) {
                self.fx.list = Some((self.fx_arm(w::LIST_MS), Mode::Library));
            }
        }
        let settings_tab = self.settings.as_ref().map(|s| s.tab);
        if settings_tab != self.fx.last_settings_tab {
            let switched_within = settings_tab.is_some() && self.fx.last_settings_tab.is_some();
            self.fx.last_settings_tab = settings_tab;
            if switched_within {
                if on(a.tabs) {
                    self.fx.tabbar = Some(self.fx_arm(w::SWITCH_MS));
                }
                if on(a.stagger) {
                    self.fx.list = Some((self.fx_arm(w::LIST_MS), Mode::Settings));
                }
            }
        }

        // A search just finished → results cascade (covers every entry path to results).
        if self.search.searching != self.fx.last_searching {
            let finished = self.fx.last_searching && !self.search.searching;
            self.fx.last_searching = self.search.searching;
            if finished && on(a.stagger) {
                self.fx.list = Some((self.fx_arm(w::LIST_MS), Mode::Search));
            }
        }

        // Any popup/dropdown newly opened → fade-in materialize.
        let popup_mask = self.fx_popup_mask();
        let newly_opened = popup_mask & !self.fx.last_popup_mask;
        self.fx.last_popup_mask = popup_mask;
        if newly_opened != 0 && on(a.popup_fade) {
            self.fx.popup = Some(self.fx_arm(w::POPUP_MS));
        }

        // The synced-lyric line advanced → flash the newly-current line. Only tracked while
        // the panel is visible on the player, so the index scan never runs anywhere else.
        if matches!(self.mode, Mode::Player) && self.lyrics.visible && on(a.lyrics) {
            let idx = self
                .lyrics
                .track
                .as_ref()
                .filter(|t| !t.lines.is_empty())
                .and_then(|t| {
                    crate::lyrics::current_index(&t.lines, self.playback.time_pos.unwrap_or(0.0))
                });
            if idx != self.fx.last_lyric_index {
                self.fx.last_lyric_index = idx;
                if idx.is_some() {
                    self.fx.lyric = Some(self.fx_arm(w::LYRIC_MS));
                }
            }
        }
    }

    /// Whether the next draw benefits from DEC synchronized update. Plain forms and one-line
    /// status redraws avoid the extra escape traffic; album art and canvas animation keep the
    /// atomic swap that prevents tearing/ghosting on terminals that support it.
    pub fn synchronized_draw_active(&self) -> bool {
        let a = self.animations();
        self.art_active()
            || (matches!(self.mode, Mode::Player)
                && a.master
                && (a.rain || a.donut || a.visualizer || a.starfield))
    }

    /// Whether the "Gemini-tan" mascot on the DJ Gem start screen should be dancing right now. True
    /// only on the DJ Gem view *before any conversation has started*, while a track is actively
    /// playing and the global animation master switch is on. Unlike the player path this gates on
    /// `master` directly (not `active()`), so the mascot grooves even when every per-effect player
    /// toggle is off — the dance is its own thing. When this is false the mascot renders a clean
    /// resting pose and the tick stays asleep.
    pub fn ai_mascot_active(&self) -> bool {
        matches!(self.mode, Mode::Ai)
            && self.ai.messages.is_empty()
            && !self.playback.paused
            && self.queue.current().is_some()
            && self.animations().master
    }

    /// The live animation config (per-effect toggles) with `master` resolved for the
    /// current mode (music vs dedicated Radio); read by the player view each frame and by
    /// the nav bar's ✨ toggle. Returned by value — persist `self.config`, never this copy,
    /// or the Radio/music inherit link gets baked into the file.
    pub fn animations(&self) -> crate::config::AnimationsConfig {
        self.config.animations.effective(self.radio_dedicated_mode)
    }

    /// Current animation frame counter — advances ~30×/s while [`Self::animation_active`].
    pub fn anim_frame(&self) -> u64 {
        self.anim.anim_frame
    }

    /// Flip the animation master switch *for the current mode* and persist it. Music and
    /// dedicated Radio mode each keep their own switch (`master` / `radio_master`, where the
    /// radio one inherits `master` until first toggled), so muting the eye-candy in one mode
    /// doesn't dim the other. Shared by the `A` shortcut ([`Action::ToggleAnimations`]) and
    /// the ✨ nav-bar button, so both paths behave identically (DRY). Shows a transient ✓/✗
    /// toast (auto-expired centrally by [`App::update`]).
    pub(in crate::app) fn toggle_animations(&mut self) -> Vec<Cmd> {
        let on = !self.animations().master;
        if self.radio_dedicated_mode {
            self.config.animations.radio_master = Some(on);
        } else {
            self.config.animations.master = on;
        }
        if !on {
            self.fx.cancel();
        }
        // If the Settings screen is open, its draft is the source of truth on close
        // (`SettingsDraft::apply_to` copies `draft.animations` wholesale), so mirror the flip there
        // too — otherwise closing Settings would silently revert what the user just toggled.
        let radio = self.radio_dedicated_mode;
        if let Some(s) = self.settings.as_mut() {
            if radio {
                s.draft.animations.radio_master = Some(on);
            } else {
                s.draft.animations.master = on;
            }
        }
        self.status.kind = StatusKind::Info;
        self.status.text = format!(
            "{}: {}",
            if radio {
                t!("Animations (Radio)", "애니메이션(라디오)")
            } else {
                t!("Animations", "애니메이션")
            },
            if on { "✓" } else { "✗" }
        );
        self.dirty = true;
        vec![Cmd::Persist(PersistCmd::Config(Box::new(
            self.config.clone(),
        )))]
    }

    /// A bitmask of visible surfaces that can cover album art. Keeping each popup/modal distinct
    /// lets the render loop notice every transition that can desynchronize native terminal
    /// graphics from ratatui's diff buffer.
    pub fn art_overlay_mask(&self) -> u32 {
        flag(self.dropdowns.eq_open, ART_OVERLAY_EQ_BIT)
            | flag(self.dropdowns.streaming_open, ART_OVERLAY_STREAMING_BIT)
            | flag(self.queue_popup.open, ART_OVERLAY_QUEUE_BIT)
            | flag(self.overlays.help_visible, ART_OVERLAY_HELP_BIT)
            | flag(self.overlays.about_visible, ART_OVERLAY_ABOUT_BIT)
            | flag(self.overlays.why_ai_visible, ART_OVERLAY_WHY_AI_BIT)
            | flag(
                self.overlays.key_conflict.is_some(),
                ART_OVERLAY_KEY_CONFLICT_BIT,
            )
            | flag(
                self.radio_mode.pending_radio_mode_confirm.is_some(),
                ART_OVERLAY_RADIO_CONFIRM_BIT,
            )
            | flag(
                self.overlays.pending_settings_confirm.is_some(),
                ART_OVERLAY_SETTINGS_CONFIRM_BIT,
            )
            // Bit 9 is shared by Library/Local file-operation confirm modals. They are modal
            // and use the same centered footprint, so one bit tracks them without claiming
            // another scarce overlay bit.
            | flag(
                self.library_ui.confirm_delete.is_some()
                    || self.library_ui.confirm_download.is_some()
                    || self.local_mode.pending_organize_confirm.is_some()
                    || self.local_mode.pending_accept_all_confirm.is_some()
                    || self.local_mode.pending_import_record_delete.is_some(),
                ART_OVERLAY_LIBRARY_CONFIRM_BIT,
            )
            | flag(!matches!(self.mode, Mode::Player), ART_OVERLAY_NOT_PLAYER_BIT)
            | flag(self.overlays.mouse_help_visible, ART_OVERLAY_MOUSE_HELP_BIT)
            | flag(
                self.library_ui.create_input.is_some(),
                ART_OVERLAY_CREATE_PLAYLIST_BIT,
            )
            | flag(
                self.library_ui.confirm_playlist_delete.is_some(),
                ART_OVERLAY_DELETE_PLAYLIST_BIT,
            )
            | flag(
                self.playlist_picker.is_some(),
                ART_OVERLAY_PLAYLIST_PICKER_BIT,
            )
            | flag(self.search_filter.open, ART_OVERLAY_SEARCH_FILTER_BIT)
            | flag(
                self.overlays.context_menu.is_some(),
                ART_OVERLAY_CONTEXT_MENU_BIT,
            )
    }

    /// Track overlay/screen transitions that can cover native terminal graphics. Ratatui's normal
    /// frame diff is sufficient for text cells, but protocols such as Sixel park image bytes in
    /// one anchor cell and mark the rest skipped. A popup open/close can therefore leave terminal
    /// graphics stale even though the next ratatui buffer is logically correct. One full clear on
    /// the next frame re-syncs the terminal without paying that cost during steady-state playback.
    pub(in crate::app) fn sync_art_overlay_state(&mut self) {
        let next = self.art_overlay_mask();
        if self.art.overlay_mask == next {
            return;
        }
        let previous = self.art.overlay_mask;
        self.art.overlay_mask = next;
        if self.native_art_active() || self.native_about_icon_touched(previous, next) {
            self.request_native_image_clear();
            tracing::debug!(
                previous,
                next,
                "native-image overlay state changed; next frame will clear before draw"
            );
        }
    }

    pub(in crate::app) fn request_native_image_clear(&mut self) {
        self.art.force_clear_next_frame = true;
        self.dirty = true;
    }

    fn reinforce_overlay_for_art_refresh(&mut self) {
        if self.art.overlay_mask == 0 || !self.native_image_protocol_selected() {
            return;
        }
        self.art.overlay_refresh_clear_frames = self
            .art
            .overlay_refresh_clear_frames
            .max(ART_REFRESH_OVERLAY_CLEAR_FRAMES.saturating_sub(1));
        self.request_native_image_clear();
    }

    /// Consume a full-redraw request set by native image / overlay synchronization.
    pub fn take_clear_before_draw(&mut self) -> bool {
        if std::mem::take(&mut self.art.force_clear_next_frame) {
            return true;
        }
        if self.art.overlay_refresh_clear_frames > 0 {
            self.art.overlay_refresh_clear_frames -= 1;
            return true;
        }
        false
    }

    pub fn clear_before_draw_pending(&self) -> bool {
        self.art.force_clear_next_frame || self.art.overlay_refresh_clear_frames > 0
    }

    /// Turn a decoded image into a render-ready protocol (or clear when there's none / no
    /// picker). Building the protocol is cheap; the encode happens lazily at render.
    pub(in crate::app) fn set_artwork(&mut self, video_id: String, image: Option<DynamicImage>) {
        match (image, self.art.picker.as_ref()) {
            (Some(img), Some(picker)) if self.art.resize_tx.is_some() => {
                self.art.dims = (img.width(), img.height());
                let tx = self.art.resize_tx.as_ref().expect("checked above").clone();
                *self.art.protocol.borrow_mut() = Some(ThreadProtocol::new(
                    tx,
                    Some(picker.new_resize_protocol(img.clone())),
                ));
                self.art.source = Some(img);
                self.art.video_id = Some(video_id);
                self.reinforce_overlay_for_art_refresh();
            }
            _ => self.clear_artwork(),
        }
    }

    pub(in crate::app) fn apply_artwork_resize(&mut self, response: ResizeResponse) {
        let updated = {
            let mut protocol = self.art.protocol.borrow_mut();
            protocol
                .as_mut()
                .is_some_and(|proto| proto.update_resized_protocol(response))
        };
        if updated {
            self.reinforce_overlay_for_art_refresh();
            self.dirty = true;
        }
    }

    /// Drop any held art (track change, or the feature turned off) — also frees its RAM.
    pub(in crate::app) fn clear_artwork(&mut self) {
        let had_native_art_under_overlay = self.native_art_active() && self.art.overlay_mask != 0;
        *self.art.protocol.borrow_mut() = None;
        self.art.source = None;
        self.art.video_id = None;
        self.art.dims = (0, 0);
        self.art.loading = false;
        if had_native_art_under_overlay {
            self.reinforce_overlay_for_art_refresh();
        }
    }

    /// The art's source, if album art is on and a protocol was detected. `None` keeps the
    /// reducer from emitting a fetch (and the view from reserving space) when off.
    pub(in crate::app) fn artwork_source(&self, song: &Song) -> Option<ArtSource> {
        if self.radio_dedicated_mode
            || !self.config.effective_album_art()
            || self.art.picker.is_none()
        {
            return None;
        }
        Some(match &song.local_path {
            Some(path) => ArtSource::Local(path.clone()),
            None => ArtSource::Remote {
                video_id: song.youtube_id()?.to_owned(),
            },
        })
    }

    /// A centered sub-rect of `area` matching the art's aspect ratio, using the terminal's
    /// font cell size so square covers render square and wide thumbnails render wide. Falls
    /// back to the whole `area` when dimensions/font size are unknown.
    pub fn art_fit_rect(&self, area: Rect) -> Rect {
        let (iw, ih) = self.art.dims;
        let Some(font) = self.art.picker.as_ref().map(Picker::font_size) else {
            return area;
        };
        if iw == 0 || ih == 0 || font.width == 0 || font.height == 0 {
            return area;
        }
        let avail_w = f64::from(area.width) * f64::from(font.width);
        let avail_h = f64::from(area.height) * f64::from(font.height);
        let scale = (avail_w / f64::from(iw)).min(avail_h / f64::from(ih));
        let w =
            (((f64::from(iw) * scale) / f64::from(font.width)).round() as u16).clamp(1, area.width);
        let h = (((f64::from(ih) * scale) / f64::from(font.height)).round() as u16)
            .clamp(1, area.height);
        Rect {
            x: area.x + (area.width - w) / 2,
            y: area.y + (area.height - h) / 2,
            width: w,
            height: h,
        }
    }
}
