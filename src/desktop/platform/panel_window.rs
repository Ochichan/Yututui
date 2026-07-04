//! Native window wrapper for the mini player WebView.

use std::cell::{Cell, RefCell};
use std::error::Error;
use std::rc::Rc;

use tao::dpi::LogicalSize;
use tao::event_loop::EventLoopWindowTarget;
use tao::window::{Window, WindowBuilder, WindowId};
use wry::{WebView, WebViewBuilder};

use crate::desktop::panel::{self, PanelCommand, PanelTheme};
use crate::desktop::status::PollUpdate;

pub struct MiniPlayerPanel {
    // Torn down on `hide()` and rebuilt on `show()` so a tray that opened the mini player
    // once doesn't keep a whole browser engine (WebView2 is ~5 processes / ~200 MB)
    // resident while idle. `RefCell` keeps `show`/`hide`/`apply_update` on `&self`, so the
    // platform tray code needs no changes. Declared before `window`: the WebView must drop
    // before its host window (wry's documented teardown order; on Windows the WebView2
    // controller must be closed while the HWND is still alive).
    webview: RefCell<Option<WebView>>,
    window: Window,
    window_id: WindowId,
    // Kept so `show()` can rebuild the WebView after `hide()` dropped it.
    on_command: Rc<dyn Fn(PanelCommand)>,
    // Last status seen, used to seed a rebuilt page so a reopened panel shows current
    // state without waiting for the next poll.
    latest: RefCell<PollUpdate>,
    // The active skin; baked into every (re)built page and persisted by the platform
    // loop, so it survives webview teardown and app restarts.
    theme: Cell<PanelTheme>,
    // The minimal theme's ⋯ expansion. Invariant: `false` whenever no webview is
    // alive — a fresh page always renders collapsed, so `hide()` resets it.
    expanded: Cell<bool>,
    // When the panel last became visible; blur-dismiss waits out a short grace
    // after this so focus churn during show (webview first-responder handoff)
    // can't instantly re-hide the panel.
    shown_at: Cell<Option<std::time::Instant>>,
    // Art rides a separate script, never the 2s status payload. Two memos with
    // different lifetimes: `encoded_art` caches the last file read+encoded
    // (path → data URI) and survives webview rebuilds; `page_art` is the artwork
    // path the LIVE page currently shows and resets whenever the webview dies,
    // so a rebuilt (blank) page gets art re-pushed exactly once.
    encoded_art: RefCell<Option<(String, String)>>,
    page_art: RefCell<Option<String>>,
}

impl MiniPlayerPanel {
    pub fn create<T, F>(
        target: &EventLoopWindowTarget<T>,
        initial: &PollUpdate,
        theme: PanelTheme,
        on_command: F,
    ) -> Result<Self, Box<dyn Error>>
    where
        T: 'static,
        F: Fn(PanelCommand) + 'static,
    {
        // The window is frameless and transparent: the page draws each theme's shape
        // (cushion / capsule / egg) with its own shadow, so the margin baked into the
        // per-theme sizes is the shadow's breathing room, not dead space.
        let (width, height) = theme.window_size(false);
        let size = LogicalSize::new(width, height);
        let builder = WindowBuilder::new()
            .with_title("YPlayer Mini Player")
            .with_inner_size(size)
            .with_min_inner_size(size)
            .with_max_inner_size(size)
            .with_resizable(false)
            .with_maximizable(false)
            // Frameless + transparent: the page's rounded cushion IS the window
            // chrome (drag via the header's `drag` IPC, close via its ✕ button).
            .with_decorations(false)
            .with_transparent(true)
            .with_always_on_top(true)
            .with_visible(true);
        // Windows-only: route transparency through a no-redirection-bitmap window so the
        // WebView2 DirectComposition surface alpha-composites onto the desktop. Without it
        // tao falls back to `DwmEnableBlurBehindWindow`, which WebView2 ignores — the
        // "transparent" margin then renders as a solid white backdrop (most visible at the
        // corners the page's drop shadow doesn't cover).
        #[cfg(windows)]
        let builder = {
            use tao::platform::windows::WindowBuilderExtWindows;
            builder.with_no_redirection_bitmap(true)
        };
        let window = builder.build(target)?;
        let window_id = window.id();
        let on_command: Rc<dyn Fn(PanelCommand)> = Rc::new(on_command);
        let desired = desired_art(initial);
        let art_uri = desired
            .as_deref()
            .and_then(|path| panel::load_art_data_uri(std::path::Path::new(path)));
        let webview = build_webview(&window, initial, theme, art_uri.as_deref(), &on_command)?;

        Ok(Self {
            webview: RefCell::new(Some(webview)),
            window,
            window_id,
            on_command,
            latest: RefCell::new(initial.clone()),
            theme: Cell::new(theme),
            expanded: Cell::new(false),
            shown_at: Cell::new(Some(std::time::Instant::now())),
            encoded_art: RefCell::new(match (&desired, &art_uri) {
                (Some(path), Some(uri)) => Some((path.clone(), uri.clone())),
                _ => None,
            }),
            page_art: RefCell::new(desired),
        })
    }

