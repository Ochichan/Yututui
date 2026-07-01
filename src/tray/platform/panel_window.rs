//! Native window wrapper for the mini player WebView.

use std::error::Error;

use tao::dpi::LogicalSize;
use tao::event_loop::EventLoopWindowTarget;
use tao::window::{Window, WindowBuilder, WindowId};
use wry::{WebView, WebViewBuilder};

use crate::tray::panel::{self, PanelCommand};
use crate::tray::status::PollUpdate;

const PANEL_WIDTH: f64 = 420.0;
const PANEL_HEIGHT: f64 = 430.0;

pub struct MiniPlayerPanel {
    window: Window,
    webview: WebView,
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
            .with_always_on_top(true)
            .with_visible(true)
            .build(target)?;
        let window_id = window.id();
        let webview = WebViewBuilder::new()
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
            window,
            webview,
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
