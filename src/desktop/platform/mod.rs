#[cfg(any(target_os = "macos", target_os = "windows"))]
mod main_window;
#[cfg(any(target_os = "macos", target_os = "windows"))]
mod panel_window;

#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "windows")]
pub mod windows;

#[cfg(any(target_os = "macos", target_os = "windows"))]
pub(crate) fn next_webview_generation() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};

    // Process-global across both Main and Mini host lifetimes. A destroyed gen-1 window must
    // never alias a newly-created gen-1 window and accept its late correlated reply.
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

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

#[cfg(all(test, any(target_os = "macos", target_os = "windows")))]
mod tests {
    use super::next_webview_generation;

    #[test]
    fn webview_generations_are_unique_across_host_recreation() {
        let first = next_webview_generation();
        let second = next_webview_generation();
        assert_ne!(first, second);
        assert_ne!(first, 0);
        assert_ne!(second, 0);
    }
}