    pub fn window_id(&self) -> WindowId {
        self.window_id
    }

    pub fn show(&self) {
        // Rebuild the WebView if `hide()` tore it down (the first `show()` after `create`
        // reuses the one built there).
        let needs_rebuild = self.webview.borrow().is_none();
        if needs_rebuild {
            let latest = self.latest.borrow().clone();
            let desired = desired_art(&latest);
            let art_uri = desired.as_deref().and_then(|path| self.encode_art(path));
            match build_webview(
                &self.window,
                &latest,
                self.theme.get(),
                art_uri.as_deref(),
                &self.on_command,
            ) {
                Ok(webview) => {
                    *self.webview.borrow_mut() = Some(webview);
                    *self.page_art.borrow_mut() = desired;
                }
                Err(e) => tracing::warn!(
                    target: "ytt_tray",
                    error = %e,
                    "could not rebuild mini player webview"
                ),
            }
        }
        self.window.set_visible(true);
        self.window.set_focus();
        if let Some(webview) = &*self.webview.borrow() {
            let _ = webview.focus();
        }
        self.shown_at.set(Some(std::time::Instant::now()));
    }

    /// Whether a focus loss should dismiss the panel: only the minimal skin
    /// behaves like a click-away tray popup (the cushion and the tamagotchi are
    /// meant to be parked on the desktop), and only after a short post-show grace.
    pub fn wants_blur_hide(&self) -> bool {
        self.theme.get() == PanelTheme::Minimal
            && self
                .shown_at
                .get()
                .is_some_and(|at| at.elapsed() >= std::time::Duration::from_millis(300))
    }

    pub fn hide(&self) {
        self.window.set_visible(false);
        // Drop the WebView so its WebView2 processes (Chromium engine, ~200 MB) exit while
        // the panel is dismissed. The window itself is cheap and stays put; the HWND
        // outlives the controller, matching wry's required teardown order.
        self.webview.borrow_mut().take();
        // A rebuilt page always starts collapsed and artless, so the window must
        // match the former and the art memo the latter.
        *self.page_art.borrow_mut() = None;
        if self.expanded.replace(false) {
            self.apply_window_size();
        }
    }

    /// Switch skins. The live page already flipped its own `data-theme` (it sent the
    /// command); this side resizes the window and keeps the theme for future rebuilds.
    /// Entering any theme starts collapsed — otherwise a minimal(expanded) → other →
    /// minimal round trip would resurrect a stale expanded window size against a
    /// freshly-collapsed DOM.
    pub fn set_theme(&self, theme: PanelTheme) {
        let changed = self.theme.replace(theme) != theme;
        let collapsed = self.expanded.replace(false);
        if changed || collapsed {
            self.apply_window_size();
        }
    }

    /// The minimal theme's ⋯ expansion toggled in the page; grow/shrink the window to
    /// match. Other themes render their extra controls inside their fixed shape.
    pub fn set_expanded(&self, expanded: bool) {
        if self.theme.get() != PanelTheme::Minimal {
            return;
        }
        if self.expanded.replace(expanded) != expanded {
            self.apply_window_size();
        }
    }

    /// Resize to the active (theme, expanded) size and, when visible, reposition so the
    /// window's bottom-center stays fixed: the panel usually sits just above a taskbar
    /// anchor, so growth should unfold upward-on-screen and a smaller theme should stay
    /// hugging the bar instead of floating up-left.
    fn apply_window_size(&self) {
        let (width, height) = self.theme.get().window_size(self.expanded.get());
        // Physical target size; decorations(false) means outer == inner, so this is
        // exact without racing a post-resize read-back.
        let scale = self.window.scale_factor();
        let (new_w, new_h) = (width * scale, height * scale);
        let anchor = if self.window.is_visible() {
            self.window.outer_position().ok().map(|pos| {
                let old = self.window.outer_size();
                (
                    pos.x as f64 + old.width as f64 / 2.0,
                    pos.y as f64 + old.height as f64,
                )
            })
        } else {
            None
        };
        // Drop the min==max cage before resizing, then re-cage at the new size: pushing
        // a new size through a live cage clamps in a platform-dependent order.
        let size = LogicalSize::new(width, height);
        self.window.set_min_inner_size(None::<LogicalSize<f64>>);
        self.window.set_max_inner_size(None::<LogicalSize<f64>>);
        self.window.set_inner_size(size);
        self.window.set_min_inner_size(Some(size));
        self.window.set_max_inner_size(Some(size));
        if let Some((anchor_x, bottom)) = anchor {
            let (mut x, mut y) = anchored_top_left(anchor_x, bottom, new_w, new_h);
            if let Some(monitor) = self.window.current_monitor() {
                let pos = monitor.position();
                let dim = monitor.size();
                (x, y) = clamp_to_rect(
                    (x, y),
                    (new_w, new_h),
                    (pos.x as f64, pos.y as f64),
                    (dim.width as f64, dim.height as f64),
                );
            }
            self.window
                .set_outer_position(tao::dpi::PhysicalPosition::new(x, y));
        }
    }

