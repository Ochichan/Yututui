//! Artwork/animation accessors, split out of the monolithic `app.rs` (behaviour-preserving).

use super::*;

impl App {
    /// Whether album art should drive the layout: the feature is on, a protocol was
    /// detected, and a decoded image is ready for the current track.
    pub fn art_active(&self) -> bool {
        self.config.effective_album_art()
            && self.art_picker.is_some()
            && self.art.borrow().is_some()
    }

    /// Whether the per-frame animation clock should run right now. True when we're on the
    /// player view (master switch + at least one effect enabled, a track loaded, not paused),
    /// **or** when the AI start-screen mascot wants to groove (see [`Self::ai_mascot_active`]).
    /// The main loop arms its ~30 fps tick on this; when it is false the tick never fires, so the
    /// app behaves byte-for-byte like today (the lightweight path).
    pub fn animation_active(&self) -> bool {
        (matches!(self.mode, Mode::Player)
            && !self.paused
            && self.queue.current().is_some()
            && self.config.animations.active())
            || self.ai_mascot_active()
    }

    /// Whether the "Gemini-tan" mascot on the AI start screen should be dancing right now. True
    /// only on the AI view *before any conversation has started*, while a track is actively
    /// playing and the global animation master switch is on. Unlike the player path this gates on
    /// `master` directly (not `active()`), so the mascot grooves even when every per-effect player
    /// toggle is off — the dance is its own thing. When this is false the mascot renders a clean
    /// resting pose and the tick stays asleep.
    pub fn ai_mascot_active(&self) -> bool {
        matches!(self.mode, Mode::Ai)
            && self.ai_messages.is_empty()
            && !self.paused
            && self.queue.current().is_some()
            && self.config.animations.master
    }

    /// The live animation config (per-effect toggles); read by the player view each frame.
    #[allow(dead_code)]
    pub fn animations(&self) -> &crate::config::AnimationsConfig {
        &self.config.animations
    }

    /// Current animation frame counter — advances ~30×/s while [`Self::animation_active`].
    pub fn anim_frame(&self) -> u64 {
        self.anim_frame
    }

    /// A bitmask of which `Clear` popups that paint over the album-art band are open:
    /// bit 0 = `eq:` dropdown, bit 1 = `radio:` dropdown, bit 2 = the queue window. The render
    /// loop snapshots this across dispatch and, on any change, calls [`Self::refresh_art`] so the
    /// graphics-protocol art repaints cleanly around (or after) the popup — see that method. A
    /// mask (not a bool) so switching one popup straight to another, or a second popup opening
    /// over a first, still registers as an edge. Add a bit here for any new art-covering popup.
    pub fn art_overlay_mask(&self) -> u8 {
        u8::from(self.eq_dropdown_open)
            | (u8::from(self.radio_dropdown_open) << 1)
            | (u8::from(self.queue_popup_open) << 2)
            | (u8::from(self.about_visible) << 3)
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
        if let (Some(img), Some(picker)) = (self.art_source.as_ref(), self.art_picker.as_ref()) {
            *self.art.borrow_mut() = Some(picker.new_resize_protocol(img.clone()));
        }
    }

    /// Turn a decoded image into a render-ready protocol (or clear when there's none / no
    /// picker). Building the protocol is cheap; the encode happens lazily at render. The decoded
    /// image is also kept (`art_source`) so [`Self::refresh_art`] can rebuild on a popup toggle.
    pub(in crate::app) fn set_artwork(&mut self, video_id: String, image: Option<DynamicImage>) {
        match (image, self.art_picker.as_ref()) {
            (Some(img), Some(picker)) => {
                self.art_dims = (img.width(), img.height());
                *self.art.borrow_mut() = Some(picker.new_resize_protocol(img.clone()));
                self.art_source = Some(img);
                self.art_video_id = Some(video_id);
            }
            _ => self.clear_artwork(),
        }
    }

    /// Drop any held art (track change, or the feature turned off) — also frees its RAM.
    pub(in crate::app) fn clear_artwork(&mut self) {
        *self.art.borrow_mut() = None;
        self.art_source = None;
        self.art_video_id = None;
        self.art_dims = (0, 0);
    }

    /// The art's source, if album art is on and a protocol was detected. `None` keeps the
    /// reducer from emitting a fetch (and the view from reserving space) when off.
    pub(in crate::app) fn artwork_source(&self, song: &Song) -> Option<ArtSource> {
        if !self.config.effective_album_art() || self.art_picker.is_none() {
            return None;
        }
        Some(match &song.local_path {
            Some(path) => ArtSource::Local(path.clone()),
            None => ArtSource::Remote { video_id: song.video_id.clone() },
        })
    }

    /// A centered sub-rect of `area` matching the art's aspect ratio, using the terminal's
    /// font cell size so square covers render square and wide thumbnails render wide. Falls
    /// back to the whole `area` when dimensions/font size are unknown.
    pub fn art_fit_rect(&self, area: Rect) -> Rect {
        let (iw, ih) = self.art_dims;
        let Some(font) = self.art_picker.as_ref().map(Picker::font_size) else {
            return area;
        };
        if iw == 0 || ih == 0 || font.width == 0 || font.height == 0 {
            return area;
        }
        let avail_w = f64::from(area.width) * f64::from(font.width);
        let avail_h = f64::from(area.height) * f64::from(font.height);
        let scale = (avail_w / f64::from(iw)).min(avail_h / f64::from(ih));
        let w = (((f64::from(iw) * scale) / f64::from(font.width)).round() as u16).clamp(1, area.width);
        let h = (((f64::from(ih) * scale) / f64::from(font.height)).round() as u16).clamp(1, area.height);
        Rect {
            x: area.x + (area.width - w) / 2,
            y: area.y + (area.height - h) / 2,
            width: w,
            height: h,
        }
    }
}
