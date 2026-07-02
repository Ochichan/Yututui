//! Native window wrapper for the mini player WebView.

use std::error::Error;

use tao::dpi::LogicalSize;
use tao::event_loop::EventLoopWindowTarget;
use tao::window::{Window, WindowBuilder, WindowId};
use wry::{WebView, WebViewBuilder};

use crate::tray::panel::{self, PanelCommand};
use crate::tray::status::PollUpdate;

// The window is frameless and transparent: the HTML draws a rounded "cushion" with
// its own shadow, so the extra margin baked into these dimensions is the shadow's
// breathing room, not dead space.
const PANEL_WIDTH: f64 = 398.0;
const PANEL_HEIGHT: f64 = 602.0;

pub struct MiniPlayerPanel {
    // Field order is load-bearing: the WebView must drop before its host window
    // (wry's documented teardown order; on Windows the WebView2 controller must be
    // closed while the HWND is still alive).
    webview: WebView,
    window: Window,
    window_id: WindowId,
}

impl MiniPlayerPanel {
    pub fn create<T, F>(
        target: &EventLoopWindowTarget<T>,
        initial: &PollUpdate,
        on_command: F,
    ) -> Result<Self, Box<dyn Error>>
    where
        T: 'static,
        F: Fn(PanelCommand) + 'static,
    {
        let size = LogicalSize::new(PANEL_WIDTH, PANEL_HEIGHT);
        let window = WindowBuilder::new()
            .with_title("YtmTui Mini Player")
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
            .with_visible(true)
            .build(target)?;
        let window_id = window.id();
        let webview = WebViewBuilder::new()
            .with_transparent(true)
            .with_html(panel::html(initial))
            .with_ipc_handler(
                move |request| match panel::parse_ipc_message(request.body()) {
                    Ok(command) => on_command(command),
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
            .build(&window)?;

        Ok(Self {
            webview,
            window,
            window_id,
        })
    }

    pub fn window_id(&self) -> WindowId {
        self.window_id
    }

    pub fn show(&self) {
        self.window.set_visible(true);
        self.window.set_focus();
        let _ = self.webview.focus();
    }

    pub fn hide(&self) {
        self.window.set_visible(false);
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
        if let Err(e) = self.webview.evaluate_script(&panel::update_script(update)) {
            tracing::warn!(
                target: "ytt_tray",
                error = %e,
                "could not update mini player panel"
            );
        }
    }
}
