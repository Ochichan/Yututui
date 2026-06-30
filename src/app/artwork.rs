//! Artwork/animation accessors, split out of the monolithic `app.rs` (behaviour-preserving).

use super::*;

impl App {
    pub(crate) fn set_art_resize_tx(
        &mut self,
        tx: tokio::sync::mpsc::UnboundedSender<ResizeRequest>,
    ) {
        self.art.resize_tx = Some(tx);
    }

    /// Whether album art should drive the layout: the feature is on, a protocol was
    /// detected, and a decoded image is ready for the current track.
    pub fn art_active(&self) -> bool {
        self.config.effective_album_art()
            && self.art.picker.is_some()
            && self.art.protocol.borrow().is_some()
    }

    /// Whether the per-frame animation clock should run right now. True when we're on the
    /// player view (master switch + at least one effect enabled, a track loaded, not paused),
    /// **or** when the AI start-screen mascot wants to groove (see [`Self::ai_mascot_active`]).
    /// The main loop arms its ~30 fps tick on this; when it is false the tick never fires, so the
    /// app behaves byte-for-byte like today (the lightweight path).
    ///
    /// Two additional gates suppress the clock even when an effect is logically running:
    /// - **Focus** — while `pause_unfocused` is on and the terminal has lost focus (minimized or
    ///   behind another window), there's nothing to see, so we park the tick. Defaults make this
    ///   a no-op on terminals that don't report focus (`focused` stays `true`).
    /// - **Full-screen overlays** — the help (`?`) and About panes cover the animated area, so we
    ///   freeze underneath them and resume on close. (Partial dropdowns don't count.)
    pub fn animation_active(&self) -> bool {
        let running = (matches!(self.mode, Mode::Player)
            && !self.playback.paused
            && self.queue.current().is_some()
            && self.config.animations.active())
            || self.ai_mascot_active();
        running
            && (!self.config.animations.pause_unfocused || self.focused)
            && !self.help_visible
            && !self.about_visible
            && !self.why_ai_visible
    }

    /// Logical animation tick rate. This remains the configured FPS so frame-based animation phases
    /// keep their existing timing even when the renderer skips expensive intermediate frames.
    pub fn animation_tick_fps(&self) -> u16 {
        self.config.animations.effective_fps()
    }

    /// Actual redraw cadence for the active animation mix. Cheap element effects keep the configured
    /// FPS; full-cell canvas effects cap repaint work; the AI mascot only needs to redraw when its
    /// pose can change.
    pub fn animation_draw_fps(&self) -> u16 {
        let fps = self.animation_tick_fps();
        let a = &self.config.animations;
        if matches!(self.mode, Mode::Player)
            && a.master
            && (a.rain || a.donut || a.visualizer || a.starfield)
        {
            fps.min(20)
        } else if self.ai_mascot_active() {
            (fps / 10).max(1)
        } else {
            fps
        }
    }

    pub(crate) fn reset_animation_cadence(&mut self) {
        self.anim_draw_credit = 0;
        self.anim_last_draw_fps = 0;
    }

    pub(in crate::app) fn advance_animation(&mut self) {
        self.anim_frame = self.anim_frame.wrapping_add(1);

        let tick_fps = self.animation_tick_fps().max(1);
        let draw_fps = self.animation_draw_fps().clamp(1, tick_fps);
        if self.anim_last_draw_fps != draw_fps {
            self.anim_last_draw_fps = draw_fps;
            self.anim_draw_credit = 0;
        }
        if draw_fps >= tick_fps {
            self.dirty = true;
            return;
        }

        self.anim_draw_credit = self.anim_draw_credit.saturating_add(draw_fps);
        if self.anim_draw_credit >= tick_fps {
            self.anim_draw_credit -= tick_fps;
            self.dirty = true;
        }
    }

    /// Whether the next draw benefits from DEC synchronized update. Plain forms and one-line
    /// status redraws avoid the extra escape traffic; album art and canvas animation keep the
    /// atomic swap that prevents tearing/ghosting on terminals that support it.
    pub fn synchronized_draw_active(&self) -> bool {
        let a = &self.config.animations;
        self.art_active()
            || (matches!(self.mode, Mode::Player)
                && a.master
                && (a.rain || a.donut || a.visualizer || a.starfield))
    }

    /// Whether the "Gemini-tan" mascot on the AI start screen should be dancing right now. True
    /// only on the AI view *before any conversation has started*, while a track is actively
    /// playing and the global animation master switch is on. Unlike the player path this gates on
    /// `master` directly (not `active()`), so the mascot grooves even when every per-effect player
    /// toggle is off — the dance is its own thing. When this is false the mascot renders a clean
    /// resting pose and the tick stays asleep.
    pub fn ai_mascot_active(&self) -> bool {
        matches!(self.mode, Mode::Ai)
            && self.ai.messages.is_empty()
            && !self.playback.paused
            && self.queue.current().is_some()
            && self.config.animations.master
    }

    /// The live animation config (per-effect toggles); read by the player view each frame and by
    /// the nav bar's ✨ toggle.
    pub fn animations(&self) -> &crate::config::AnimationsConfig {
        &self.config.animations
    }

    /// Current animation frame counter — advances ~30×/s while [`Self::animation_active`].
    pub fn anim_frame(&self) -> u64 {
        self.anim_frame
    }

