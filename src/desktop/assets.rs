//! Embedded frontend dist + the `ytm://app` custom-protocol resolver (docs/gui/04 §3).
//!
//! One host (`app`) serves everything: the SPA and its subresources from the build.rs
//! `ASSETS` table, and artwork from the on-disk cache under the `art/<key>` prefix (bytes
//! never ride the socket — docs/gui/02 §12). The logic here is wry-free and unit-tested;
//! `platform/main_window.rs` maps a [`Served`] onto a `wry::http::Response` and attaches
//! the [`CSP`] header.

use std::borrow::Cow;
use std::path::PathBuf;

// build.rs emits: `pub const DIST_EMBEDDED: bool` and
// `pub static ASSETS: &[(&str /*path*/, &[u8] /*bytes*/, &str /*mime*/)]`.
include!(concat!(env!("OUT_DIR"), "/gui_assets.rs"));

/// Content-Security-Policy applied to every response (docs/gui/04 §3.2). No external
/// origins — the webview never talks to the network. `'unsafe-inline'` styles are required
/// by Svelte's scoped-style injection; scripts stay `'self'` only.
pub const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
img-src 'self' data:; font-src 'self'; connect-src 'self'";

/// Artwork files are trusted (the core's own cache) but still capped so a corrupt path
/// can't balloon memory.
const MAX_ART_BYTES: u64 = 16 * 1024 * 1024;

/// A resolved response: raw bytes + content-type + status. `immutable` marks
/// content-addressed art so the webview caches it forever.
pub struct Served {
    pub status: u16,
    pub content_type: Cow<'static, str>,
    pub body: Cow<'static, [u8]>,
    pub immutable: bool,
}

impl Served {
    fn ok(content_type: Cow<'static, str>, body: Cow<'static, [u8]>, immutable: bool) -> Self {
        Served {
            status: 200,
            content_type,
            body,
            immutable,
        }
    }

    pub fn not_found() -> Self {
        Served {
            status: 404,
            content_type: Cow::Borrowed("text/plain; charset=utf-8"),
            body: Cow::Borrowed(b"not found"),
            immutable: false,
        }
    }
}

/// Resolve a request URL to a response. `art` maps an artwork cache key to an on-disk path
/// (the gateway holds these from B1; M0 passes a resolver that always returns `None`).
pub fn resolve(url: &str, art: impl Fn(&str) -> Option<PathBuf>) -> Served {
    let Some(path) = request_path(url) else {
        return Served::not_found();
    };
    match path.strip_prefix("art/") {
        Some(key) if !key.is_empty() => serve_art(key, art),
        _ => serve_asset(&path),
    }
}

/// Parse the traversal-safe relative path from a request URL, handling both wry surfaces:
/// macOS/WKWebView keeps `ytm://app/index.html`; Windows surfaces it as
/// `https://ytm.app/index.html` (a different literal prefix, same path). Returns `None` for
/// a malformed URL or any traversal attempt.
pub fn request_path(url: &str) -> Option<String> {
    // Drop query/fragment.
    let url = url.split(['?', '#']).next().unwrap_or(url);
    // Everything after the scheme's `://`.
    let after_scheme = url.split_once("://")?.1;
    // Split the authority (host[:port]) from the path at the first '/'.
    let path = match after_scheme.split_once('/') {
        Some((_host, rest)) => rest,
        None => "", // `ytm://app` with no path → root
    };
    if path.is_empty() {
        return Some("index.html".to_string());
    }

    // Reject Windows separators outright, then validate each segment.
    if path.contains('\\') {
        return None;
    }
    let mut clean = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => continue, // collapse `//` and `./`
            ".." => return None,  // never allow traversal
            s => clean.push(s),
        }
    }
    if clean.is_empty() {
        return Some("index.html".to_string());
    }
    Some(clean.join("/"))
}

