//! The main GUI window (docs/gui/03 §3): decorated, opaque, 1200×800 default / 960×620 min,
//! loading the embedded Svelte app from `ytm://app/index.html` with a per-response CSP and a
//! navigation lock. Distinct from the frameless/transparent mini player (`panel_window.rs`).

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::error::Error;

use tao::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use tao::event_loop::EventLoopWindowTarget;
use tao::window::{Window, WindowBuilder, WindowId};
use wry::http::{Request, Response, Uri};
use wry::{NewWindowResponse, WebView, WebViewBuilder};

use crate::desktop::assets;
use crate::desktop::bridge::UiSnapshot;
use crate::desktop::window_state::{
    DesktopState, MonitorDescriptor, MonitorRect, Size, WindowPlacement, WindowRect,
    clamp_to_monitors, reconcile_physical_rect, resolve_legacy_rect,
};

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
    on_message: std::rc::Rc<dyn Fn(u64, String)>,
    // A newly-created/rebuilt WebView remains hidden until its page has installed
    // IPC handlers and sent the explicit FrontendReady handshake.
    frontend_ready: Cell<bool>,
    page_generation: Cell<u64>,
    // Small, non-authoritative navigation state captured by the live page before the idle
    // WebView grace expires. It belongs to the native host so a browser-engine rebuild can
    // rehydrate without persisting transient UI state to disk.
    ui_snapshot: RefCell<Option<UiSnapshot>>,
    // The OS reports maximized/minimized bounds while those states are active.
    // Retain the latest normal rect so persistence restores a useful window.
    last_normal_rect: Cell<Option<WindowRect>>,
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
        F: Fn(u64, String) + 'static,
    {
        let state = DesktopState::load();
        let builder = WindowBuilder::new()
            .with_title("YuTuTray!")
            .with_inner_size(LogicalSize::new(DEFAULT_W, DEFAULT_H))
            .with_min_inner_size(LogicalSize::new(MIN_W, MIN_H))
            .with_resizable(true)
            // Hidden-first prevents the default-position/blank WebView flash and
            // keeps an incomplete window out of app switchers if WebView setup fails.
            .with_focused(false)
            .with_visible(false);
        // Title-bar + taskbar identity; macOS draws the bundle icon, so this is Windows-only.
        #[cfg(windows)]
        let builder = builder.with_window_icon(crate::desktop::platform::windows::window_icon());
        let window = builder.build(target)?;
        let window_id = window.id();

        let restored = restore_rect(&state, &window);
        if let Some(restored) = restored {
            apply_rect(&window, restored.physical);
        }

        let page_generation = super::next_webview_generation();
        let on_message: std::rc::Rc<dyn Fn(u64, String)> = std::rc::Rc::new(on_message);
        let webview = build_webview(
            &window,
            &boot_json,
            dev_url.as_deref(),
            page_generation,
            &on_message,
        )?;

        let initial_normal = restored
            .map(|restored| restored.logical)
            .or_else(|| current_rect(&window))
            .map(as_normal_rect);
        Ok(Self {
            webview: RefCell::new(Some(webview)),
            window,
            window_id,
            boot_json,
            dev_url,
            on_message,
            frontend_ready: Cell::new(false),
            page_generation: Cell::new(page_generation),
            ui_snapshot: RefCell::new(None),
            last_normal_rect: Cell::new(initial_normal),
        })
    }

    pub fn window_id(&self) -> WindowId {
        self.window_id
    }

    /// Ensure the hidden native host has a usable WebView without exposing it. This preserves
    /// hidden host → role/placement → FrontendReady → show ordering across rebuilds.
    pub fn ensure_surface(&self) -> bool {
        // A hidden host can outlive the monitor it was placed on. Reconcile before either
        // rebuilding or showing so an off-screen WebView never becomes the user's only way
        // back into the application after dock/undock, sleep, or an RDP topology change.
        self.reconcile_live_placement();
        if self.webview.borrow().is_none() {
            let next_generation = super::next_webview_generation();
            let boot_json =
                boot_json_with_ui_snapshot(&self.boot_json, self.ui_snapshot.borrow().as_ref());
            match build_webview(
                &self.window,
                &boot_json,
                self.dev_url.as_deref(),
                next_generation,
                &self.on_message,
            ) {
                Ok(webview) => {
                    *self.webview.borrow_mut() = Some(webview);
                    self.page_generation.set(next_generation);
                }
                Err(e) => {
                    crate::desktop::native_error::show(
                        "YuTuTui! Desktop",
                        &format!("Could not restore the main window: {e}"),
                    );
                    tracing::warn!(target: "ytt_desktop", error = %e, "could not rebuild main webview");
                    self.window.set_visible(false);
                    return false;
                }
            }
            self.frontend_ready.set(false);
        }
        true
    }

    /// Show only after the current page generation completed `FrontendReady`.
    pub fn show(&self) -> bool {
        if !self.ensure_surface() {
            return false;
        }
        if !self.frontend_ready.get() {
            self.window.set_visible(false);
            return false;
        }
        self.window.set_minimized(false);
        self.window.set_visible(true);
        self.window.set_focus();
        if let Some(webview) = &*self.webview.borrow() {
            let _ = webview.focus();
        }
        true
    }

    /// Mark the current page generation ready. The platform adapter replays the
    /// latest connection/snapshots before calling [`show`](Self::show).
    pub fn mark_frontend_ready(&self) {
        self.frontend_ready.set(true);
    }

    pub fn page_generation(&self) -> u64 {
        self.page_generation.get()
    }

    pub fn cache_ui_snapshot(&self, snapshot: UiSnapshot) {
        *self.ui_snapshot.borrow_mut() = Some(snapshot);
    }

    pub fn hide(&self) {
        self.record_geometry();
        self.window.set_visible(false);
    }

    pub fn teardown_webview(&self) {
        self.webview.borrow_mut().take();
        self.frontend_ready.set(false);
    }

    /// Refresh the cached normal bounds after a user move/resize event.
    pub fn record_geometry(&self) {
        if !self.window.is_maximized()
            && !self.window.is_minimized()
            && let Some(rect) = current_rect(&self.window)
        {
            self.last_normal_rect.set(Some(as_normal_rect(rect)));
        }
    }

    pub fn start_drag(&self) {
        if let Err(e) = self.window.drag_window() {
            tracing::debug!(target: "ytt_desktop", error = %e, "main window drag failed");
        }
    }

    /// Run a script in the webview (e.g. push a `conn`/`event` frame via bridge::receive_script).
    pub fn eval(&self, script: &str) {
        if self.frontend_ready.get()
            && let Some(webview) = &*self.webview.borrow()
            && let Err(e) = webview.evaluate_script(script)
        {
            tracing::warn!(target: "ytt_desktop", error = %e, "main window evaluate_script failed");
        }
    }

    /// Snapshot the current geometry for persistence (logical px).
    pub fn geometry(&self) -> Option<crate::desktop::window_state::WindowRect> {
        let maximized = self.window.is_maximized();
        geometry_snapshot(
            maximized,
            self.window.is_minimized(),
            self.last_normal_rect.get(),
            current_rect(&self.window),
        )
    }

    pub fn placement(&self) -> Option<WindowPlacement> {
        let rect = self.geometry()?;
        let monitor = descriptor_for_window(&self.window)?;
        Some(WindowPlacement::capture(rect, &monitor))
    }

    /// Reconcile an existing native host against live work areas. Returns whether a native
    /// move/resize was requested. A normally placed window keeps its current monitor and
    /// physical origin; a removed display recovers to the primary, and its normal DIP size is
    /// converted with the *target* monitor's scale rather than the stale window scale.
    pub fn reconcile_live_placement(&self) -> bool {
        let monitors = monitor_descriptors(&self.window);
        let Some(current) = current_physical_rect(&self.window) else {
            return false;
        };
        // A live maximized window is already owned by its monitor's native window manager.
        // Only disturb it when that monitor vanished and the native bounds no longer overlap
        // any work area; toggling maximize during an ordinary DPI event would visibly flash.
        if current.maximized && overlaps_live_monitor(current, &monitors) {
            return false;
        }
        let logical_size = self
            .last_normal_rect
            .get()
            .map(|rect| Size {
                w: rect.w,
                h: rect.h,
            })
            .unwrap_or_else(|| {
                let scale = self.window.scale_factor().max(f64::EPSILON);
                Size {
                    w: (f64::from(current.w) / scale).round().max(MIN_W) as u32,
                    h: (f64::from(current.h) / scale).round().max(MIN_H) as u32,
                }
            });
        let Some(reconciled) = reconcile_physical_rect(current, logical_size, &monitors, 0) else {
            return false;
        };
        if physical_rect_matches(current, reconciled.rect) {
            return false;
        }

        let was_maximized = self.window.is_maximized();
        if was_maximized {
            self.window.set_maximized(false);
        }
        let mut normal = reconciled.rect;
        normal.maximized = false;
        apply_rect(&self.window, normal);
        if was_maximized {
            self.window.set_maximized(true);
        }
        true
    }
}