    /// Flip the global animation master switch and persist it. Shared by the `A` shortcut
    /// ([`Action::ToggleAnimations`]) and the ✨ nav-bar button, so both paths behave identically
    /// (DRY). Shows a transient ✓/✗ toast (auto-expired centrally by [`App::update`]).
    pub(in crate::app) fn toggle_animations(&mut self) -> Vec<Cmd> {
        let on = !self.config.animations.master;
        self.config.animations.master = on;
        // If the Settings screen is open, its draft is the source of truth on close
        // (`SettingsDraft::apply_to` copies `draft.animations` wholesale), so mirror the flip there
        // too — otherwise closing Settings would silently revert what the user just toggled.
        if let Some(s) = self.settings.as_mut() {
            s.draft.animations.master = on;
        }
        self.status.kind = StatusKind::Info;
        self.status.text = format!(
            "{}: {}",
            t!("Animations", "애니메이션"),
            if on { "✓" } else { "✗" }
        );
        self.dirty = true;
        vec![Cmd::SaveConfig(Box::new(self.config.clone()))]
    }

    /// A bitmask of visible surfaces that can cover graphics-protocol album art. Text-cell
    /// overlays (`Clear`, modal blocks, and non-player screens) do not reliably erase Sixel/iTerm2
    /// pixels, so the main loop snapshots this mask and clears native terminal graphics when it
    /// changes. A mask (not a bool) keeps direct switches between overlays visible to the loop.
    pub fn art_overlay_mask(&self) -> u16 {
        u8::from(self.dropdowns.eq_open) as u16
            | ((self.dropdowns.radio_open as u16) << 1)
            | ((self.queue_popup.open as u16) << 2)
            | ((self.help_visible as u16) << 3)
            | ((self.about_visible as u16) << 4)
            | ((self.why_ai_visible as u16) << 5)
            | ((self.key_conflict.is_some() as u16) << 6)
            | ((self.confirm_reset_all as u16) << 7)
            | ((self.library_ui.confirm_delete.is_some() as u16) << 8)
            | ((!matches!(self.mode, Mode::Player) as u16) << 9)
    }

    /// Whether the active album-art backend paints outside the normal text buffer. Halfblocks is
    /// regular terminal text and can be overdrawn by `Clear`; graphics protocols need stronger
    /// handling when popups or screen-level panels cover them.
    pub fn art_uses_terminal_graphics(&self) -> bool {
        self.art
            .picker
            .as_ref()
            .is_some_and(|picker| !matches!(picker.protocol_type(), ProtocolType::Halfblocks))
    }

    /// Suppress native graphics-protocol art while an overlay is visible. The main loop clears the
    /// terminal graphics layer on overlay transitions, and this keeps the next frame from
    /// immediately re-planting art above the popup on terminals without reliable graphics z-order.
    pub fn art_suppressed_by_overlay(&self) -> bool {
        self.art_uses_terminal_graphics() && self.art_overlay_mask() != 0
    }

    /// Rebuild the held art into a fresh protocol so the *next* render re-transmits and
    /// re-emits the whole image. ratatui-image only re-emits its Kitty unicode-placeholder rows
    /// when the render *area* changes, so a `Clear` popup that overdraws part of the art (the
    /// `eq:`/`radio:` dropdowns, the queue window) leaves a stale background box where it was —
    /// the art never repaints there on its own. `new_resize_protocol` mints a new random
    /// graphics id, which changes every row's escape so ratatui's diff re-emits all of them and
    /// the terminal re-transmits the pixels — a complete, flicker-localized repaint with no
    /// full-screen `clear()` flash. This is how `eq:`/`radio:`/queue get the same clean
    /// appear/disappear the (full-width) `?` help overlay gets for free. Cheap clone of one
    /// already-bounded image (`MAX_DIM`), only on a popup toggle.
    pub fn refresh_art(&self) {
        if let (Some(img), Some(picker), Some(tx)) = (
            self.art.source.as_ref(),
            self.art.picker.as_ref(),
            self.art.resize_tx.as_ref(),
        ) {
            let fresh = picker.new_resize_protocol(img.clone());
            let mut protocol = self.art.protocol.borrow_mut();
            match protocol.as_mut() {
                Some(current) => current.refresh_protocol(fresh),
                None => *protocol = Some(ThreadProtocol::new(tx.clone(), Some(fresh))),
            }
        }
    }

    /// Turn a decoded image into a render-ready protocol (or clear when there's none / no
    /// picker). Building the protocol is cheap; the encode happens lazily at render. The decoded
    /// image is also kept (`art_source`) so [`Self::refresh_art`] can rebuild on a popup toggle.
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
            }
            _ => self.clear_artwork(),
        }
    }

    pub(in crate::app) fn apply_artwork_resize(&mut self, response: ResizeResponse) {
        if let Some(proto) = self.art.protocol.borrow_mut().as_mut()
            && proto.update_resized_protocol(response)
        {
            self.dirty = true;
        }
    }

    /// Drop any held art (track change, or the feature turned off) — also frees its RAM.
    pub(in crate::app) fn clear_artwork(&mut self) {
        *self.art.protocol.borrow_mut() = None;
        self.art.source = None;
        self.art.video_id = None;
        self.art.dims = (0, 0);
    }

    /// The art's source, if album art is on and a protocol was detected. `None` keeps the
    /// reducer from emitting a fetch (and the view from reserving space) when off.
    pub(in crate::app) fn artwork_source(&self, song: &Song) -> Option<ArtSource> {
        if !self.config.effective_album_art() || self.art.picker.is_none() {
            return None;
        }
        Some(match &song.local_path {
            Some(path) => ArtSource::Local(path.clone()),
            None => ArtSource::Remote {
                video_id: song.video_id.clone(),
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
