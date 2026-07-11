//! Native window wrapper for the mini player WebView.

use std::cell::{Cell, RefCell};
use std::error::Error;
use std::rc::Rc;

use tao::dpi::{LogicalSize, PhysicalSize};
use tao::event_loop::EventLoopWindowTarget;
use tao::window::{Window, WindowBuilder, WindowId};
use wry::{NewWindowResponse, WebView, WebViewBuilder};

use crate::desktop::panel::{
    self, DesktopCommandError, PanelRequest, PanelSheet, PanelTheme, PanelUiSnapshot,
};
use crate::desktop::status::PollUpdate;
use crate::desktop::window_state::{
    DesktopState, MonitorDescriptor, MonitorRect, Size, WindowPlacement, WindowRect,
    reconcile_physical_rect, resolve_legacy_rect,
};

pub struct MiniPlayerPanel {
    // Torn down after the hide grace and rebuilt on `show()` so a tray that opened the mini
    // player once doesn't keep a whole browser engine (WebView2 is ~5 processes / ~200 MB)
    // resident while idle. `RefCell` keeps the lifecycle on `&self`, so the
    // platform tray code needs no changes. Declared before `window`: the WebView must drop
    // before its host window (wry's documented teardown order; on Windows the WebView2
    // controller must be closed while the HWND is still alive).
    webview: RefCell<Option<WebView>>,
    window: Window,
    window_id: WindowId,
    // Kept so `show()` can rebuild the WebView after `hide()` dropped it.
    on_command: Rc<dyn Fn(u64, PanelRequest)>,
    // Last status seen, used to seed a rebuilt page so a reopened panel shows current
    // state without waiting for the next poll.
    latest: RefCell<PollUpdate>,
    // The active skin; baked into every (re)built page and persisted by the platform
    // loop, so it survives webview teardown and app restarts.
    theme: Cell<PanelTheme>,
    // Window behavior is explicit and independent of the visual skin.
    pinned: Cell<bool>,
    // Host-owned transient page state. These small values survive an idle WebView teardown;
    // authoritative theme, pin, placement, and player state remain separate.
    expanded: Cell<bool>,
    shared_sheet: Cell<Option<PanelSheet>>,
    ui_snapshot: RefCell<PanelUiSnapshot>,
    // Art rides a separate script, never the 2s status payload. Two memos with
    // different lifetimes: `encoded_art` caches the last file read+encoded
    // (path → data URI) and survives webview rebuilds; `page_art` is the artwork
    // path the LIVE page currently shows and resets whenever the webview dies,
    // so a rebuilt (blank) page gets art re-pushed exactly once.
    encoded_art: RefCell<Option<(String, String)>>,
    page_art: RefCell<Option<PageArtKey>>,
    art_failures: RefCell<Option<(PageArtKey, u8)>>,
    // Visibility is gated by the current page generation's FrontendReady IPC.
    frontend_ready: Cell<bool>,
    page_generation: Cell<u64>,
    // Work-area edge that owns the transient tray anchor. Expansion keeps this edge fixed so a
    // top/side taskbar panel grows into the screen instead of always growing upward.
    transient_edge: Cell<Option<AnchorEdge>>,
    // Native move/resize notifications are asynchronous. Remember the rect requested by the
    // host so those notifications are not mistaken for a user's drag and persisted as a pinned
    // placement. A real user move differs from this rect and clears the guard immediately.
    programmatic_geometry: Cell<Option<ProgrammaticGeometry>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProgrammaticGeometry {
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    remaining_events: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AnchorEdge {
    Top,
    Bottom,
    Left,
    Right,
}

/// Artwork delivery is scoped to the player identity, not just the cache path. Two tracks can
/// legitimately share one cover file; the page clears old pixels immediately on identity change,
/// so the host must re-deliver even when that path is unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PageArtKey {
    track_identity: String,
    path: Option<String>,
}

impl MiniPlayerPanel {
    pub fn create<T, F>(
        target: &EventLoopWindowTarget<T>,
        initial: &PollUpdate,
        theme: PanelTheme,
        pinned: bool,
        on_command: F,
    ) -> Result<Self, Box<dyn Error>>
    where
        T: 'static,
        F: Fn(u64, PanelRequest) + 'static,
    {
        // The window is frameless and transparent: the page draws each theme's shape
        // (cushion / capsule / egg) with its own shadow, so the margin baked into the
        // per-theme sizes is the shadow's breathing room, not dead space.
        let (width, height) = theme.window_size(false);
        let size = LogicalSize::new(width, height);
        let builder = WindowBuilder::new()
            .with_title("YuTuTray! Mini Player")
            .with_inner_size(size)
            .with_min_inner_size(size)
            .with_max_inner_size(size)
            .with_resizable(false)
            .with_maximizable(false)
            // Frameless + transparent: the page's rounded cushion IS the window
            // chrome (drag via the header's `drag` IPC, close via its ✕ button).
            .with_decorations(false)
            .with_transparent(true)
            .with_always_on_top(pinned)
            // Apply native tool-window role, placement, and WebView before the
            // first show so no default-position rectangle flashes on screen.
            .with_focused(false)
            .with_visible(false);
        // Windows-only: route transparency through a no-redirection-bitmap window so the
        // WebView2 DirectComposition surface alpha-composites onto the desktop. Without it
        // tao falls back to `DwmEnableBlurBehindWindow`, which WebView2 ignores — the
        // "transparent" margin then renders as a solid white backdrop (most visible at the
        // corners the page's drop shadow doesn't cover).
        #[cfg(windows)]
        let builder = {
            use tao::platform::windows::WindowBuilderExtWindows;
            builder
                .with_no_redirection_bitmap(true)
                .with_skip_taskbar(true)
        };
        let window = builder.build(target)?;
        #[cfg(windows)]
        crate::desktop::platform::windows::configure_mini_window(&window)?;
        // macOS-only: the page draws its own drop shadow, so suppress the system
        // window shadow. Otherwise macOS casts it around the fixed rectangular window
        // bounds (not the transparent panel shape), reappearing as a boxy frame.
        #[cfg(target_os = "macos")]
        {
            use tao::platform::macos::WindowExtMacOS;
            window.set_has_shadow(false);
        }
        let window_id = window.id();
        let page_generation = super::next_webview_generation();
        let on_command: Rc<dyn Fn(u64, PanelRequest)> = Rc::new(on_command);
        let art_key = desired_art_key(initial);
        let desired = art_key.path.clone();
        let art_uri = desired
            .as_deref()
            .and_then(|path| panel::load_art_data_uri(std::path::Path::new(path)));
        let ui_snapshot = PanelUiSnapshot::default();
        let webview = build_webview(
            &window,
            PanelWebViewState {
                update: initial,
                theme,
                pinned,
                expanded: false,
                shared_sheet: None,
                ui_snapshot: &ui_snapshot,
                art_uri: art_uri.as_deref(),
            },
            page_generation,
            &on_command,
        )?;

        let panel = Self {
            webview: RefCell::new(Some(webview)),
            window,
            window_id,
            on_command,
            latest: RefCell::new(initial.clone()),
            theme: Cell::new(theme),
            pinned: Cell::new(pinned),
            expanded: Cell::new(false),
            shared_sheet: Cell::new(None),
            ui_snapshot: RefCell::new(ui_snapshot),
            encoded_art: RefCell::new(match (&desired, &art_uri) {
                (Some(path), Some(uri)) => Some((path.clone(), uri.clone())),
                _ => None,
            }),
            // A failed read rendered the placeholder, not the requested file. Keep the key
            // unset so the next authoritative snapshot retries instead of memoizing failure.
            page_art: RefCell::new(if desired.is_none() || art_uri.is_some() {
                Some(art_key)
            } else {
                None
            }),
            art_failures: RefCell::new(None),
            frontend_ready: Cell::new(false),
            page_generation: Cell::new(page_generation),
            transient_edge: Cell::new(None),
            programmatic_geometry: Cell::new(None),
        };
        if pinned {
            panel.restore_saved_position();
        }
        Ok(panel)
    }