fn current_rect(window: &Window) -> Option<WindowRect> {
    let monitor = descriptor_for_window(window)?;
    let mut rect = monitor.to_logical(current_physical_rect(window)?);
    rect.w = rect.w.max(MIN_W as u32);
    rect.h = rect.h.max(MIN_H as u32);
    Some(rect)
}

fn boot_json_with_ui_snapshot(boot_json: &str, snapshot: Option<&UiSnapshot>) -> String {
    let Some(snapshot) = snapshot else {
        return boot_json.to_string();
    };
    let Ok(mut boot) = serde_json::from_str::<serde_json::Value>(boot_json) else {
        return boot_json.to_string();
    };
    let Some(object) = boot.as_object_mut() else {
        return boot_json.to_string();
    };
    let Ok(snapshot) = serde_json::to_value(snapshot) else {
        return boot_json.to_string();
    };
    object.insert("uiState".to_string(), snapshot);
    boot.to_string()
}

fn current_physical_rect(window: &Window) -> Option<WindowRect> {
    let pos = window.outer_position().ok()?;
    let size = window.outer_size();
    Some(WindowRect {
        x: pos.x,
        y: pos.y,
        w: size.width,
        h: size.height,
        maximized: window.is_maximized(),
    })
}

fn overlaps_live_monitor(rect: WindowRect, monitors: &[MonitorDescriptor]) -> bool {
    monitors
        .iter()
        .any(|monitor| rects_overlap(rect, monitor.physical_work_area))
}

