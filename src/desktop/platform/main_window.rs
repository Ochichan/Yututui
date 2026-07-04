//! The main GUI window (docs/gui/03 §3): decorated, opaque, 1200×800 default / 960×620 min,
//! loading the embedded Svelte app from `ytm://app/index.html` with a per-response CSP and a
//! navigation lock. Distinct from the frameless/transparent mini player (`panel_window.rs`).

use std::borrow::Cow;
use std::cell::RefCell;
use std::error::Error;

use tao::dpi::{LogicalPosition, LogicalSize};
use tao::event_loop::EventLoopWindowTarget;
use tao::window::{Window, WindowBuilder, WindowId};
use wry::http::{Request, Response};
use wry::{NewWindowResponse, WebView, WebViewBuilder};

use crate::desktop::assets;
use crate::desktop::window_state::DesktopState;

const DEFAULT_W: f64 = 1200.0;
const DEFAULT_H: f64 = 800.0;
const MIN_W: f64 = 960.0;
const MIN_H: f64 = 620.0;

pub struct MainWindow {
    // WebView declared before `window`: it must drop first (wry teardown order; the mini
    // player honors the same field order at panel_window.rs:28).
    webview: RefCell<Option<WebView>>,
    window: Window,
    window_id: WindowId,
    // Kept so `show()` can rebuild the WebView after a drop-on-hide (M1).
    boot_json: String,
    dev_url: Option<String>,
    on_message: std::rc::Rc<dyn Fn(String)>,
}

impl MainWindow {
    /// Create the main window. `boot_json` is the `window.__YTM_BOOT__` object literal;
    /// `on_message` receives each webview IPC line (parsed by `bridge::dispatch`).
    /// `dev_url` (from `--dev-frontend`) loads a Vite dev server instead of embedded assets.
    pub fn create<T, F>(
        target: &EventLoopWindowTarget<T>,
        boot_json: String,
        dev_url: Option<String>,
        on_message: F,
    ) -> Result<Self, Box<dyn Error>>
    where
        T: 'static,
        F: Fn(String) + 'static,
    {
        let state = DesktopState::load();
        let mut builder = WindowBuilder::new()
            .with_title("YPlayer")
            .with_inner_size(LogicalSize::new(DEFAULT_W, DEFAULT_H))
            .with_min_inner_size(LogicalSize::new(MIN_W, MIN_H))
            .with_resizable(true)
            .with_visible(true);
        // Title-bar + taskbar identity; macOS draws the bundle icon, so this is Windows-only.
        #[cfg(windows)]
        {
            builder = builder.with_window_icon(crate::desktop::platform::windows::window_icon());
        }
        // Restore saved geometry (the event loop clamps to live monitors on move/resize via
        // window_state::clamp_to_monitors; a fully off-screen restore is the rare edge case).
        if let Some(rect) = state.main {
            builder = builder
                .with_position(LogicalPosition::new(rect.x as f64, rect.y as f64))
                .with_inner_size(LogicalSize::new(rect.w as f64, rect.h as f64))
                .with_maximized(rect.maximized);
        }
        let window = builder.build(target)?;
        let window_id = window.id();

        let on_message: std::rc::Rc<dyn Fn(String)> = std::rc::Rc::new(on_message);
        let webview = build_webview(&window, &boot_json, dev_url.as_deref(), &on_message)?;

        Ok(Self {
            webview: RefCell::new(Some(webview)),
            window,
            window_id,
            boot_json,
            dev_url,
            on_message,
        })
    }

    pub fn window_id(&self) -> WindowId {
        self.window_id
    }

    pub fn show(&self) {
        if self.webview.borrow().is_none() {
            match build_webview(
                &self.window,
                &self.boot_json,
                self.dev_url.as_deref(),
                &self.on_message,
            ) {
                Ok(webview) => *self.webview.borrow_mut() = Some(webview),
                Err(e) => {
                    tracing::warn!(target: "ytt_desktop", error = %e, "could not rebuild main webview")
                }
            }
        }
        self.window.set_visible(true);
        self.window.set_focus();
        if let Some(webview) = &*self.webview.borrow() {
            let _ = webview.focus();
        }
    }

    pub fn hide(&self) {
        self.window.set_visible(false);
    }

    pub fn start_drag(&self) {
        if let Err(e) = self.window.drag_window() {
            tracing::debug!(target: "ytt_desktop", error = %e, "main window drag failed");
        }
    }

    /// Run a script in the webview (e.g. push a `conn`/`event` frame via bridge::receive_script).
    pub fn eval(&self, script: &str) {
        if let Some(webview) = &*self.webview.borrow()
            && let Err(e) = webview.evaluate_script(script)
        {
            tracing::warn!(target: "ytt_desktop", error = %e, "main window evaluate_script failed");
        }
    }