    pub fn window_id(&self) -> WindowId {
        self.window_id
    }

    /// Ensure the hidden native host has a live WebView without making it visible. The page's
    /// generation-scoped `FrontendReady` handshake is the only path that may later call `show`.
    pub fn ensure_surface(&self) -> bool {
        self.reconcile_live_placement();
        // Rebuild the WebView if the grace teardown released it (the first ensure after `create`
        // reuses the one built there).
        let needs_rebuild = self.webview.borrow().is_none();
        if needs_rebuild {
            let next_generation = super::next_webview_generation();
            let latest = self.latest.borrow().clone();
            let art_key = desired_art_key(&latest);
            let desired = art_key.path.clone();
            let art_uri = desired.as_deref().and_then(|path| self.encode_art(path));
            let ui_snapshot = self.ui_snapshot.borrow();
            match build_webview(
                &self.window,
                PanelWebViewState {
                    update: &latest,
                    theme: self.theme.get(),
                    pinned: self.pinned.get(),
                    expanded: self.expanded.get(),
                    shared_sheet: self.shared_sheet.get(),
                    ui_snapshot: &ui_snapshot,
                    art_uri: art_uri.as_deref(),
                },
                next_generation,
                &self.on_command,
            ) {
                Ok(webview) => {
                    *self.webview.borrow_mut() = Some(webview);
                    self.page_generation.set(next_generation);
                    self.frontend_ready.set(false);
                    *self.page_art.borrow_mut() = if desired.is_none() || art_uri.is_some() {
                        Some(art_key)
                    } else {
                        None
                    };
                }
                Err(e) => {
                    crate::desktop::native_error::show(
                        "YuTuTui! Mini Player",
                        &format!("Could not restore the mini player: {e}"),
                    );
                    tracing::warn!(
                        target: "ytt_tray",
                        error = %e,
                        "could not rebuild mini player webview"
                    );
                }
            }
        }
        if self.webview.borrow().is_none() {
            self.window.set_visible(false);
            return false;
        }
        true
    }