    /// Begin a native move triggered by a mousedown in the page's header. The IPC
    /// round-trip arrives while the button is still held, which is what the OS
    /// window-drag protocols require.
    pub fn start_drag(&self) {
        if let Err(e) = self.window.drag_window() {
            tracing::debug!(target: "ytt_tray", error = %e, "mini player drag failed");
        }
    }

    /// Place the panel next to a tray-icon click (physical pixels): centered on the
    /// click, preferring above it (bottom taskbars), flipping below for top bars,
    /// and kept inside the monitor that contains the click.
    ///
    /// Windows-only: the notification-area click reports an anchor position. On macOS
    /// the panel opens from a menu item (no click coordinates), so the OS places it.
    #[cfg(windows)]
    pub fn position_near(&self, (anchor_x, anchor_y): (f64, f64)) {
        let size = self.window.outer_size();
        let monitor = self
            .window
            .available_monitors()
            .find(|monitor| {
                let pos = monitor.position();
                let dim = monitor.size();
                anchor_x >= pos.x as f64
                    && anchor_x < pos.x as f64 + dim.width as f64
                    && anchor_y >= pos.y as f64
                    && anchor_y < pos.y as f64 + dim.height as f64
            })
            .or_else(|| self.window.current_monitor());
        let Some(monitor) = monitor else {
            return;
        };
        let mon_pos = monitor.position();
        let mon_size = monitor.size();

        const MARGIN: f64 = 8.0;
        let mut x = anchor_x - size.width as f64 / 2.0;
        let mut y = anchor_y - size.height as f64 - MARGIN;
        if y < mon_pos.y as f64 + MARGIN {
            y = anchor_y + MARGIN;
        }
        // min-then-max (not `clamp`) so a monitor smaller than the panel degrades
        // to the top-left margin instead of panicking on an inverted range.
        x = x
            .min(mon_pos.x as f64 + mon_size.width as f64 - size.width as f64 - MARGIN)
            .max(mon_pos.x as f64 + MARGIN);
        y = y
            .min(mon_pos.y as f64 + mon_size.height as f64 - size.height as f64 - MARGIN)
            .max(mon_pos.y as f64 + MARGIN);
        self.window
            .set_outer_position(tao::dpi::PhysicalPosition::new(x, y));
    }

    pub fn apply_update(&self, update: &PollUpdate) {
        *self.latest.borrow_mut() = update.clone();
        if let Some(webview) = &*self.webview.borrow()
            && let Err(e) = webview.evaluate_script(&panel::update_script(update))
        {
            tracing::warn!(
                target: "ytt_tray",
                error = %e,
                "could not update mini player panel"
            );
        }
        self.apply_art(update);
    }

    /// Push the update's artwork into the live page iff it differs from what the
    /// page already shows (boot art is baked into the page itself, so this only
    /// fires on real changes). An unreadable/oversized file records a `null` push
    /// so the 2s poll doesn't retry the same broken file forever.
    fn apply_art(&self, update: &PollUpdate) {
        let desired = desired_art(update);
        if *self.page_art.borrow() == desired {
            return;
        }
        let Some(webview) = &*self.webview.borrow() else {
            return;
        };
        let uri = desired.as_deref().and_then(|path| self.encode_art(path));
        if let Err(e) = webview.evaluate_script(&panel::art_script(uri.as_deref())) {
            tracing::warn!(
                target: "ytt_tray",
                error = %e,
                "could not update mini player artwork"
            );
        }
        *self.page_art.borrow_mut() = desired;
    }

    /// Read + encode `path`, memoized on the last path so webview rebuilds and
    /// repeated pushes never re-read the same file.
    fn encode_art(&self, path: &str) -> Option<String> {
        let mut memo = self.encoded_art.borrow_mut();
        if let Some((cached_path, cached_uri)) = &*memo
            && cached_path == path
        {
            return Some(cached_uri.clone());
        }
        let uri = panel::load_art_data_uri(std::path::Path::new(path))?;
        *memo = Some((path.to_string(), uri.clone()));
        Some(uri)
    }
}