fn rects_overlap(rect: WindowRect, area: MonitorRect) -> bool {
    let left = i64::from(rect.x);
    let top = i64::from(rect.y);
    let right = left.saturating_add(i64::from(rect.w));
    let bottom = top.saturating_add(i64::from(rect.h));
    let area_left = i64::from(area.x);
    let area_top = i64::from(area.y);
    let area_right = area_left.saturating_add(i64::from(area.w));
    let area_bottom = area_top.saturating_add(i64::from(area.h));
    left < area_right && right > area_left && top < area_bottom && bottom > area_top
}

fn physical_rect_matches(left: WindowRect, right: WindowRect) -> bool {
    const NATIVE_ROUNDING_TOLERANCE: i64 = 2;
    (i64::from(left.x) - i64::from(right.x)).abs() <= NATIVE_ROUNDING_TOLERANCE
        && (i64::from(left.y) - i64::from(right.y)).abs() <= NATIVE_ROUNDING_TOLERANCE
        && (i64::from(left.w) - i64::from(right.w)).abs() <= NATIVE_ROUNDING_TOLERANCE
        && (i64::from(left.h) - i64::from(right.h)).abs() <= NATIVE_ROUNDING_TOLERANCE
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RestoredRect {
    logical: WindowRect,
    physical: WindowRect,
}

fn restore_rect(state: &DesktopState, window: &Window) -> Option<RestoredRect> {
    let monitors = monitor_descriptors(window);
    let areas: Vec<_> = monitors.iter().map(|monitor| monitor.work_area).collect();
    if let Some(resolved) = state
        .placement_v2
        .main
        .as_ref()
        .and_then(|placement| placement.resolve(&monitors))
    {
        let host = &monitors[resolved.monitor_index];
        let logical = prepare_restored_rect(resolved.rect, &[host.work_area])?;
        return Some(RestoredRect {
            logical,
            physical: host.to_physical(logical),
        });
    }

    let logical = prepare_restored_rect(state.main?, &areas)?;
    let resolved = resolve_legacy_rect(logical, &monitors)?;
    let host = &monitors[resolved.monitor_index];
    Some(RestoredRect {
        logical: resolved.rect,
        physical: host.to_physical(resolved.rect),
    })
}

/// Enforce the main window's preferred minimum first, then cap it to the selected live work
/// area. The second step is intentionally last: a small work area must win over our minimum.
fn prepare_restored_rect(mut rect: WindowRect, work_areas: &[MonitorRect]) -> Option<WindowRect> {
    if work_areas.is_empty() {
        return None;
    }
    rect.w = rect.w.max(MIN_W as u32);
    rect.h = rect.h.max(MIN_H as u32);
    Some(clamp_to_monitors(rect, work_areas))
}

fn as_normal_rect(mut rect: WindowRect) -> WindowRect {
    rect.maximized = false;
    rect
}

fn geometry_snapshot(
    maximized: bool,
    minimized: bool,
    last_normal: Option<WindowRect>,
    current: Option<WindowRect>,
) -> Option<WindowRect> {
    let mut rect = if maximized || minimized {
        last_normal.or(current)?
    } else {
        current?
    };
    rect.maximized = maximized;
    Some(rect)
}

fn apply_rect(window: &Window, rect: WindowRect) {
    window.set_outer_position(PhysicalPosition::new(rect.x, rect.y));
    // tao exposes outer-position but only inner-size mutation. Convert the persisted outer
    // bounds using the decoration insets of this already-created native window so a relaunch
    // does not grow by one title bar/frame on every save/restore cycle.
    let outer = window.outer_size();
    let inner = window.inner_size();
    window.set_inner_size(inner_size_for_outer(rect.w, rect.h, outer, inner));
    window.set_maximized(rect.maximized);
}

fn inner_size_for_outer(
    target_w: u32,
    target_h: u32,
    current_outer: PhysicalSize<u32>,
    current_inner: PhysicalSize<u32>,
) -> PhysicalSize<u32> {
    PhysicalSize::new(
        target_w.saturating_sub(current_outer.width.saturating_sub(current_inner.width)),
        target_h.saturating_sub(current_outer.height.saturating_sub(current_inner.height)),
    )
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
    handles.iter().map(monitor_descriptor).collect::<Vec<_>>()
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

fn build_webview(
    window: &Window,
    boot_json: &str,
    dev_url: Option<&str>,
    page_generation: u64,
    on_message: &std::rc::Rc<dyn Fn(u64, String)>,
) -> Result<WebView, Box<dyn Error>> {
    let on_message = std::rc::Rc::clone(on_message);
    // U+2028/2029 are valid JSON but illegal in a JS source position; escape them so the
    // injected object literal parses (mirrors panel.rs json_for_script).
    let safe_boot = boot_json
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029");
    let context_menu_guard = if cfg!(debug_assertions) {
        ""
    } else {
        "document.addEventListener('contextmenu',event=>event.preventDefault(),true);"
    };
    let init = format!("window.__YTM_BOOT__ = {safe_boot};{context_menu_guard}");

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
        WebViewBuilder::new_with_web_context(&mut web_context)
            .with_https_scheme(true)
            .with_default_context_menus(cfg!(debug_assertions))
    };
    #[cfg(not(windows))]
    let builder = WebViewBuilder::new();

    let webview = builder
        .with_url(start_url)
        .with_initialization_script(init)
        .with_custom_protocol("ytm".to_string(), ytm_protocol)
        .with_ipc_handler(move |req: Request<String>| {
            (*on_message)(page_generation, req.body().clone());
        })
        // Navigation lock (docs/gui/03 §3): wry injects window.ipc into whatever the webview
        // navigates to, so deny everything except our own scheme (+ the dev origin under
        // --dev-frontend). Prevents a stray link from handing foreign content the command surface.
        .with_navigation_handler(move |url: String| is_allowed(&url, dev_origin.as_deref()))
        .with_new_window_req_handler(|_url, _features| NewWindowResponse::Deny)
        .with_devtools(cfg!(debug_assertions))
        .build(window)?;
    Ok(webview)
}

/// The `ytm://app` custom-protocol handler: embedded assets + artwork from the media-art
/// disk cache (deterministic layout, so no engine state is needed), each with the CSP.
fn ytm_protocol(_id: &str, request: Request<Vec<u8>>) -> Response<Cow<'static, [u8]>> {
    let served = assets::resolve(
        &request.uri().to_string(),
        crate::media::artwork::cached_art_path,
    );
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
    if url == "about:blank" {
        return true;
    }
    let Ok(uri) = url.parse::<Uri>() else {
        return false;
    };
    let scheme = uri.scheme_str();
    let authority = uri.authority().map(|authority| authority.as_str());

    // macOS keeps the custom URL. Comparing parsed authorities prevents look-alike hosts,
    // user-info, ports, and simple string-prefix escapes.
    if matches!((scheme, authority), (Some("ytm"), Some("app"))) {
        return true;
    }
    // Only WebView2 rewrites the custom scheme to this secure synthetic origin. On WKWebView
    // the same URL is a real network page and must never inherit the native IPC surface.
    #[cfg(windows)]
    if matches!((scheme, authority), (Some("https"), Some("ytm.app"))) {
        return true;
    }

    let Some(dev_origin) = dev_origin.and_then(http_origin) else {
        return false;
    };
    http_origin(url).is_some_and(|candidate| candidate == dev_origin)
}