    pub fn show(&self) -> bool {
        if !self.ensure_surface() {
            return false;
        }
        if !self.frontend_ready.get() {
            self.window.set_visible(false);
            return false;
        }
        self.window.set_visible(true);
        self.window.set_focus();
        if let Some(webview) = &*self.webview.borrow() {
            let _ = webview.focus();
            if let Err(error) =
                webview.evaluate_script("window.ytmTuiFocusPrimary && window.ytmTuiFocusPrimary();")
            {
                tracing::debug!(target: "ytt_tray", %error, "could not focus mini player primary control");
            }
        }
        true
    }

    /// Consume the native event generated by a host-requested resize/re-anchor. Returns `false`
    /// as soon as the current rectangle diverges, which is how an actual drag becomes persistable.
    pub fn consume_programmatic_geometry_event(&self) -> bool {
        let Some(expected) = self.programmatic_geometry.get() else {
            return false;
        };
        let Ok(position) = self.window.outer_position() else {
            self.programmatic_geometry.set(None);
            return false;
        };
        let size = self.window.outer_size();
        if !programmatic_rect_matches(expected, position.x, position.y, size.width, size.height) {
            self.programmatic_geometry.set(None);
            return false;
        }
        self.programmatic_geometry.set(
            expected
                .remaining_events
                .checked_sub(1)
                .filter(|remaining| *remaining > 0)
                .map(|remaining_events| ProgrammaticGeometry {
                    remaining_events,
                    ..expected
                }),
        );
        true
    }

    pub fn mark_frontend_ready(&self) {
        self.frontend_ready.set(true);
    }

    pub fn page_generation(&self) -> u64 {
        self.page_generation.get()
    }

    pub fn apply_command_result(&self, id: u64, error: Option<&DesktopCommandError>) {
        if let Some(webview) = &*self.webview.borrow()
            && let Err(error) = webview.evaluate_script(&panel::command_result_script(id, error))
        {
            tracing::debug!(target: "ytt_tray", %error, "could not deliver panel command result");
        }
    }

    pub fn is_pinned(&self) -> bool {
        self.pinned.get()
    }

    pub fn set_pinned(&self, pinned: bool) {
        if self.pinned.replace(pinned) != pinned {
            self.window.set_always_on_top(pinned);
        }
    }

    pub fn hide(&self) {
        self.window.set_visible(false);
    }

    /// Release the browser engine after the event-loop-owned idle grace. Keeping
    /// hiding separate from teardown makes a quick reopen instant and lets the
    /// persisted `keep_webview_alive` policy opt out entirely.
    pub fn teardown_webview(&self) {
        // Drop the WebView so its WebView2 processes (Chromium engine, ~200 MB) exit while
        // the panel is dismissed. The window itself is cheap and stays put; the HWND
        // outlives the controller, matching wry's required teardown order.
        self.webview.borrow_mut().take();
        self.frontend_ready.set(false);
        // Artwork is page-local, but the small host-owned UI snapshot survives a teardown.
        // This keeps a pinned panel's expanded/sheet state stable across the memory-saving
        // WebView grace instead of surprising the user on reopen.
        *self.page_art.borrow_mut() = None;
    }

    pub fn placement(&self) -> Option<WindowPlacement> {
        let monitor = descriptor_for_window(&self.window)?;
        let rect = monitor.to_logical(current_physical_rect(&self.window)?);
        Some(WindowPlacement::capture(rect, &monitor))
    }

    /// Reconcile a long-lived native host after display removal, work-area changes, or a DPI
    /// transition. The fixed mini-player size is expressed in DIP but applied in the selected
    /// monitor's physical pixels, so a hidden host created on the primary cannot size a panel
    /// incorrectly when it later opens on a 150/200% tray display.
    pub fn reconcile_live_placement(&self) -> bool {
        let monitors = monitor_descriptors(&self.window);
        let Some(current) = current_physical_rect(&self.window) else {
            return false;
        };
        let Some(reconciled) =
            reconcile_physical_rect(current, self.logical_window_size(), &monitors, 8)
        else {
            return false;
        };
        if programmatic_rect_matches(
            ProgrammaticGeometry {
                x: current.x,
                y: current.y,
                w: current.w,
                h: current.h,
                remaining_events: 1,
            },
            reconciled.rect.x,
            reconciled.rect.y,
            reconciled.rect.w,
            reconciled.rect.h,
        ) {
            return false;
        }
        self.apply_physical_geometry(reconciled.rect);
        true
    }

    fn logical_window_size(&self) -> Size {
        let (w, h) = if self.shared_sheet.get().is_some() {
            PanelTheme::Default.window_size(false)
        } else {
            self.theme.get().window_size(self.expanded.get())
        };
        Size {
            w: w.round().max(1.0) as u32,
            h: h.round().max(1.0) as u32,
        }
    }

    fn apply_physical_geometry(&self, rect: WindowRect) {
        self.set_fixed_physical_size(PhysicalSize::new(rect.w, rect.h));
        self.window
            .set_outer_position(tao::dpi::PhysicalPosition::new(rect.x, rect.y));
        self.remember_programmatic_rect(rect.x, rect.y, rect.w, rect.h);
    }

