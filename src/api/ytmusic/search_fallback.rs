use std::future::Future;
use std::time::Duration;

use anyhow::Result;

use super::{
    SEARCH_RESULT_LIMIT, Song, YoutubeSearchKind, mark_auth_search_degraded,
    mark_auth_search_healthy, ytdlp_search,
};
use crate::util::sanitize;

const AUTH_SEARCH_TIMEOUT: Duration = Duration::from_secs(12);

pub(super) async fn authenticated_youtube_search_with_fallback(
    query: &str,
    search: impl Future<Output = Result<Vec<Song>>>,
) -> Result<Vec<(Song, YoutubeSearchKind)>> {
    authenticated_youtube_search_with_timeout(
        search,
        ytdlp_search(query, SEARCH_RESULT_LIMIT),
        AUTH_SEARCH_TIMEOUT,
    )
    .await
}

async fn authenticated_youtube_search_with_timeout(
    search: impl Future<Output = Result<Vec<Song>>>,
    fallback: impl Future<Output = Result<Vec<Song>>>,
    timeout: Duration,
) -> Result<Vec<(Song, YoutubeSearchKind)>> {
    match tokio::time::timeout(timeout, search).await {
        Ok(Ok(songs)) if !songs.is_empty() => {
            mark_auth_search_healthy();
            return Ok(songs
                .into_iter()
                .map(|song| (song, YoutubeSearchKind::YtmCatalogSong))
                .collect());
        }
        Ok(Ok(_)) => {
            // An authenticated parser can turn a changed YTM response layout into an empty
            // success. Treat that as a soft miss so ordinary YouTube search still works. It is
            // still a completed request, so it resets a preceding hard-failure streak.
            mark_auth_search_healthy();
            tracing::debug!(
                "authenticated search returned no results; using yt-dlp for this query"
            );
        }
        Ok(Err(e)) => {
            // ytmapi-rs response parsers rot as YouTube shifts layouts (0.3.2 is current
            // upstream). Degrade instead of failing the search outright.
            mark_auth_search_degraded();
            tracing::warn!(
                error = %sanitize::sanitize_error_text(format!("{e:#}")),
                "authenticated search failed; using yt-dlp for this query"
            );
        }
        Err(_) => {
            // The YTM continuation stream has no upstream request timeout. Bound the whole
            // authenticated attempt so one stalled page cannot block the serial API actor and
            // every later search behind it.
            mark_auth_search_degraded();
            tracing::warn!(
                timeout_secs = timeout.as_secs_f64(),
                "authenticated search timed out; using yt-dlp for this query"
            );
        }
    }

    Ok(fallback
        .await?
        .into_iter()
        .map(|song| (song, YoutubeSearchKind::YoutubeVideoSearch))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::ytmusic::{
        AUTH_SEARCH_TEST_LOCK, auth_search_degraded, mark_auth_search_degraded,
        mark_auth_search_healthy,
    };

    fn public_result() -> Song {
        Song::remote("aaa111bbb22", "Public result", "Artist", "3:00")
    }

    #[tokio::test]
    async fn empty_and_stalled_authenticated_search_use_public_fallback() {
        let _guard = AUTH_SEARCH_TEST_LOCK.lock().await;
        mark_auth_search_healthy();
        mark_auth_search_degraded();

        let empty = authenticated_youtube_search_with_timeout(
            std::future::ready(Ok(Vec::new())),
            std::future::ready(Ok(vec![public_result()])),
            Duration::from_millis(50),
        )
        .await
        .expect("empty authenticated search should use public fallback");
        assert_eq!(empty[0].0.video_id, "aaa111bbb22");
        assert_eq!(empty[0].1, YoutubeSearchKind::YoutubeVideoSearch);

        let stalled = authenticated_youtube_search_with_timeout(
            std::future::pending::<Result<Vec<Song>>>(),
            std::future::ready(Ok(vec![public_result()])),
            Duration::from_millis(10),
        )
        .await
        .expect("stalled authenticated search should time out into public fallback");
        assert_eq!(stalled[0].0.video_id, "aaa111bbb22");
        assert_eq!(stalled[0].1, YoutubeSearchKind::YoutubeVideoSearch);
        assert!(
            !auth_search_degraded(),
            "the empty success must reset the prior failure before the timeout"
        );

        mark_auth_search_healthy();
    }
}