    /// Snapshot the current geometry for persistence (logical px).
    pub fn geometry(&self) -> Option<crate::desktop::window_state::WindowRect> {
        let scale = self.window.scale_factor();
        let pos = self.window.outer_position().ok()?.to_logical::<f64>(scale);
        let size = self.window.inner_size().to_logical::<f64>(scale);
        Some(crate::desktop::window_state::WindowRect {
            x: pos.x as i32,
            y: pos.y as i32,
            w: size.width as u32,
            h: size.height as u32,
            maximized: self.window.is_maximized(),
        })
    }
}

fn build_webview(
    window: &Window,
    boot_json: &str,
    dev_url: Option<&str>,
    on_message: &std::rc::Rc<dyn Fn(String)>,
) -> Result<WebView, Box<dyn Error>> {
    let on_message = std::rc::Rc::clone(on_message);
    // U+2028/2029 are valid JSON but illegal in a JS source position; escape them so the
    // injected object literal parses (mirrors panel.rs json_for_script).
    let safe_boot = boot_json
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029");
    let init = format!("window.__YTM_BOOT__ = {safe_boot};");

    let start_url = dev_url.unwrap_or("ytm://app/index.html").to_string();
    let dev_origin = dev_url.and_then(origin_of);

    // Windows: point WebView2 at the shared user-data folder (docs/gui/03 §3) so the
    // main window and the mini player ride one browser-process set. macOS ignores the
    // web context, so the default builder keeps the signed-off WKWebView path untouched.
    #[cfg(windows)]
    let mut web_context = crate::desktop::platform::shared_web_context();
    #[cfg(windows)]
    let builder = {
        use wry::WebViewBuilderExtWindows;
        // WebView2 has no real custom schemes; wry rides them on `{http|https}://ytm.…`
        // and defaults to http — which the navigation lock below (rightly) denies, so the
        // window rendered black. https matches the lock + gives the page a secure context.
        WebViewBuilder::new_with_web_context(&mut web_context).with_https_scheme(true)
    };
    #[cfg(not(windows))]
    let builder = WebViewBuilder::new();

    let webview = builder
        .with_url(start_url)
        .with_initialization_script(init)
        .with_custom_protocol("ytm".to_string(), ytm_protocol)
        .with_ipc_handler(move |req: Request<String>| (*on_message)(req.body().clone()))
        // Navigation lock (docs/gui/03 §3): wry injects window.ipc into whatever the webview
        // navigates to, so deny everything except our own scheme (+ the dev origin under
        // --dev-frontend). Prevents a stray link from handing foreign content the command surface.
        .with_navigation_handler(move |url: String| is_allowed(&url, dev_origin.as_deref()))
        .with_new_window_req_handler(|_url, _features| NewWindowResponse::Deny)
        .with_devtools(cfg!(debug_assertions))
        .build(window)?;
    Ok(webview)
}

/// The `ytm://app` custom-protocol handler. Serves embedded assets + (M1) artwork, each with
/// the CSP header. M0 has no artwork resolver yet, so `art/*` 404s.
fn ytm_protocol(_id: &str, request: Request<Vec<u8>>) -> Response<Cow<'static, [u8]>> {
    let served = assets::resolve(&request.uri().to_string(), |_key| None);
    let mut builder = Response::builder()
        .status(served.status)
        .header("Content-Type", served.content_type.as_ref())
        .header("Content-Security-Policy", assets::CSP);
    if served.immutable {
        builder = builder.header("Cache-Control", "public, max-age=31536000, immutable");
    }
    builder
        .body(served.body)
        .unwrap_or_else(|_| Response::new(Cow::Borrowed(&b"internal error"[..])))
}

fn is_allowed(url: &str, dev_origin: Option<&str>) -> bool {
    // macOS keeps `ytm://`; Windows surfaces it as `https://ytm.<host>/…`.
    if url.starts_with("ytm://") || url.starts_with("https://ytm.") || url == "about:blank" {
        return true;
    }
    dev_origin.is_some_and(|origin| url.starts_with(origin))
}

/// `scheme://host[:port]` prefix of a URL, for the dev-server navigation allowance.
fn origin_of(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let authority = rest.split('/').next().unwrap_or(rest);
    Some(format!("{scheme}://{authority}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn navigation_lock_allows_only_our_scheme_and_dev_origin() {
        assert!(is_allowed("ytm://app/index.html", None));
        assert!(is_allowed("https://ytm.app/assets/x.js", None));
        assert!(!is_allowed("https://evil.example/x", None));
        assert!(!is_allowed("http://localhost:5173/", None));
        assert!(is_allowed(
            "http://localhost:5173/",
            Some("http://localhost:5173")
        ));
    }

    #[test]
    fn origin_extraction() {
        assert_eq!(
            origin_of("http://localhost:5173/src/main.ts").as_deref(),
            Some("http://localhost:5173")
        );
        assert_eq!(origin_of("not a url"), None);
    }
}
