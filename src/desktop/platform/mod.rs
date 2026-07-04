#[cfg(any(target_os = "macos", target_os = "windows"))]
mod main_window;
#[cfg(any(target_os = "macos", target_os = "windows"))]
mod panel_window;

#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "windows")]
pub mod windows;

/// One explicit WebView2 user-data folder for every `ytt-desktop` webview
/// (docs/gui/03 §3, normative): windows sharing a UDF share a single WebView2
/// browser/GPU/utility process set, so the second window costs one renderer
/// instead of a whole second tree — and the data lands in a known, purgeable
/// spot instead of the exe-adjacent default (which breaks under a read-only
/// install dir). The context only feeds `CreateCoreWebView2Environment` at
/// build time, so each build may hold its own short-lived instance.
#[cfg(target_os = "windows")]
pub(crate) fn shared_web_context() -> wry::WebContext {
    let dir = directories::ProjectDirs::from("", "", "ytm-tui")
        .map(|dirs| dirs.data_local_dir().join("WebView2"));
    let dir = dir.and_then(|dir| match std::fs::create_dir_all(&dir) {
        Ok(()) => Some(dir),
        Err(e) => {
            tracing::warn!(
                target: "ytt_tray",
                error = %e,
                path = %dir.display(),
                "could not create the WebView2 user-data folder; using the default"
            );
            None
        }
    });
    wry::WebContext::new(dir)
}