/// The artwork cache path the update wants shown, if any.
fn desired_art(update: &PollUpdate) -> Option<String> {
    update
        .state
        .status()
        .and_then(|status| status.artwork.as_ref())
        .and_then(|art| art.path.clone())
}

/// Top-left that keeps an old rect's bottom-center fixed across a resize
/// (all physical px).
fn anchored_top_left(anchor_x: f64, bottom: f64, new_w: f64, new_h: f64) -> (f64, f64) {
    (anchor_x - new_w / 2.0, bottom - new_h)
}

/// 8px-margin min-then-max clamp onto a monitor rect. Min-then-max (not `clamp`) so a
/// monitor smaller than the panel degrades to the top-left margin instead of panicking
/// on an inverted range — same convention as `position_near` and `clamp_to_monitors`.
fn clamp_to_rect(
    (x, y): (f64, f64),
    (w, h): (f64, f64),
    (mon_x, mon_y): (f64, f64),
    (mon_w, mon_h): (f64, f64),
) -> (f64, f64) {
    const MARGIN: f64 = 8.0;
    (
        x.min(mon_x + mon_w - w - MARGIN).max(mon_x + MARGIN),
        y.min(mon_y + mon_h - h - MARGIN).max(mon_y + MARGIN),
    )
}

/// Build the mini player WebView on `window`, seeded with `update`, the active
/// theme, and the boot artwork. Shared by `create` and by `show()` when it rebuilds
/// the view after `hide()` tore it down.
fn build_webview(
    window: &Window,
    update: &PollUpdate,
    theme: PanelTheme,
    art_uri: Option<&str>,
    on_command: &Rc<dyn Fn(PanelCommand)>,
) -> Result<WebView, Box<dyn Error>> {
    let on_command = Rc::clone(on_command);
    // Windows: share the WebView2 user-data folder with the main window (docs/gui/03 §3)
    // so both surfaces cost one browser-process set. macOS ignores the web context.
    #[cfg(windows)]
    let mut web_context = crate::desktop::platform::shared_web_context();
    #[cfg(windows)]
    let builder = WebViewBuilder::new_with_web_context(&mut web_context);
    #[cfg(not(windows))]
    let builder = WebViewBuilder::new();
    let webview = builder
        .with_transparent(true)
        .with_html(panel::html(update, theme, art_uri))
        .with_ipc_handler(
            move |request| match panel::parse_ipc_message(request.body()) {
                Ok(command) => (*on_command)(command),
                Err(e) => {
                    tracing::warn!(
                        target: "ytt_tray",
                        message = request.body().as_str(),
                        error = %e,
                        "ignored invalid mini player IPC message"
                    );
                }
            },
        )
        .build(window)?;
    Ok(webview)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchored_top_left_keeps_bottom_center_fixed() {
        // Old rect: top-left (100, 100), 306×90 → bottom-center (253, 190).
        let (anchor_x, bottom) = (100.0 + 306.0 / 2.0, 100.0 + 90.0);
        // Growing to 306×276 unfolds upward: same center, top moves up.
        let (x, y) = anchored_top_left(anchor_x, bottom, 306.0, 276.0);
        assert_eq!((x, y), (100.0, 190.0 - 276.0));
        // Shrinking 398×602 → 306×90 keeps hugging the old bottom edge.
        let (anchor_x, bottom) = (50.0 + 398.0 / 2.0, 40.0 + 602.0);
        let (x, y) = anchored_top_left(anchor_x, bottom, 306.0, 90.0);
        assert_eq!(x, 50.0 + (398.0 - 306.0) / 2.0);
        assert_eq!(y, 642.0 - 90.0);
    }

    #[test]
    fn clamp_keeps_rect_inside_monitor_with_margin() {
        // Off the top edge (expansion grew past the top) → pinned to the margin.
        let (x, y) = clamp_to_rect((100.0, -86.0), (306.0, 276.0), (0.0, 0.0), (1920.0, 1080.0));
        assert_eq!((x, y), (100.0, 8.0));
        // Off the right/bottom → pulled back inside.
        let (x, y) = clamp_to_rect(
            (1900.0, 1060.0),
            (306.0, 276.0),
            (0.0, 0.0),
            (1920.0, 1080.0),
        );
        assert_eq!((x, y), (1920.0 - 306.0 - 8.0, 1080.0 - 276.0 - 8.0));
    }

    #[test]
    fn clamp_degrades_gracefully_on_a_tiny_monitor() {
        // Monitor smaller than the panel: min-then-max lands on the top-left margin
        // instead of panicking on an inverted range.
        let (x, y) = clamp_to_rect((500.0, 500.0), (306.0, 276.0), (0.0, 0.0), (200.0, 150.0));
        assert_eq!((x, y), (8.0, 8.0));
    }
}