/// `scheme://host[:port]` prefix of a URL, for the dev-server navigation allowance.
fn origin_of(url: &str) -> Option<String> {
    let uri = url.parse::<Uri>().ok()?;
    let scheme = uri.scheme_str()?;
    if !matches!(scheme, "http" | "https") {
        return None;
    }
    let authority = uri.authority()?.as_str();
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    Some(format!("{scheme}://{authority}"))
}

#[derive(Debug, PartialEq, Eq)]
struct HttpOrigin {
    scheme: String,
    host: String,
    effective_port: u16,
}

fn http_origin(url: &str) -> Option<HttpOrigin> {
    let uri = url.parse::<Uri>().ok()?;
    let scheme = uri.scheme_str()?;
    let default_port = match scheme {
        "http" => 80,
        "https" => 443,
        _ => return None,
    };
    let authority = uri.authority()?;
    if authority.as_str().contains('@') || authority.host().is_empty() {
        return None;
    }
    Some(HttpOrigin {
        scheme: scheme.to_ascii_lowercase(),
        host: authority.host().to_ascii_lowercase(),
        effective_port: authority.port_u16().unwrap_or(default_port),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PRIMARY: MonitorRect = MonitorRect {
        x: 0,
        y: 0,
        w: 1920,
        h: 1080,
    };

    #[test]
    fn restored_main_rect_respects_minimum_and_work_area_maximum() {
        struct Case {
            name: &'static str,
            rect: WindowRect,
            work_areas: Vec<MonitorRect>,
            expected: Option<WindowRect>,
        }

        let cases = vec![
            Case {
                name: "corrupt zero size grows to preferred minimum",
                rect: WindowRect {
                    x: 100,
                    y: 80,
                    w: 0,
                    h: 0,
                    maximized: false,
                },
                work_areas: vec![PRIMARY],
                expected: Some(WindowRect {
                    x: 100,
                    y: 80,
                    w: 960,
                    h: 620,
                    maximized: false,
                }),
            },
            Case {
                name: "oversized maximized normal rect shrinks to work area",
                rect: WindowRect {
                    x: -500,
                    y: -400,
                    w: u32::MAX,
                    h: u32::MAX,
                    maximized: true,
                },
                work_areas: vec![PRIMARY],
                expected: Some(WindowRect {
                    x: 0,
                    y: 0,
                    w: 1920,
                    h: 1080,
                    maximized: true,
                }),
            },
            Case {
                name: "small negative-coordinate work area wins over preferred minimum",
                rect: WindowRect {
                    x: -700,
                    y: -50,
                    w: 1,
                    h: 1,
                    maximized: false,
                },
                work_areas: vec![MonitorRect {
                    x: -800,
                    y: -100,
                    w: 800,
                    h: 600,
                }],
                expected: Some(WindowRect {
                    x: -800,
                    y: -100,
                    w: 800,
                    h: 600,
                    maximized: false,
                }),
            },
            Case {
                name: "no monitor ignores persisted geometry",
                rect: WindowRect {
                    x: i32::MAX,
                    y: i32::MIN,
                    w: u32::MAX,
                    h: u32::MAX,
                    maximized: false,
                },
                work_areas: vec![],
                expected: None,
            },
        ];

        for case in cases {
            assert_eq!(
                prepare_restored_rect(case.rect, &case.work_areas),
                case.expected,
                "{}",
                case.name
            );
        }
    }

    #[test]
    fn geometry_snapshot_keeps_normal_bounds_while_maximized_or_minimized() {
        struct Case {
            name: &'static str,
            maximized: bool,
            minimized: bool,
            last_normal: Option<WindowRect>,
            current: Option<WindowRect>,
            expected: Option<WindowRect>,
        }

        let normal = WindowRect {
            x: 180,
            y: 120,
            w: 1200,
            h: 800,
            maximized: false,
        };
        let os_maximized_bounds = WindowRect {
            x: 0,
            y: 0,
            w: 1920,
            h: 1080,
            maximized: true,
        };
        let current_normal = WindowRect {
            x: 300,
            y: 200,
            w: 1000,
            h: 700,
            maximized: false,
        };
        let cases = [
            Case {
                name: "maximized uses cached normal bounds",
                maximized: true,
                minimized: false,
                last_normal: Some(normal),
                current: Some(os_maximized_bounds),
                expected: Some(WindowRect {
                    maximized: true,
                    ..normal
                }),
            },
            Case {
                name: "minimized uses cached normal bounds",
                maximized: false,
                minimized: true,
                last_normal: Some(normal),
                current: Some(os_maximized_bounds),
                expected: Some(normal),
            },
            Case {
                name: "normal uses current bounds",
                maximized: false,
                minimized: false,
                last_normal: Some(normal),
                current: Some(current_normal),
                expected: Some(current_normal),
            },
            Case {
                name: "maximized falls back when no normal bounds were observed",
                maximized: true,
                minimized: false,
                last_normal: None,
                current: Some(os_maximized_bounds),
                expected: Some(os_maximized_bounds),
            },
        ];

        for case in cases {
            assert_eq!(
                geometry_snapshot(
                    case.maximized,
                    case.minimized,
                    case.last_normal,
                    case.current
                ),
                case.expected,
                "{}",
                case.name
            );
        }
    }

    #[test]
    fn persisted_outer_size_is_converted_to_inner_size_once() {
        assert_eq!(
            inner_size_for_outer(
                1200,
                800,
                PhysicalSize::new(1220, 840),
                PhysicalSize::new(1200, 800),
            ),
            PhysicalSize::new(1180, 760)
        );
        assert_eq!(
            inner_size_for_outer(10, 10, PhysicalSize::new(40, 50), PhysicalSize::new(0, 0),),
            PhysicalSize::new(0, 0)
        );
    }

    #[test]
    fn main_ui_snapshot_is_injected_only_for_a_webview_rebuild() {
        let boot = r#"{"platform":"windows","uiState":null}"#;
        let snapshot = UiSnapshot {
            view: crate::desktop::bridge::UiView::Library,
            queue_open: true,
            settings_tab: crate::desktop::bridge::UiSettingsTab::Playback,
            library_tab: crate::desktop::bridge::UiLibraryTab::Favorites,
            scroll_y: 420,
            active_control: Some("track-row-7".to_string()),
            scroll_positions: std::collections::BTreeMap::from([("queue-list".to_string(), 260)]),
            drafts: std::collections::BTreeMap::from([(
                "library-filter".to_string(),
                "jazz".to_string(),
            )]),
        };
        let value: serde_json::Value =
            serde_json::from_str(&boot_json_with_ui_snapshot(boot, Some(&snapshot))).unwrap();

        assert_eq!(value["platform"], "windows");
        assert_eq!(value["uiState"]["view"], "library");
        assert_eq!(value["uiState"]["queueOpen"], true);
        assert_eq!(value["uiState"]["scrollY"], 420);
        assert_eq!(value["uiState"]["activeControl"], "track-row-7");
        assert_eq!(value["uiState"]["scrollPositions"]["queue-list"], 260);
        assert_eq!(value["uiState"]["drafts"]["library-filter"], "jazz");
        assert_eq!(boot_json_with_ui_snapshot(boot, None), boot);
        assert_eq!(
            boot_json_with_ui_snapshot("not-json", Some(&snapshot)),
            "not-json"
        );
    }

    #[test]
    fn physical_overlap_and_rounding_checks_are_boundary_safe() {
        let rect = WindowRect {
            x: -100,
            y: 20,
            w: 100,
            h: 80,
            maximized: false,
        };
        assert!(!rects_overlap(rect, PRIMARY));
        assert!(rects_overlap(WindowRect { x: -99, ..rect }, PRIMARY));
        assert!(physical_rect_matches(
            rect,
            WindowRect {
                x: -98,
                y: 18,
                w: 102,
                h: 78,
                maximized: false,
            }
        ));
        assert!(!physical_rect_matches(rect, WindowRect { x: -97, ..rect }));
    }

    #[test]
    fn navigation_lock_allows_only_our_scheme_and_dev_origin() {
        assert!(is_allowed("ytm://app/index.html", None));
        assert_eq!(
            is_allowed("https://ytm.app/assets/x.js", None),
            cfg!(windows)
        );
        assert!(!is_allowed("ytm://evil/index.html", None));
        assert!(!is_allowed("https://ytm.attacker.example/", None));
        assert!(!is_allowed("https://ytm.app.attacker.example/", None));
        assert!(!is_allowed("https://ytm.app:443/", None));
        assert!(!is_allowed("https://user@ytm.app/", None));
        assert!(!is_allowed("https://evil.example/x", None));
        assert!(!is_allowed("http://localhost:5173/", None));
        assert!(is_allowed(
            "http://localhost:5173/",
            Some("http://localhost:5173")
        ));
        assert!(!is_allowed(
            "http://localhost:51730/",
            Some("http://localhost:5173")
        ));
        assert!(!is_allowed(
            "http://localhost:5173.evil.example/",
            Some("http://localhost:5173")
        ));
        assert!(is_allowed(
            "http://localhost:80/src/main.ts",
            Some("http://localhost")
        ));
        assert!(is_allowed(
            "https://localhost/src/main.ts",
            Some("https://localhost:443")
        ));
        assert!(!is_allowed(
            "https://localhost:444/src/main.ts",
            Some("https://localhost")
        ));
    }

    #[test]
    fn origin_extraction() {
        assert_eq!(
            origin_of("http://localhost:5173/src/main.ts").as_deref(),
            Some("http://localhost:5173")
        );
        assert_eq!(origin_of("not a url"), None);
        assert_eq!(origin_of("file:///tmp/index.html"), None);
        assert_eq!(origin_of("http://user@localhost:5173/"), None);
    }
}