fn serve_asset(path: &str) -> Served {
    for (asset_path, bytes, mime) in ASSETS {
        if *asset_path == path {
            return Served::ok(Cow::Borrowed(*mime), Cow::Borrowed(*bytes), false);
        }
    }
    Served::not_found()
}

fn serve_art(key: &str, art: impl Fn(&str) -> Option<PathBuf>) -> Served {
    let Some(path) = art(key) else {
        return Served::not_found();
    };
    let Ok(meta) = std::fs::metadata(&path) else {
        return Served::not_found();
    };
    if !meta.is_file() || meta.len() > MAX_ART_BYTES {
        return Served::not_found();
    }
    let Ok(bytes) = std::fs::read(&path) else {
        return Served::not_found();
    };
    let mime = sniff_image(&bytes);
    Served::ok(Cow::Borrowed(mime), Cow::Owned(bytes), true)
}

/// Content-type from magic bytes — art keys are content-addressed, not extension-tagged.
fn sniff_image(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "image/jpeg"
    } else if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        "image/png"
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        "image/webp"
    } else {
        "application/octet-stream"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_macos_and_windows_url_forms() {
        assert_eq!(
            request_path("ytm://app/index.html").as_deref(),
            Some("index.html")
        );
        assert_eq!(
            request_path("https://ytm.app/assets/index.js").as_deref(),
            Some("assets/index.js")
        );
        // Root with no path resolves to index.html on both surfaces.
        assert_eq!(request_path("ytm://app").as_deref(), Some("index.html"));
        assert_eq!(request_path("ytm://app/").as_deref(), Some("index.html"));
        assert_eq!(
            request_path("https://ytm.app/").as_deref(),
            Some("index.html")
        );
    }

    #[test]
    fn strips_query_and_fragment() {
        assert_eq!(
            request_path("ytm://app/assets/x.js?v=2#frag").as_deref(),
            Some("assets/x.js")
        );
    }

    #[test]
    fn rejects_traversal_and_backslashes() {
        assert_eq!(request_path("ytm://app/../../etc/passwd"), None);
        assert_eq!(request_path("ytm://app/art/../escape"), None);
        assert_eq!(
            request_path("ytm://app/assets/..%2f"),
            Some("assets/..%2f".to_string())
        );
        assert_eq!(request_path(r"ytm://app/a\b"), None);
    }

    #[test]
    fn art_route_detected_and_404s_without_a_resolver() {
        let served = resolve("ytm://app/art/vid123", |_| None);
        assert_eq!(served.status, 404);
        // An empty key is not an art request; it falls through to asset lookup (also 404).
        assert_eq!(resolve("ytm://app/art/", |_| None).status, 404);
    }

    #[test]
    fn art_resolver_serves_bytes_with_sniffed_mime() {
        let dir = std::env::temp_dir().join(format!("ytt-art-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("cover");
        std::fs::write(&file, [0xFF, 0xD8, 0xFF, 0x00, 0x11]).unwrap();
        let served = resolve("ytm://app/art/vid", |k| (k == "vid").then(|| file.clone()));
        assert_eq!(served.status, 200);
        assert_eq!(served.content_type, "image/jpeg");
        assert!(served.immutable);
        assert_eq!(&*served.body, &[0xFF, 0xD8, 0xFF, 0x00, 0x11]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn index_html_is_always_embedded() {
        // Stub table or real dist, index.html resolves to a 200 HTML document.
        let served = resolve("ytm://app/index.html", |_| None);
        assert_eq!(served.status, 200);
        assert!(served.content_type.starts_with("text/html"));
        assert!(!served.body.is_empty());
    }

    #[test]
    fn unknown_asset_404s() {
        assert_eq!(resolve("ytm://app/nope.js", |_| None).status, 404);
    }

    #[test]
    fn csp_denies_external_origins() {
        assert!(CSP.contains("default-src 'none'"));
        assert!(CSP.contains("connect-src 'self'"));
        assert!(!CSP.contains("http://"));
    }
}
