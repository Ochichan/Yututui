#[cfg(any(target_os = "macos", target_os = "windows"))]
mod main_window;
#[cfg(any(target_os = "macos", target_os = "windows"))]
mod panel_window;

#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "windows")]
pub mod windows;

/// One explicit WebView2 user-data folder for every `yututray` webview
/// (docs/gui/03 §3, normative): windows sharing a UDF share a single WebView2
/// browser/GPU/utility process set, so the second window costs one renderer
/// instead of a whole second tree — and the data lands in a known, purgeable
/// spot instead of the exe-adjacent default (which breaks under a read-only
/// install dir). The context only feeds `CreateCoreWebView2Environment` at
/// build time, so each build may hold its own short-lived instance.
#[cfg(target_os = "windows")]
pub(crate) fn shared_web_context() -> std::io::Result<wry::WebContext> {
    let directory = crate::desktop::persistence::webview_data_dir()?;
    Ok(wry::WebContext::new(Some(directory)))
}