    fn set_fixed_physical_size(&self, size: PhysicalSize<u32>) {
        self.window.set_min_inner_size(None::<PhysicalSize<u32>>);
        self.window.set_max_inner_size(None::<PhysicalSize<u32>>);
        self.window.set_inner_size(size);
        self.window.set_min_inner_size(Some(size));
        self.window.set_max_inner_size(Some(size));
    }

    fn desired_physical_size(&self, scale: f64) -> PhysicalSize<u32> {
        let logical = self.logical_window_size();
        let scale = if scale.is_finite() && scale > 0.0 {
            scale
        } else {
            1.0
        };
        PhysicalSize::new(
            (f64::from(logical.w) * scale).round().max(1.0) as u32,
            (f64::from(logical.h) * scale).round().max(1.0) as u32,
        )
    }

    fn restore_saved_position(&self) {
        let state = DesktopState::load();
        let monitors = monitor_descriptors(&self.window);
        let logical_size = self.logical_window_size();
        let resolved = state
            .placement_v2
            .mini
            .as_ref()
            .and_then(|placement| placement.resolve(&monitors))
            .or_else(|| {
                let point = state.mini?;
                resolve_legacy_rect(
                    WindowRect {
                        x: point.x,
                        y: point.y,
                        w: logical_size.w,
                        h: logical_size.h,
                        maximized: false,
                    },
                    &monitors,
                )
            });
        if let Some(resolved) = resolved {
            let physical = monitors[resolved.monitor_index].to_physical(resolved.rect);
            if let Some(reconciled) = reconcile_physical_rect(physical, logical_size, &monitors, 8)
            {
                self.apply_physical_geometry(reconciled.rect);
            }
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
        let sheet_closed = self.shared_sheet.replace(None).is_some();
        if changed || collapsed || sheet_closed {
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

    pub fn set_shared_sheet(&self, sheet: Option<PanelSheet>) {
        if self.shared_sheet.replace(sheet) != sheet {
            self.apply_window_size();
        }
    }

    pub fn persist_ui(&self, snapshot: PanelUiSnapshot) {
        *self.ui_snapshot.borrow_mut() = snapshot;
    }

    /// Resize to the active (theme, expanded) size and, when visible, reposition so the
    /// window's bottom-center stays fixed: the panel usually sits just above a taskbar
    /// anchor, so growth should unfold upward-on-screen and a smaller theme should stay
    /// hugging the bar instead of floating up-left.
    fn apply_window_size(&self) {
        // Physical target size; decorations(false) means outer == inner, so this is
        // exact without racing a post-resize read-back.
        let scale = self.window.scale_factor();
        let physical_size = self.desired_physical_size(scale);
        let (new_w, new_h) = (
            f64::from(physical_size.width),
            f64::from(physical_size.height),
        );
        let anchor = if self.window.is_visible() {
            self.window.outer_position().ok().map(|pos| {
                let old = self.window.outer_size();
                let center_x = pos.x as f64 + old.width as f64 / 2.0;
                if self.pinned.get() {
                    (
                        center_x,
                        pos.y as f64 + old.height as f64 / 2.0,
                        true,
                        AnchorEdge::Bottom,
                    )
                } else {
                    let edge = self.transient_edge.get().unwrap_or(AnchorEdge::Bottom);
                    match edge {
                        AnchorEdge::Top => (center_x, pos.y as f64, false, edge),
                        AnchorEdge::Bottom => {
                            (center_x, pos.y as f64 + old.height as f64, false, edge)
                        }
                        AnchorEdge::Left => (
                            pos.x as f64,
                            pos.y as f64 + old.height as f64 / 2.0,
                            false,
                            edge,
                        ),
                        AnchorEdge::Right => (
                            pos.x as f64 + old.width as f64,
                            pos.y as f64 + old.height as f64 / 2.0,
                            false,
                            edge,
                        ),
                    }
                }
            })
        } else {
            None
        };
        // Drop the min==max cage before resizing, then re-cage at the new size: pushing
        // a new size through a live cage clamps in a platform-dependent order.
        self.set_fixed_physical_size(physical_size);
        if let Some((anchor_x, anchor_y, centered, edge)) = anchor {
            let (mut x, mut y) = if centered {
                (anchor_x - new_w / 2.0, anchor_y - new_h / 2.0)
            } else {
                anchored_to_edge(edge, anchor_x, anchor_y, new_w, new_h)
            };
            #[cfg(windows)]
            let work_area =
                crate::desktop::platform::windows::work_area_for_point((anchor_x, anchor_y));
            #[cfg(target_os = "macos")]
            let work_area =
                crate::desktop::platform::macos::work_area_for_point((anchor_x, anchor_y));
            #[cfg(all(not(windows), not(target_os = "macos")))]
            let work_area: Option<MonitorRect> = None;
            if let Some(area) = work_area {
                (x, y) = clamp_to_rect(
                    (x, y),
                    (new_w, new_h),
                    (area.x as f64, area.y as f64),
                    (area.w as f64, area.h as f64),
                    8.0 * scale,
                );
            } else if let Some(monitor) = self.window.current_monitor() {
                let pos = monitor.position();
                let dim = monitor.size();
                (x, y) = clamp_to_rect(
                    (x, y),
                    (new_w, new_h),
                    (pos.x as f64, pos.y as f64),
                    (dim.width as f64, dim.height as f64),
                    8.0 * scale,
                );
            }
            self.window
                .set_outer_position(tao::dpi::PhysicalPosition::new(x, y));
            self.remember_programmatic_rect(
                x.round() as i32,
                y.round() as i32,
                new_w.round().max(1.0) as u32,
                new_h.round().max(1.0) as u32,
            );
        } else if let Ok(position) = self.window.outer_position() {
            self.remember_programmatic_rect(
                position.x,
                position.y,
                new_w.round().max(1.0) as u32,
                new_h.round().max(1.0) as u32,
            );
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
    /// Windows passes the notification-area click directly; macOS samples the
    /// cursor after the native menu item is selected.
    pub fn position_near(&self, (anchor_x, anchor_y): (f64, f64)) {
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
        let size = self.desired_physical_size(monitor.scale_factor());
        self.set_fixed_physical_size(size);
        let mon_pos = monitor.position();
        let mon_size = monitor.size();
        #[cfg(windows)]
        let area = crate::desktop::platform::windows::work_area_for_point((anchor_x, anchor_y))
            .unwrap_or(MonitorRect {
                x: mon_pos.x,
                y: mon_pos.y,
                w: mon_size.width,
                h: mon_size.height,
            });
        #[cfg(target_os = "macos")]
        let area = crate::desktop::platform::macos::work_area_for_point((anchor_x, anchor_y))
            .unwrap_or(MonitorRect {
                x: mon_pos.x,
                y: mon_pos.y,
                w: mon_size.width,
                h: mon_size.height,
            });
        #[cfg(all(not(windows), not(target_os = "macos")))]
        let area = MonitorRect {
            x: mon_pos.x,
            y: mon_pos.y,
            w: mon_size.width,
            h: mon_size.height,
        };

        let (x, y) = position_for_anchor(
            (anchor_x, anchor_y),
            (size.width as f64, size.height as f64),
            area,
            8.0 * monitor.scale_factor(),
        );
        self.transient_edge
            .set(Some(nearest_anchor_edge((anchor_x, anchor_y), area)));
        self.window
            .set_outer_position(tao::dpi::PhysicalPosition::new(x, y));
        self.remember_programmatic_rect(
            x.round() as i32,
            y.round() as i32,
            size.width,
            size.height,
        );
    }

    #[cfg(target_os = "macos")]
    pub fn position_near_cursor(&self) {
        if let Ok(position) = self.window.cursor_position() {
            self.position_near((position.x, position.y));
        }
    }

    pub fn apply_update(&self, update: &PollUpdate) {
        *self.latest.borrow_mut() = update.clone();
        if self.frontend_ready.get()
            && let Some(webview) = &*self.webview.borrow()
            && let Err(e) = webview.evaluate_script(&panel::update_script(update))
        {
            tracing::warn!(
                target: "ytt_tray",
                error = %e,
                "could not update mini player panel"
            );
        }
        if self.frontend_ready.get() {
            self.apply_art(update);
        }
    }

    /// Push the update's artwork into the live page iff it differs from what the
    /// page already shows (boot art is baked into the page itself, so this only
    /// fires on real changes). An unreadable/oversized file pushes `null` to clear stale
    /// artwork but is not memoized as success, so a later snapshot can retry it.
    fn apply_art(&self, update: &PollUpdate) {
        let art_key = desired_art_key(update);
        let desired = art_key.path.clone();
        {
            let mut failures = self.art_failures.borrow_mut();
            if failures
                .as_ref()
                .is_some_and(|(failed, _)| failed != &art_key)
            {
                *failures = None;
            }
            if failures
                .as_ref()
                .is_some_and(|(failed, count)| failed == &art_key && *count >= 2)
            {
                return;
            }
        }
        if self.page_art.borrow().as_ref() == Some(&art_key) {
            return;
        }
        let Some(webview) = &*self.webview.borrow() else {
            return;
        };
        let uri = desired.as_deref().and_then(|path| self.encode_art(path));
        match webview.evaluate_script(&panel::art_script(uri.as_deref())) {
            Ok(()) if desired.is_none() || uri.is_some() => {
                *self.page_art.borrow_mut() = Some(art_key);
            }
            Ok(()) => {}
            Err(e) => {
                tracing::warn!(
                    target: "ytt_tray",
                    error = %e,
                    "could not update mini player artwork"
                );
            }
        }
    }

    /// Invalidate the optimistic script-delivery memo after the browser reports a decode error.
    /// Retry at most once for a path; a track/artwork change clears the failure budget.
    pub fn artwork_failed(&self) {
        let art_key = desired_art_key(&self.latest.borrow());
        let Some(path) = art_key.path.clone() else {
            return;
        };
        let count = {
            let mut failures = self.art_failures.borrow_mut();
            let next = failures
                .as_ref()
                .filter(|(failed, _)| failed == &art_key)
                .map_or(1, |(_, count)| count.saturating_add(1));
            *failures = Some((art_key, next));
            next
        };
        *self.page_art.borrow_mut() = None;
        if self
            .encoded_art
            .borrow()
            .as_ref()
            .is_some_and(|(cached_path, _)| cached_path == &path)
        {
            *self.encoded_art.borrow_mut() = None;
        }
        if count < 2 {
            let latest = self.latest.borrow().clone();
            self.apply_art(&latest);
        }
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

    fn remember_programmatic_rect(&self, x: i32, y: i32, w: u32, h: u32) {
        self.programmatic_geometry.set(Some(ProgrammaticGeometry {
            x,
            y,
            w,
            h,
            // tao may report a size, move, and scale notification for one operation.
            remaining_events: 3,
        }));
    }
}

fn programmatic_rect_matches(
    expected: ProgrammaticGeometry,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
) -> bool {
    const NATIVE_ROUNDING_TOLERANCE: i64 = 2;
    (i64::from(expected.x) - i64::from(x)).abs() <= NATIVE_ROUNDING_TOLERANCE
        && (i64::from(expected.y) - i64::from(y)).abs() <= NATIVE_ROUNDING_TOLERANCE
        && (i64::from(expected.w) - i64::from(w)).abs() <= NATIVE_ROUNDING_TOLERANCE
        && (i64::from(expected.h) - i64::from(h)).abs() <= NATIVE_ROUNDING_TOLERANCE
}

/// The artwork cache path the update wants shown, if any.
fn desired_art(update: &PollUpdate) -> Option<String> {
    update
        .state
        .status()
        .and_then(|status| status.artwork.as_ref())
        .and_then(|art| art.path.clone())
}

fn desired_art_key(update: &PollUpdate) -> PageArtKey {
    PageArtKey {
        track_identity: panel::track_identity(&update.state),
        path: desired_art(update),
    }
}

/// Top-left that keeps an old rect's bottom-center fixed across a resize
/// (all physical px).
fn anchored_top_left(anchor_x: f64, bottom: f64, new_w: f64, new_h: f64) -> (f64, f64) {
    (anchor_x - new_w / 2.0, bottom - new_h)
}

fn anchored_to_edge(
    edge: AnchorEdge,
    anchor_x: f64,
    anchor_y: f64,
    new_w: f64,
    new_h: f64,
) -> (f64, f64) {
    match edge {
        AnchorEdge::Top => (anchor_x - new_w / 2.0, anchor_y),
        AnchorEdge::Bottom => anchored_top_left(anchor_x, anchor_y, new_w, new_h),
        AnchorEdge::Left => (anchor_x, anchor_y - new_h / 2.0),
        AnchorEdge::Right => (anchor_x - new_w, anchor_y - new_h / 2.0),
    }
}

fn nearest_anchor_edge(anchor: (f64, f64), area: MonitorRect) -> AnchorEdge {
    let left = area.x as f64;
    let top = area.y as f64;
    let right = left + area.w as f64;
    let bottom = top + area.h as f64;
    [
        (AnchorEdge::Top, (anchor.1 - top).abs()),
        (AnchorEdge::Bottom, (anchor.1 - bottom).abs()),
        (AnchorEdge::Left, (anchor.0 - left).abs()),
        (AnchorEdge::Right, (anchor.0 - right).abs()),
    ]
    .into_iter()
    .min_by(|(_, a), (_, b)| a.total_cmp(b))
    .map_or(AnchorEdge::Bottom, |(edge, _)| edge)
}

/// Pick the first fully-visible tray-relative candidate, favoring the direction that points
/// into the work area from the nearest taskbar/menu-bar edge. When no candidate fits (for
/// example a panel larger than the work area), clamp the preferred candidate with an 8 DIP
/// inset instead of producing an off-screen or inverted rectangle.
fn position_for_anchor(
    anchor: (f64, f64),
    size: (f64, f64),
    area: MonitorRect,
    margin: f64,
) -> (f64, f64) {
    let (anchor_x, anchor_y) = anchor;
    let (width, height) = size;
    let left = area.x as f64;
    let top = area.y as f64;
    let right = left + area.w as f64;
    let bottom = top + area.h as f64;

    // Candidates point away from the tray anchor: above, below, left, right.
    let candidates = [
        (anchor_x - width / 2.0, anchor_y - height - margin),
        (anchor_x - width / 2.0, anchor_y + margin),
        (anchor_x - width - margin, anchor_y - height / 2.0),
        (anchor_x + margin, anchor_y - height / 2.0),
    ];
    // Nearest top edge → below (1), bottom → above (0), left → right (3), right → left (2).
    let preferred = match nearest_anchor_edge(anchor, area) {
        AnchorEdge::Top => 1,
        AnchorEdge::Bottom => 0,
        AnchorEdge::Left => 3,
        AnchorEdge::Right => 2,
    };
    let order = [preferred, 0, 1, 2, 3];

    for index in order {
        let (x, y) = candidates[index];
        if x >= left + margin
            && y >= top + margin
            && x + width <= right - margin
            && y + height <= bottom - margin
        {
            return (x, y);
        }
    }

    clamp_to_rect(
        candidates[preferred],
        size,
        (left, top),
        (area.w as f64, area.h as f64),
        margin,
    )
}

/// Margin-aware min-then-max clamp onto a monitor rect. Min-then-max (not `clamp`) so a
/// monitor smaller than the panel degrades to the top-left margin instead of panicking
/// on an inverted range — same convention as `position_near` and `clamp_to_monitors`.
fn clamp_to_rect(
    (x, y): (f64, f64),
    (w, h): (f64, f64),
    (mon_x, mon_y): (f64, f64),
    (mon_w, mon_h): (f64, f64),
    margin: f64,
) -> (f64, f64) {
    (
        x.min(mon_x + mon_w - w - margin).max(mon_x + margin),
        y.min(mon_y + mon_h - h - margin).max(mon_y + margin),
    )
}

fn current_physical_rect(window: &Window) -> Option<WindowRect> {
    let position = window.outer_position().ok()?;
    let size = window.outer_size();
    Some(WindowRect {
        x: position.x,
        y: position.y,
        w: size.width,
        h: size.height,
        maximized: false,
    })
}

fn descriptor_for_window(window: &Window) -> Option<MonitorDescriptor> {
    window
        .current_monitor()
        .map(|monitor| monitor_descriptor(&monitor))
}

fn monitor_descriptors(window: &Window) -> Vec<MonitorDescriptor> {
    let primary = window.primary_monitor();
    let mut handles = Vec::new();
    if let Some(primary) = primary {
        handles.push(primary);
    }
    for monitor in window.available_monitors() {
        if !handles.contains(&monitor) {
            handles.push(monitor);
        }
    }
    handles.iter().map(monitor_descriptor).collect()
}

fn monitor_descriptor(monitor: &tao::monitor::MonitorHandle) -> MonitorDescriptor {
    let scale = monitor.scale_factor();
    let pos = monitor.position();
    let size = monitor.size();
    #[cfg(windows)]
    let physical = crate::desktop::platform::windows::work_area_for_point((
        pos.x as f64 + size.width as f64 / 2.0,
        pos.y as f64 + size.height as f64 / 2.0,
    ))
    .unwrap_or(MonitorRect {
        x: pos.x,
        y: pos.y,
        w: size.width,
        h: size.height,
    });
    #[cfg(target_os = "macos")]
    let physical = crate::desktop::platform::macos::work_area_for_point((
        pos.x as f64 + size.width as f64 / 2.0,
        pos.y as f64 + size.height as f64 / 2.0,
    ))
    .unwrap_or(MonitorRect {
        x: pos.x,
        y: pos.y,
        w: size.width,
        h: size.height,
    });
    MonitorDescriptor {
        key: monitor.name(),
        work_area: MonitorRect {
            x: (physical.x as f64 / scale).round() as i32,
            y: (physical.y as f64 / scale).round() as i32,
            w: (physical.w as f64 / scale).round() as u32,
            h: (physical.h as f64 / scale).round() as u32,
        },
        physical_work_area: physical,
        scale_factor: scale,
    }
}

/// Build the mini player WebView on `window`, seeded with `update`, the active
/// theme, and the boot artwork. Shared by `create` and by `show()` when it rebuilds
/// the view after `hide()` tore it down.
struct PanelWebViewState<'a> {
    update: &'a PollUpdate,
    theme: PanelTheme,
    pinned: bool,
    expanded: bool,
    shared_sheet: Option<PanelSheet>,
    ui_snapshot: &'a PanelUiSnapshot,
    art_uri: Option<&'a str>,
}

fn build_webview(
    window: &Window,
    state: PanelWebViewState<'_>,
    page_generation: u64,
    on_command: &Rc<dyn Fn(u64, PanelRequest)>,
) -> Result<WebView, Box<dyn Error>> {
    let on_command = Rc::clone(on_command);
    // Windows: share the WebView2 user-data folder with the main window (docs/gui/03 §3)
    // so both surfaces cost one browser-process set. macOS ignores the web context.
    #[cfg(windows)]
    let mut web_context = crate::desktop::platform::shared_web_context();
    #[cfg(windows)]
    let builder = {
        use wry::WebViewBuilderExtWindows;
        WebViewBuilder::new_with_web_context(&mut web_context)
            .with_default_context_menus(cfg!(debug_assertions))
    };
    #[cfg(not(windows))]
    let builder = WebViewBuilder::new();
    let context_menu_guard = if cfg!(debug_assertions) {
        ""
    } else {
        "document.addEventListener('contextmenu',event=>event.preventDefault(),true);"
    };
    let webview = builder
        .with_transparent(true)
        // Fully transparent under-page color so no opaque base backdrop bleeds around the
        // panel shape (belt-and-suspenders alongside wry's `transparent` feature).
        .with_background_color((0, 0, 0, 0))
        .with_initialization_script(context_menu_guard)
        .with_html(panel::html_with_panel_ui_state(
            state.update,
            state.theme,
            state.art_uri,
            state.pinned,
            state.expanded,
            state.shared_sheet,
            state.ui_snapshot,
        ))
        .with_ipc_handler(
            move |request| match panel::parse_ipc_request(request.body()) {
                Ok(command) => (*on_command)(page_generation, command),
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
        // The mini player has no browsing surface. CSP blocks subresources; these native
        // handlers additionally deny top-level and popup navigation before foreign content
        // could inherit the IPC bridge.
        .with_navigation_handler(|_url| false)
        .with_new_window_req_handler(|_url, _features| NewWindowResponse::Deny)
        .build(window)?;
    Ok(webview)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_art_memo_is_scoped_to_track_identity() {
        let first = PageArtKey {
            track_identity: "track-a".to_string(),
            path: Some("shared-cover.png".to_string()),
        };
        let second = PageArtKey {
            track_identity: "track-b".to_string(),
            path: Some("shared-cover.png".to_string()),
        };
        assert_ne!(first, second);
    }

    #[test]
    fn programmatic_geometry_guard_allows_rounding_but_not_a_user_move() {
        let expected = ProgrammaticGeometry {
            x: -901,
            y: 42,
            w: 459,
            h: 414,
            remaining_events: 3,
        };
        assert!(programmatic_rect_matches(expected, -900, 40, 460, 415));
        assert!(!programmatic_rect_matches(expected, -860, 42, 459, 414));
        assert!(!programmatic_rect_matches(expected, -901, 42, 500, 414));
    }

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
    fn transient_resize_keeps_the_owning_work_area_edge_fixed() {
        assert_eq!(
            anchored_to_edge(AnchorEdge::Top, 500.0, 58.0, 306.0, 276.0),
            (347.0, 58.0)
        );
        assert_eq!(
            anchored_to_edge(AnchorEdge::Bottom, 500.0, 742.0, 306.0, 276.0),
            (347.0, 466.0)
        );
        assert_eq!(
            anchored_to_edge(AnchorEdge::Left, 108.0, 400.0, 306.0, 276.0),
            (108.0, 262.0)
        );
        assert_eq!(
            anchored_to_edge(AnchorEdge::Right, 1092.0, 400.0, 306.0, 276.0),
            (786.0, 262.0)
        );
    }

    #[test]
    fn clamp_keeps_rect_inside_monitor_with_margin() {
        // Off the top edge (expansion grew past the top) → pinned to the margin.
        let (x, y) = clamp_to_rect(
            (100.0, -86.0),
            (306.0, 276.0),
            (0.0, 0.0),
            (1920.0, 1080.0),
            8.0,
        );
        assert_eq!((x, y), (100.0, 8.0));
        // Off the right/bottom → pulled back inside.
        let (x, y) = clamp_to_rect(
            (1900.0, 1060.0),
            (306.0, 276.0),
            (0.0, 0.0),
            (1920.0, 1080.0),
            8.0,
        );
        assert_eq!((x, y), (1920.0 - 306.0 - 8.0, 1080.0 - 276.0 - 8.0));
    }

    #[test]
    fn clamp_degrades_gracefully_on_a_tiny_monitor() {
        // Monitor smaller than the panel: min-then-max lands on the top-left margin
        // instead of panicking on an inverted range.
        let (x, y) = clamp_to_rect(
            (500.0, 500.0),
            (306.0, 276.0),
            (0.0, 0.0),
            (200.0, 150.0),
            8.0,
        );
        assert_eq!((x, y), (8.0, 8.0));
    }

    #[test]
    fn tray_anchor_candidates_point_into_each_work_area_edge() {
        let area = MonitorRect {
            x: 100,
            y: 50,
            w: 1000,
            h: 700,
        };
        let size = (300.0, 200.0);
        assert_eq!(
            position_for_anchor((600.0, 750.0), size, area, 8.0),
            (450.0, 542.0),
            "bottom taskbar should place above"
        );
        assert_eq!(
            position_for_anchor((600.0, 50.0), size, area, 8.0),
            (450.0, 58.0),
            "top taskbar should place below"
        );
        assert_eq!(
            position_for_anchor((100.0, 400.0), size, area, 8.0),
            (108.0, 300.0),
            "left taskbar should place right"
        );
        assert_eq!(
            position_for_anchor((1100.0, 400.0), size, area, 8.0),
            (792.0, 300.0),
            "right taskbar should place left"
        );
    }

    #[test]
    fn tray_anchor_clamps_oversized_panels_without_inverting_ranges() {
        let area = MonitorRect {
            x: -500,
            y: -200,
            w: 200,
            h: 150,
        };
        assert_eq!(
            position_for_anchor((-500.0, -125.0), (306.0, 276.0), area, 8.0),
            (-492.0, -192.0)
        );
    }

    #[test]
    fn tray_anchor_margin_scales_from_dip_to_physical_pixels() {
        let area = MonitorRect {
            x: 0,
            y: 0,
            w: 2400,
            h: 1600,
        };
        assert_eq!(
            position_for_anchor((1200.0, 1600.0), (600.0, 400.0), area, 16.0),
            (900.0, 1184.0),
            "8 DIP at 200% scale must produce a 16 physical-pixel inset"
        );
    }
}
