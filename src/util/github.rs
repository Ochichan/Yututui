//! Tiny GitHub-release helpers shared by the managed yt-dlp updater and the app's
//! own update check.
//!
//! [`latest_release_tag`] resolves a repo's latest **stable** release tag from the
//! `releases/latest` **302 redirect** on the plain web endpoint — never the REST API.
//! That dodges the 60-req/hour unauthenticated API rate limit entirely, needs no JSON
//! parsing, and (because `releases/latest` already skips prereleases and drafts) only
//! ever reports a real published release.

use std::time::Duration;

/// The latest release tag for `repo` (`"owner/name"`), read from the `releases/latest`
/// redirect's `Location` header (`…/releases/tag/<tag>` → `<tag>`).
pub async fn latest_release_tag(repo: &str) -> Result<String, String> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("yututui/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| format!("http client: {e}"))?;
    let url = format!("https://github.com/{repo}/releases/latest");
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("release check failed: {e}"))?;
    let Some(location) = resp
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|v| v.to_str().ok())
    else {
        return Err(format!(
            "release check failed: expected a redirect from {url}, got HTTP {}",
            resp.status()
        ));
    };
    parse_tag_from_location(location)
        .ok_or_else(|| format!("release check failed: unexpected redirect target {location}"))
}

/// `…/releases/tag/<tag>` → `<tag>`.
pub(crate) fn parse_tag_from_location(location: &str) -> Option<String> {
    let (_, tag) = location.split_once("/releases/tag/")?;
    let tag = tag
        .split(['?', '#'])
        .next()
        .unwrap_or(tag)
        .trim_end_matches('/');
    (!tag.is_empty()).then(|| tag.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_parses_from_release_redirect_location() {
        assert_eq!(
            parse_tag_from_location(
                "https://github.com/yt-dlp/yt-dlp-nightly-builds/releases/tag/2026.07.03.234421"
            )
            .as_deref(),
            Some("2026.07.03.234421")
        );
        // Relative Location, trailing slash, query/fragment noise.
        assert_eq!(
            parse_tag_from_location("/yt-dlp/yt-dlp/releases/tag/2026.06.09/").as_deref(),
            Some("2026.06.09")
        );
        assert_eq!(
            parse_tag_from_location("/yt-dlp/yt-dlp/releases/tag/2026.06.09?foo=1#frag").as_deref(),
            Some("2026.06.09")
        );
        // App release tags carry a leading `v`; the parser keeps them verbatim.
        assert_eq!(
            parse_tag_from_location("/Ochichan/Yututui/releases/tag/v1.7.0").as_deref(),
            Some("v1.7.0")
        );
        // Not a tag redirect (repo missing → redirected to login, etc.).
        assert_eq!(parse_tag_from_location("https://github.com/login"), None);
        assert_eq!(
            parse_tag_from_location("/yt-dlp/yt-dlp/releases/tag/"),
            None
        );
    }
}
