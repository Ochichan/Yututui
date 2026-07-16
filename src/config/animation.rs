use serde::{Deserialize, Serialize};

/// UI eye-candy toggles (the **Animations** settings tab). Every field is an
/// independent on/off; **all default to `false`** so a fresh install behaves exactly like
/// before (the app's whole identity is "fast and light"). `master` is a global kill-switch:
/// when it is off, nothing animates regardless of the per-effect flags, and the animation
/// frame-clock never even wakes (see `App::animation_active`). Grouped under one JSON key
/// (`"animations"`) and `#[serde(default)]` so older config files forward-migrate cleanly.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct AnimationsConfig {
    /// Global enable. Off → the player renders identically to today, zero overhead.
    pub master: bool,
    /// Dedicated-Radio-mode override for `master`. `None` inherits `master` (existing
    /// configs keep behaving as one global switch); the first ✨/`A` toggle taken while in
    /// Radio mode pins it, after which the two modes animate independently. A scope
    /// selector, not an effect — deliberately excluded from [`Self::any_effect`]. Resolved
    /// through [`Self::effective`]; there is no Settings row for it.
    pub radio_master: Option<bool>,
    // Element-level effects (restyle existing widgets in place) -----------------
    /// Shimmer + marquee scroll on the now-playing title line.
    pub title: bool,
    /// Pulse the `♥` like-marker when the track is in the library.
    pub heart: bool,
    /// Seekbar motion: the sweeping comet on the filled gauge plus smooth sub-second fill
    /// (the gauge interpolates between mpv's one-per-second reports while the clock runs).
    pub seekbar: bool,
    /// Spinning throbber next to "▸ playing" on the status line.
    pub spinner: bool,
    /// Faux VU `▁▂▃▅▇` bars on the status line (and a mini VU marker on the queue window's
    /// now-playing row).
    pub eq_bars: bool,
    /// Pulse/glow the transport play-pause glyph.
    pub controls: bool,
    /// "Breathing" outer border colour cycle.
    pub border: bool,
    // Player one-shots (event feedback, each plays once and the clock re-sleeps) -
    /// Letter-cascade reveal of the title line when a new track starts.
    pub track_intro: bool,
    /// Synced-lyrics polish: the current line breathes and flashes as it becomes current;
    /// far lines fade with distance.
    pub lyrics: bool,
    /// Transient status messages type themselves in with a bright caret head.
    pub toast: bool,
    /// A short volume gauge flashes under the transport strip when the volume changes.
    pub volume_flash: bool,
    /// A little burst of hearts/sparks around the title when the track is liked.
    pub like_burst: bool,
    /// A bright ripple at the seekbar head after a seek.
    pub seek_flash: bool,
    // UI-wide effects (Search / Library / Settings / DJ Gem, not just the player) -
    /// The focused list selection bar breathes gently toward the accent colour.
    pub selection: bool,
    /// List rows cascade in top-to-bottom on view/tab switches and new search results.
    pub stagger: bool,
    /// Text-input carets blink (search box, filter, playlist names, settings fields, DJ Gem).
    pub caret: bool,
    /// The active tab pops with a brief accent wash on view/tab switches.
    pub tabs: bool,
    /// Popups and dropdowns materialize with a ~150 ms fade-in instead of appearing at once.
    pub popup_fade: bool,
    /// Activity indicators animate: "Searching…" dots, lyrics fetching, DJ Gem "…thinking",
    /// and a spinner on a running download's `⬇ N%` tag.
    pub activity: bool,
    /// The About card twinkles: sparkles around the icon and a gradient sweep on the name.
    pub about_fx: bool,
    // Second-wave Now Playing element effects ----------------------------------
    /// The seekbar's gauge and time label glow briefly as each playback second lands.
    pub time_glow: bool,
    /// Tiny sparks twinkle around the seekbar head while playback runs.
    pub progress_sparkle: bool,
    /// A short bright comet chases clockwise around the Player view's outer border.
    pub border_chase: bool,
    /// A light wave washes across the transport controls when play/pause toggles.
    pub pause_flash: bool,
    /// Error status messages shake side-to-side with a decaying oscillation.
    pub error_shake: bool,
    // Filler-canvas effects (drawn only in blank zones) ------------------------
    /// Matrix-style digital rain in the free zone(s).
    pub rain: bool,
    /// Classic spinning ASCII donut.
    pub donut: bool,
    /// Decorative (non-audio-reactive) spectrum visualizer.
    pub visualizer: bool,
    /// Drifting stars / musical notes.
    pub starfield: bool,
    /// DVD-style bouncing logo.
    pub bounce: bool,
    /// Occasional diagonal shooting-star streaks.
    pub comets: bool,
    /// Sparse drifting snowfall.
    pub snow: bool,
    /// Fireflies wandering on smooth glowing paths.
    pub fireflies: bool,
    /// Rotating 3-D wireframe cube.
    pub cube: bool,
    /// ASCII aquarium: fish swimming both ways plus rising bubbles.
    pub aquarium: bool,
    /// Layered ocean waves along the bottom of the free zone.
    pub waves: bool,
    /// Periodic firework launches with radial particle bursts.
    pub fireworks: bool,
    /// Conway's Game of Life colony, colour-coded by cell age.
    pub life: bool,
    /// The classic pipes screensaver growing across the free zone.
    pub pipes: bool,
    /// Demoscene plasma colour field over the whole free zone (the heaviest effect).
    pub plasma: bool,
    // Behaviour knobs (not effects) -------------------------------------------
    /// Park the animation tick while the terminal is unfocused (minimized or behind another
    /// window). Defaults to `true`; opt out to keep animating off-screen. No-op on terminals that
    /// don't report focus (DECSET ?1004). See [`crate::app::App::animation_active`].
    pub pause_unfocused: bool,
    /// Target animation frame rate. Read through [`Self::effective_fps`], which clamps to
    /// [`FPS_MIN`]..=[`FPS_MAX`] so a hand-edited or corrupt config can't spin the loop or freeze
    /// it. Lower values trade smoothness for less CPU/battery. Default [`FPS_DEFAULT`].
    pub fps: u16,
}

/// Animation frame-rate bounds. The floor keeps motion perceptible; the ceiling caps the
/// redraw cost. The default matches the long-standing fixed ~30 fps tick.
pub const FPS_MIN: u16 = 5;
pub const FPS_MAX: u16 = 60;
pub const FPS_DEFAULT: u16 = 30;

impl Default for AnimationsConfig {
    /// All visual effects start **off** (reduced-motion by default); the behaviour knobs default
    /// to `pause_unfocused: true` and `fps: 30`. A manual impl (rather than `#[derive(Default)]`)
    /// is required so these aren't `bool`'s `false` / `u16`'s `0` (a `0` fps would clamp to the
    /// floor and silently override the intended default).
    fn default() -> Self {
        Self {
            master: false,
            radio_master: None,
            title: false,
            heart: false,
            seekbar: false,
            spinner: false,
            eq_bars: false,
            controls: false,
            border: false,
            track_intro: false,
            lyrics: false,
            toast: false,
            volume_flash: false,
            like_burst: false,
            seek_flash: false,
            selection: false,
            stagger: false,
            caret: false,
            tabs: false,
            popup_fade: false,
            activity: false,
            about_fx: false,
            time_glow: false,
            progress_sparkle: false,
            border_chase: false,
            pause_flash: false,
            error_shake: false,
            rain: false,
            donut: false,
            visualizer: false,
            starfield: false,
            bounce: false,
            comets: false,
            snow: false,
            fireflies: false,
            cube: false,
            aquarium: false,
            waves: false,
            fireworks: false,
            life: false,
            pipes: false,
            plasma: false,
            pause_unfocused: true,
            fps: FPS_DEFAULT,
        }
    }
}

impl AnimationsConfig {
    /// The frame rate to actually drive the tick at, clamped to a sane range so a bad config
    /// value (0, or absurdly high) can't busy-spin or stall the animation loop.
    pub fn effective_fps(&self) -> u16 {
        self.fps.clamp(FPS_MIN, FPS_MAX)
    }

    /// Whether any individual effect is enabled (ignores `master`).
    pub fn any_effect(&self) -> bool {
        self.title
            || self.heart
            || self.seekbar
            || self.spinner
            || self.eq_bars
            || self.controls
            || self.border
            || self.track_intro
            || self.lyrics
            || self.toast
            || self.volume_flash
            || self.like_burst
            || self.seek_flash
            || self.selection
            || self.stagger
            || self.caret
            || self.tabs
            || self.popup_fade
            || self.activity
            || self.about_fx
            || self.time_glow
            || self.progress_sparkle
            || self.border_chase
            || self.pause_flash
            || self.error_shake
            || self.any_canvas()
    }

    /// Whether any filler-canvas effect is enabled — the group `ui::anim::render_canvas`
    /// dispatches, drawn only into blank zones.
    pub fn any_canvas(&self) -> bool {
        self.bounce || self.any_canvas_heavy()
    }

    /// Canvas effects that repaint enough cells per frame to earn the reduced draw-fps cap
    /// and DEC synchronized update: every canvas effect except the single-label `bounce`.
    pub fn any_canvas_heavy(&self) -> bool {
        self.rain
            || self.donut
            || self.visualizer
            || self.starfield
            || self.comets
            || self.snow
            || self.fireflies
            || self.cube
            || self.aquarium
            || self.waves
            || self.fireworks
            || self.life
            || self.pipes
            || self.plasma
    }

    /// Whether animations should actually run: the master switch is on *and* at least one
    /// effect is enabled. When this is `false`, the per-frame animation clock stays asleep.
    pub fn active(&self) -> bool {
        self.master && self.any_effect()
    }

    /// The config as the render/gating layer should see it in the given mode: `master`
    /// resolves to the Radio override while dedicated Radio mode is active (`None` =
    /// inherit). Callers must keep persisting the *stored* config — saving this resolved
    /// copy would bake the inherit link into the file.
    pub fn effective(self, radio: bool) -> Self {
        Self {
            master: if radio {
                self.radio_master.unwrap_or(self.master)
            } else {
                self.master
            },
            ..self
        }
    }
}
