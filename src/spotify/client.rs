//! The minimal Spotify Web API client. One central [`SpotifyClient::request_json`] path
//! carries all policy: self-pacing, proactive + reactive token refresh (rotation-safe),
//! 429 handling with a per-job wait budget, message-preserving 403 handling, body caps, and
//! secret-safe errors. Exclusive `&mut self` ownership (one task per client) is what
//! makes refresh single-flight — documented invariant, no mutex.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;

use super::auth::{self, SpotifyToken};
use super::models::{
    Paging, RawPlaylist, RawTrackItem, SpotifyPlaylist, SpotifyPlaylistItemRef, SpotifyTrack,
    SpotifyUser, playlist_item_ref, simplify,
};
use super::{API_BASE, BODY_MAX};
use crate::util::http::json_limited;
use crate::util::sanitize::sanitize_error_text;

/// Hard ceiling on paginated fetches. Far above any real Spotify library (20k pages × 50–100
/// items = 1–2M entries), it exists only to bound a buggy or hostile `next` link that would
/// otherwise loop forever and grow the result Vec without limit.
const MAX_PAGES: u32 = 20_000;
const PAGE_LIMIT: u32 = 50;
/// Reserve from Spotify's reported `total`, but never trust it with an unbounded allocation.
/// This matches the transfer engine's hard work cap and removes repeated Vec growth for the
/// common large-playlist path without letting a hostile envelope request gigabytes.
const MAX_INITIAL_RESERVE_ITEMS: usize = 10_000;

/// Minimum interval between calls (gentle, well under any documented limit).
const PACE: Duration = Duration::from_millis(250);
/// A single 429 wait is capped here (a bigger Retry-After aborts resumably)…
const RATE_WAIT_MAX: Duration = Duration::from_secs(60);
/// …and the cumulative 429 waiting a single client (= job) will absorb.
const RATE_BUDGET: Duration = Duration::from_secs(300);
/// Transient network/5xx retries (short — jobs checkpoint and resume anyway).
const TRANSIENT_RETRIES: u32 = 2;
const TRANSIENT_BACKOFF_BASE: Duration = Duration::from_millis(400);
const TRANSIENT_BACKOFF_JITTER_MAX_MS: u64 = 200;

fn reserve_reported_total<T>(out: &mut Vec<T>, total: u32) {
    let target = reported_reserve_target(total);
    if target <= out.capacity() {
        return;
    }
    let additional = target.saturating_sub(out.len());
    if let Err(error) = out.try_reserve(additional) {
        // Allocation failure is not an API failure: Vec can still grow normally while rows
        // arrive, and the process allocator remains the final authority.
        tracing::debug!(%error, target, "could not preallocate Spotify page results");
    }
}

fn reported_reserve_target(total: u32) -> usize {
    usize::try_from(total)
        .unwrap_or(MAX_INITIAL_RESERVE_ITEMS)
        .min(MAX_INITIAL_RESERVE_ITEMS)
}

fn extend_oldest_playable_from_newest_page(
    oldest_first: &mut Vec<SpotifyTrack>,
    page: &[RawTrackItem],
    max_tracks: usize,
) {
    for item in page.iter().rev() {
        if let Some(track) = simplify(item) {
            oldest_first.push(track);
            if oldest_first.len() >= max_tracks {
                break;
            }
        }
    }
}

fn extend_playlist_page(
    out: &mut Vec<SpotifyTrack>,
    page: &[RawTrackItem],
    max_tracks: Option<usize>,
    skipped: &mut u32,
) {
    for item in page {
        if max_tracks.is_some_and(|max| out.len() >= max) {
            break;
        }
        if let Some(track) = simplify(item) {
            out.push(track);
        } else {
            *skipped = skipped.saturating_add(1);
        }
    }
}

fn response_page_is_monotonic(
    last_end: &mut Option<u32>,
    offset: u32,
    item_count: usize,
    endpoint: &'static str,
) -> Result<(), SpotifyError> {
    let expected = last_end.unwrap_or(0);
    if offset != expected {
        tracing::warn!(
            endpoint,
            offset,
            expected,
            "Spotify pagination returned a non-contiguous page"
        );
        return Err(SpotifyError::Network(format!(
            "{endpoint} pagination expected offset {expected}, got {offset}"
        )));
    }
    let count = u32::try_from(item_count).unwrap_or(u32::MAX);
    *last_end = Some(offset.saturating_add(count));
    Ok(())
}

fn response_total_is_stable(
    expected_total: &mut Option<u32>,
    total: u32,
    endpoint: &'static str,
) -> Result<(), SpotifyError> {
    match *expected_total {
        Some(expected) if expected != total => Err(SpotifyError::Network(format!(
            "{endpoint} pagination total changed from {expected} to {total}"
        ))),
        Some(_) => Ok(()),
        None => {
            *expected_total = Some(total);
            Ok(())
        }
    }
}

fn next_page_url(
    next: Option<String>,
    pages: u32,
    current_offset: u32,
    current_items: usize,
    visited: &mut HashSet<String>,
    endpoint: &'static str,
) -> Result<Option<String>, SpotifyError> {
    let Some(next) = next else {
        return Ok(None);
    };
    if pages >= MAX_PAGES {
        tracing::warn!(pages, endpoint, "Spotify pagination hit the page ceiling");
        return Err(SpotifyError::Network(format!(
            "{endpoint} pagination exceeded {MAX_PAGES} pages"
        )));
    }
    if visited.contains(&next) {
        tracing::warn!(pages, endpoint, "Spotify pagination repeated a next URL");
        return Err(SpotifyError::Network(format!(
            "{endpoint} pagination repeated a continuation"
        )));
    }
    if let Ok(parsed) = reqwest::Url::parse(&next)
        && let Some(next_offset) = parsed.query_pairs().find_map(|(key, value)| {
            (key == "offset")
                .then(|| value.parse::<u32>().ok())
                .flatten()
        })
    {
        let count = u32::try_from(current_items).unwrap_or(u32::MAX);
        let expected_next = current_offset.saturating_add(count);
        if count == 0 || next_offset != expected_next {
            tracing::warn!(
                pages,
                endpoint,
                current_offset,
                next_offset,
                expected_next,
                "Spotify pagination returned a non-contiguous next URL"
            );
            return Err(SpotifyError::Network(format!(
                "{endpoint} pagination expected next offset {expected_next}, got {next_offset}"
            )));
        }
    }
    visited.insert(next.clone());
    Ok(Some(next))
}

fn retry_after_wait(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let requested = match headers.get(reqwest::header::RETRY_AFTER) {
        Some(value) => value.to_str().ok()?.trim().parse::<u64>().ok()?,
        None => 2,
    };
    let wait = Duration::from_secs(requested).max(Duration::from_secs(1));
    (wait <= RATE_WAIT_MAX).then_some(wait)
}

fn can_replay_after_transient_failure(method: &reqwest::Method) -> bool {
    matches!(
        *method,
        reqwest::Method::GET | reqwest::Method::HEAD | reqwest::Method::OPTIONS
    )
}

#[derive(Debug, serde::Deserialize)]
struct PlaylistItemsSnapshot {
    #[serde(default)]
    snapshot_id: Option<String>,
}

fn validate_playlist_item_batch(uris: &[String]) -> Result<(), SpotifyError> {
    if uris.len() <= 100 {
        return Ok(());
    }
    Err(SpotifyError::Api {
        status: 400,
        message: format!(
            "playlist item update contains {} URIs; Spotify accepts at most 100 per request",
            uris.len()
        ),
    })
}

fn transient_backoff(attempt: u32) -> Duration {
    let multiplier = 1u32 << attempt.min(8);
    TRANSIENT_BACKOFF_BASE.saturating_mul(multiplier)
        + Duration::from_millis(fastrand::u64(0..=TRANSIENT_BACKOFF_JITTER_MAX_MS))
}

#[derive(Debug, Clone)]
pub enum SpotifyError {
    /// 401 after a refresh attempt, or the refresh itself failed: reconnect.
    Auth(String),
    /// Explicit Development Mode guidance for callers that have positively identified
    /// an allowlist failure. Generic 403 responses remain `Api` so their sanitized Spotify
    /// message is not discarded or misclassified as an allowlist problem.
    NotAllowlisted,
    /// The per-job 429 budget ran out; the job should checkpoint and abort resumably.
    RateLimited,
    Network(String),
    Api {
        status: u16,
        message: String,
    },
    Decode(String),
}

impl std::fmt::Display for SpotifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpotifyError::Auth(m) => write!(
                f,
                "Spotify session expired ({m}) — reconnect with `ytt auth spotify` or Settings › Accounts"
            ),
            SpotifyError::NotAllowlisted => write!(
                f,
                "Spotify rejected the request (403). Your Spotify app is in Development Mode: open \
                 the Developer Dashboard → your app → User Management and add your own Spotify \
                 account, and double-check the Client ID"
            ),
            SpotifyError::RateLimited => {
                write!(
                    f,
                    "Spotify rate limit exhausted — resume the job in a few minutes"
                )
            }
            SpotifyError::Network(m) => write!(f, "network: {m}"),
            SpotifyError::Api { status, message } => write!(f, "Spotify HTTP {status}: {message}"),
            SpotifyError::Decode(m) => write!(f, "unexpected Spotify response: {m}"),
        }
    }
}

impl std::error::Error for SpotifyError {}

// No `Debug`: holds the bearer token.
pub struct SpotifyClient {
    http: reqwest::Client,
    token: SpotifyToken,
    user: Option<SpotifyUser>,
    last_call: Option<Instant>,
    rate_waited: Duration,
}

impl SpotifyClient {
    /// Build from the saved token. `client_id_hint` (the config value) guards against a
    /// token minted by a different app registration.
    pub fn from_saved(client_id_hint: Option<&str>) -> Result<Self, SpotifyError> {
        let token = SpotifyToken::load()
            .ok_or_else(|| SpotifyError::Auth("not connected (no saved token)".to_owned()))?;
        if let Some(hint) = client_id_hint.map(str::trim).filter(|h| !h.is_empty())
            && !token.client_id.is_empty()
            && token.client_id != hint
        {
            return Err(SpotifyError::Auth(
                "saved token belongs to a different Client ID".to_owned(),
            ));
        }
        Ok(Self::with_token(token))
    }

    pub fn with_token(token: SpotifyToken) -> Self {
        Self {
            http: reqwest::Client::builder()
                .user_agent(format!("yututui/{}", env!("CARGO_PKG_VERSION")))
                .timeout(Duration::from_secs(20))
                .build()
                .unwrap_or_default(),
            token,
            user: None,
            last_call: None,
            rate_waited: Duration::ZERO,
        }
    }

    /// Fresh 429 budget for a new job on a long-lived client (the TUI actor).
    pub fn reset_rate_budget(&mut self) {
        self.rate_waited = Duration::ZERO;
    }

    // Endpoints -----------------------------------------------------------------

    pub async fn me(&mut self) -> Result<SpotifyUser, SpotifyError> {
        if let Some(user) = &self.user {
            return Ok(user.clone());
        }
        let user: SpotifyUser = self
            .request_json(reqwest::Method::GET, format!("{API_BASE}/me"), None)
            .await?;
        self.user = Some(user.clone());
        Ok(user)
    }

    pub async fn my_playlists(&mut self) -> Result<Vec<SpotifyPlaylist>, SpotifyError> {
        let mut out = Vec::new();
        let mut url = format!("{API_BASE}/me/playlists?limit={PAGE_LIMIT}");
        let mut visited = HashSet::from([url.clone()]);
        let mut last_end = None;
        let mut expected_total = None;
        let mut pages = 0u32;
        loop {
            pages += 1;
            let page: Paging<RawPlaylist> =
                self.request_json(reqwest::Method::GET, url, None).await?;
            let item_count = page.items.len();
            response_page_is_monotonic(&mut last_end, page.offset, item_count, "my_playlists")?;
            response_total_is_stable(&mut expected_total, page.total, "my_playlists")?;
            if pages == 1 {
                reserve_reported_total(&mut out, page.total);
            }
            out.extend(page.items.into_iter().map(SpotifyPlaylist::from));
            let Some(next) = next_page_url(
                page.next,
                pages,
                page.offset,
                item_count,
                &mut visited,
                "my_playlists",
            )?
            else {
                return Ok(out);
            };
            url = next;
        }
    }

    pub async fn playlist_meta(&mut self, id: &str) -> Result<SpotifyPlaylist, SpotifyError> {
        // Both generations of the contents-ref name are requested; whichever the server
        // knows survives the fields filter (unknown names are silently omitted).
        let raw: RawPlaylist = self
            .request_json(
                reqwest::Method::GET,
                format!(
                    "{API_BASE}/playlists/{id}?fields=id,name,collaborative,snapshot_id,owner(display_name,id),items(total),tracks(total)"
                ),
                None,
            )
            .await?;
        Ok(raw.into())
    }

    /// Every row in a playlist, in destination order, including episodes, local files,
    /// unknown future item types, and removed/null rows. Unlike [`Self::playlist_tracks`],
    /// this method never filters the envelope's item rows.
    pub async fn playlist_item_refs(
        &mut self,
        id: &str,
        on_page: &mut (dyn FnMut(u32, u32) + Send),
    ) -> Result<Vec<SpotifyPlaylistItemRef>, SpotifyError> {
        let fields = "items(is_local,item(type,uri,is_local)),total,next,offset,limit";
        let mut url = format!("{API_BASE}/playlists/{id}/items?limit={PAGE_LIMIT}&fields={fields}");
        let mut out = Vec::new();
        let mut seen = 0u32;
        let mut visited = HashSet::from([url.clone()]);
        let mut last_end = None;
        let mut expected_total = None;
        let mut pages = 0u32;
        loop {
            pages += 1;
            let page: Paging<RawTrackItem> =
                self.request_json(reqwest::Method::GET, url, None).await?;
            let item_count = page.items.len();
            response_page_is_monotonic(
                &mut last_end,
                page.offset,
                item_count,
                "playlist_item_refs",
            )?;
            response_total_is_stable(&mut expected_total, page.total, "playlist_item_refs")?;
            if pages == 1 {
                reserve_reported_total(&mut out, page.total);
            }
            seen = seen.saturating_add(u32::try_from(item_count).unwrap_or(u32::MAX));
            out.extend(page.items.iter().map(playlist_item_ref));
            on_page(seen, page.total);
            let Some(next) = next_page_url(
                page.next,
                pages,
                page.offset,
                item_count,
                &mut visited,
                "playlist_item_refs",
            )?
            else {
                return Ok(out);
            };
            url = next;
        }
    }

    /// All of a playlist's playable tracks, in order; `on_page(done, total)` reports
    /// pagination progress. Episodes/local/null items are silently skipped (the caller
    /// can compare the returned length with the playlist total to report them).
    ///
    /// Uses the post-March-2026 `/items` endpoint — the old `/tracks` one is 403 for
    /// Development Mode apps.
    pub async fn playlist_tracks(
        &mut self,
        id: &str,
        on_page: &mut (dyn FnMut(u32, u32) + Send),
    ) -> Result<Vec<SpotifyTrack>, SpotifyError> {
        self.playlist_tracks_bounded(id, None, on_page)
            .await
            .map(|(tracks, _)| tracks)
    }

    /// Transfer-only bounded playlist fetch. It stops once `max_tracks` playable rows have
    /// been collected and reports only genuinely unmatchable rows encountered in fetched pages.
    pub(crate) async fn playlist_tracks_for_transfer(
        &mut self,
        id: &str,
        max_tracks: usize,
        on_page: &mut (dyn FnMut(u32, u32) + Send),
    ) -> Result<(Vec<SpotifyTrack>, u32), SpotifyError> {
        self.playlist_tracks_bounded(id, Some(max_tracks), on_page)
            .await
    }

    async fn playlist_tracks_bounded(
        &mut self,
        id: &str,
        max_tracks: Option<usize>,
        on_page: &mut (dyn FnMut(u32, u32) + Send),
    ) -> Result<(Vec<SpotifyTrack>, u32), SpotifyError> {
        if max_tracks == Some(0) {
            return Ok((Vec::new(), 0));
        }
        let fields = "items(added_at,is_local,item(type,id,uri,name,duration_ms,explicit,is_local,is_playable,disc_number,track_number,artists(name,id,uri,external_urls(spotify)),album(id,uri,name,album_type,total_tracks,release_date,release_date_precision,images(url,width,height),artists(name,id,uri,external_urls(spotify)),external_urls(spotify)),external_ids(isrc),external_urls(spotify),restrictions(reason),linked_from(id,uri))),total,next,offset,limit";
        let mut url = format!("{API_BASE}/playlists/{id}/items?limit={PAGE_LIMIT}&fields={fields}");
        let mut out = Vec::new();
        let mut seen = 0u32;
        let mut skipped = 0u32;
        let mut visited = HashSet::from([url.clone()]);
        let mut last_end = None;
        let mut expected_total = None;
        let mut pages = 0u32;
        loop {
            pages += 1;
            let page: Paging<RawTrackItem> =
                self.request_json(reqwest::Method::GET, url, None).await?;
            let item_count = page.items.len();
            response_page_is_monotonic(&mut last_end, page.offset, item_count, "playlist_tracks")?;
            response_total_is_stable(&mut expected_total, page.total, "playlist_tracks")?;
            if pages == 1 {
                let reserve = max_tracks
                    .map(|max| page.total.min(u32::try_from(max).unwrap_or(u32::MAX)))
                    .unwrap_or(page.total);
                reserve_reported_total(&mut out, reserve);
            }
            seen = seen.saturating_add(u32::try_from(item_count).unwrap_or(u32::MAX));
            extend_playlist_page(&mut out, &page.items, max_tracks, &mut skipped);
            let progress_total = max_tracks
                .map(|max| page.total.min(u32::try_from(max).unwrap_or(u32::MAX)))
                .unwrap_or(page.total);
            on_page(seen.min(progress_total), progress_total);
            if max_tracks.is_some_and(|max| out.len() >= max) {
                return Ok((out, skipped));
            }
            let Some(next) = next_page_url(
                page.next,
                pages,
                page.offset,
                item_count,
                &mut visited,
                "playlist_tracks",
            )?
            else {
                return Ok((out, skipped));
            };
            url = next;
        }
    }

    /// Liked songs. Spotify returns newest-first; callers reverse when they need
    /// oldest-first (like-order-preserving import).
    pub async fn liked_tracks(
        &mut self,
        on_page: &mut (dyn FnMut(u32, u32) + Send),
    ) -> Result<Vec<SpotifyTrack>, SpotifyError> {
        let mut url = format!("{API_BASE}/me/tracks?limit={PAGE_LIMIT}");
        let mut out = Vec::new();
        let mut seen = 0u32;
        let mut visited = HashSet::from([url.clone()]);
        let mut last_end = None;
        let mut expected_total = None;
        let mut pages = 0u32;
        loop {
            pages += 1;
            let page: Paging<RawTrackItem> =
                self.request_json(reqwest::Method::GET, url, None).await?;
            let item_count = page.items.len();
            response_page_is_monotonic(&mut last_end, page.offset, item_count, "liked_tracks")?;
            response_total_is_stable(&mut expected_total, page.total, "liked_tracks")?;
            if pages == 1 {
                reserve_reported_total(&mut out, page.total);
            }
            seen = seen.saturating_add(u32::try_from(item_count).unwrap_or(u32::MAX));
            out.extend(page.items.iter().filter_map(simplify));
            on_page(seen, page.total);
            let Some(next) = next_page_url(
                page.next,
                pages,
                page.offset,
                item_count,
                &mut visited,
                "liked_tracks",
            )?
            else {
                return Ok(out);
            };
            url = next;
        }
    }

    /// Fetch the oldest `max_tracks` playable saved tracks without downloading a larger
    /// newest-first library in full. The returned order remains newest-first, matching
    /// [`liked_tracks`], so callers can reverse once for chronological import.
    pub(crate) async fn liked_tracks_for_transfer(
        &mut self,
        max_tracks: usize,
        on_page: &mut (dyn FnMut(u32, u32) + Send),
    ) -> Result<(Vec<SpotifyTrack>, bool), SpotifyError> {
        if max_tracks == 0 {
            return Ok((Vec::new(), false));
        }

        let first_url = format!("{API_BASE}/me/tracks?limit={PAGE_LIMIT}&offset=0");
        let first: Paging<RawTrackItem> = self
            .request_json(reqwest::Method::GET, first_url.clone(), None)
            .await?;
        if first.offset != 0 {
            return Err(SpotifyError::Network(format!(
                "liked_tracks_for_transfer expected first offset 0, got {}",
                first.offset
            )));
        }
        let total = first.total;
        let truncated = usize::try_from(total).unwrap_or(usize::MAX) > max_tracks;
        if usize::try_from(total).unwrap_or(usize::MAX) <= max_tracks {
            let mut out = Vec::new();
            reserve_reported_total(&mut out, total);
            let mut seen = 0u32;
            let mut visited = HashSet::from([first_url]);
            let mut last_end = None;
            let mut expected_total = Some(total);
            let mut pages = 0u32;
            let mut page = first;
            loop {
                pages += 1;
                let item_count = page.items.len();
                response_page_is_monotonic(
                    &mut last_end,
                    page.offset,
                    item_count,
                    "liked_tracks_for_transfer",
                )?;
                response_total_is_stable(
                    &mut expected_total,
                    page.total,
                    "liked_tracks_for_transfer",
                )?;
                seen = seen.saturating_add(u32::try_from(item_count).unwrap_or(u32::MAX));
                out.extend(page.items.iter().filter_map(simplify));
                on_page(seen.min(total), total);
                let Some(next) = next_page_url(
                    page.next,
                    pages,
                    page.offset,
                    item_count,
                    &mut visited,
                    "liked_tracks_for_transfer",
                )?
                else {
                    return Ok((out, truncated));
                };
                page = self.request_json(reqwest::Method::GET, next, None).await?;
            }
        }

        let target = u32::try_from(max_tracks).unwrap_or(u32::MAX).min(total);
        let mut oldest_first = Vec::with_capacity(max_tracks.min(MAX_INITIAL_RESERVE_ITEMS));
        let mut upper = total;
        let mut pages = 0u32;
        let mut requested_offsets = HashSet::new();
        while upper > 0 && oldest_first.len() < max_tracks {
            pages += 1;
            if pages > MAX_PAGES {
                return Err(SpotifyError::Network(format!(
                    "liked_tracks_for_transfer pagination exceeded {MAX_PAGES} pages"
                )));
            }
            let limit = PAGE_LIMIT.min(upper);
            let offset = upper - limit;
            if !requested_offsets.insert(offset) {
                return Err(SpotifyError::Network(
                    "liked_tracks_for_transfer pagination repeated an offset".to_owned(),
                ));
            }
            let url = format!("{API_BASE}/me/tracks?limit={limit}&offset={offset}");
            let page: Paging<RawTrackItem> =
                self.request_json(reqwest::Method::GET, url, None).await?;
            if page.total != total {
                return Err(SpotifyError::Network(format!(
                    "liked_tracks_for_transfer library changed during pagination (expected total {total}, got {})",
                    page.total
                )));
            }
            if page.offset != offset {
                return Err(SpotifyError::Network(format!(
                    "liked_tracks_for_transfer expected offset {offset}, got {}",
                    page.offset
                )));
            }
            if page.items.len() > usize::try_from(limit).unwrap_or(usize::MAX) {
                return Err(SpotifyError::Network(format!(
                    "liked_tracks_for_transfer returned {} rows for limit {limit} at offset {offset}",
                    page.items.len()
                )));
            }
            extend_oldest_playable_from_newest_page(&mut oldest_first, &page.items, max_tracks);
            on_page(
                u32::try_from(oldest_first.len())
                    .unwrap_or(u32::MAX)
                    .min(target),
                target,
            );
            upper = offset;
        }
        oldest_first.reverse();
        Ok((oldest_first, truncated))
    }

    /// Create a private playlist; returns its id.
    pub async fn create_playlist(
        &mut self,
        name: &str,
        description: &str,
    ) -> Result<String, SpotifyError> {
        let body: serde_json::Value = self
            .request_json(
                reqwest::Method::POST,
                format!("{API_BASE}/me/playlists"),
                Some(serde_json::json!({
                    "name": name, "description": description, "public": false,
                })),
            )
            .await?;
        body.get("id")
            .and_then(|i| i.as_str())
            .map(str::to_owned)
            .ok_or_else(|| SpotifyError::Decode("create playlist: no id".to_owned()))
    }

    /// Append ≤100 URIs (caller pre-chunks; order within a call is preserved), returning
    /// Spotify's successor snapshot when present.
    /// Post-migration endpoint; works on existing own playlists even in Development Mode.
    pub async fn add_tracks(
        &mut self,
        playlist_id: &str,
        uris: &[String],
    ) -> Result<Option<String>, SpotifyError> {
        validate_playlist_item_batch(uris)?;
        let snapshot: PlaylistItemsSnapshot = self
            .request_json(
                reqwest::Method::POST,
                format!("{API_BASE}/playlists/{playlist_id}/items"),
                Some(serde_json::json!({ "uris": uris })),
            )
            .await?;
        Ok(snapshot.snapshot_id.filter(|id| !id.is_empty()))
    }

    /// Replace a playlist with at most 100 initial URIs, returning Spotify's successor
    /// snapshot when present. An empty slice is intentional and clears the playlist.
    pub async fn replace_playlist_items(
        &mut self,
        playlist_id: &str,
        uris: &[String],
    ) -> Result<Option<String>, SpotifyError> {
        validate_playlist_item_batch(uris)?;
        let snapshot: PlaylistItemsSnapshot = self
            .request_json(
                reqwest::Method::PUT,
                format!("{API_BASE}/playlists/{playlist_id}/items"),
                Some(serde_json::json!({ "uris": uris })),
            )
            .await?;
        Ok(snapshot.snapshot_id.filter(|id| !id.is_empty()))
    }

    /// Track search for the YTM→Spotify direction, top 10.
    pub async fn search_track(
        &mut self,
        query: &str,
        market: Option<&str>,
    ) -> Result<Vec<SpotifyTrack>, SpotifyError> {
        let mut url = reqwest::Url::parse(&format!("{API_BASE}/search"))
            .map_err(|e| SpotifyError::Decode(e.to_string()))?;
        url.query_pairs_mut()
            .append_pair("type", "track")
            .append_pair("limit", "10")
            .append_pair("q", query);
        if let Some(market) = market.map(str::trim).filter(|m| !m.is_empty()) {
            url.query_pairs_mut().append_pair("market", market);
        }
        let body: serde_json::Value = self
            .request_json(reqwest::Method::GET, url.to_string(), None)
            .await?;
        let items: Vec<RawTrackItem> = body
            .pointer("/tracks/items")
            .and_then(|i| i.as_array())
            .map(|arr| {
                arr.iter()
                    .map(|t| RawTrackItem {
                        track: Some(t.clone()),
                        ..RawTrackItem::default()
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(items.iter().filter_map(simplify).collect())
    }

    // The one central request path --------------------------------------------------

    async fn request_json<T: DeserializeOwned>(
        &mut self,
        method: reqwest::Method,
        url: String,
        body: Option<serde_json::Value>,
    ) -> Result<T, SpotifyError> {
        let mut refreshed = false;
        let mut transient_left = TRANSIENT_RETRIES;
        let mut transient_attempt = 0u32;
        let replay_safe = can_replay_after_transient_failure(&method);
        loop {
            // Self-pace every call.
            if let Some(last) = self.last_call {
                let since = last.elapsed();
                if since < PACE {
                    tokio::time::sleep(PACE - since).await;
                }
            }
            // Proactive refresh keeps the reactive 401 path a rarity.
            if self.token.is_expired() && !refreshed {
                self.refresh().await?;
                refreshed = true;
            }
            self.last_call = Some(Instant::now());

            let mut bearer = reqwest::header::HeaderValue::from_str(&format!(
                "Bearer {}",
                self.token.access_token
            ))
            .map_err(|_| SpotifyError::Auth("token has invalid characters".to_owned()))?;
            bearer.set_sensitive(true);
            let mut req = self
                .http
                .request(method.clone(), &url)
                .header(reqwest::header::AUTHORIZATION, bearer);
            if let Some(body) = &body {
                req = req.json(body);
            }
            let resp = match req.send().await {
                Ok(resp) => resp,
                Err(error) if replay_safe && transient_left > 0 => {
                    let wait = transient_backoff(transient_attempt);
                    transient_left -= 1;
                    transient_attempt += 1;
                    tracing::debug!(
                        attempt = transient_attempt,
                        wait_ms = wait.as_millis(),
                        error = %sanitize_error_text(format!("{error}")),
                        "retrying transient Spotify network failure"
                    );
                    tokio::time::sleep(wait).await;
                    continue;
                }
                Err(e) => {
                    return Err(SpotifyError::Network(sanitize_error_text(format!("{e}"))));
                }
            };
            let status = resp.status();

            if status == reqwest::StatusCode::UNAUTHORIZED && !refreshed {
                self.refresh().await?;
                refreshed = true;
                continue;
            }
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                let Some(wait) = retry_after_wait(resp.headers()) else {
                    return Err(SpotifyError::RateLimited);
                };
                let total_wait = self.rate_waited.saturating_add(wait);
                if total_wait > RATE_BUDGET {
                    return Err(SpotifyError::RateLimited);
                }
                self.rate_waited = total_wait;
                tracing::info!(secs = wait.as_secs(), "spotify 429; waiting");
                tokio::time::sleep(wait).await;
                continue;
            }
            if (status.is_server_error() || status == reqwest::StatusCode::REQUEST_TIMEOUT)
                && replay_safe
                && transient_left > 0
            {
                let wait = transient_backoff(transient_attempt);
                transient_left -= 1;
                transient_attempt += 1;
                tracing::debug!(
                    attempt = transient_attempt,
                    wait_ms = wait.as_millis(),
                    status = status.as_u16(),
                    "retrying transient Spotify HTTP failure"
                );
                tokio::time::sleep(wait).await;
                continue;
            }
            if !status.is_success() {
                let body: serde_json::Value =
                    json_limited(resp, BODY_MAX).await.unwrap_or_default();
                let message = sanitize_error_text(
                    body.pointer("/error/message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("unknown"),
                );
                return Err(match status.as_u16() {
                    401 => SpotifyError::Auth(message),
                    code => SpotifyError::Api {
                        status: code,
                        message,
                    },
                });
            }
            // 201/200 with body; DELETE/PUT snapshots also return small JSON.
            return json_limited(resp, BODY_MAX)
                .await
                .map_err(|e| SpotifyError::Decode(sanitize_error_text(format!("{e:#}"))));
        }
    }

    /// Rotation-safe refresh: `auth::refresh` persists the successor before returning.
    async fn refresh(&mut self) -> Result<(), SpotifyError> {
        match auth::refresh(&self.http, &self.token).await {
            Ok(fresh) => {
                self.token = fresh;
                Ok(())
            }
            Err(e) => Err(SpotifyError::Auth(sanitize_error_text(format!("{e:#}")))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn token() -> SpotifyToken {
        SpotifyToken {
            access_token: "access".to_owned(),
            refresh_token: "refresh".to_owned(),
            expires_at: crate::signals::unix_now() + 3600,
            scopes: "scope-a scope-b".to_owned(),
            client_id: "client-id".to_owned(),
        }
    }

    #[test]
    fn spotify_error_display_gives_actionable_messages() {
        assert!(
            SpotifyError::Auth("expired".to_owned())
                .to_string()
                .contains("ytt auth spotify")
        );
        assert!(
            SpotifyError::NotAllowlisted
                .to_string()
                .contains("User Management")
        );
        assert!(
            SpotifyError::RateLimited
                .to_string()
                .contains("resume the job")
        );
        assert_eq!(
            SpotifyError::Network("timeout".to_owned()).to_string(),
            "network: timeout"
        );
        assert_eq!(
            SpotifyError::Api {
                status: 418,
                message: "teapot".to_owned(),
            }
            .to_string(),
            "Spotify HTTP 418: teapot"
        );
        assert_eq!(
            SpotifyError::Decode("bad json".to_owned()).to_string(),
            "unexpected Spotify response: bad json"
        );
    }

    #[test]
    fn client_with_token_starts_without_cached_user_or_rate_debt() {
        let client = SpotifyClient::with_token(token());

        assert!(client.user.is_none());
        assert!(client.last_call.is_none());
        assert_eq!(client.rate_waited, Duration::ZERO);
        assert_eq!(client.token.client_id, "client-id");
    }

    #[test]
    fn reset_rate_budget_clears_cumulative_wait_without_touching_token() {
        let mut client = SpotifyClient::with_token(token());
        client.rate_waited = Duration::from_secs(42);

        client.reset_rate_budget();

        assert_eq!(client.rate_waited, Duration::ZERO);
        assert_eq!(client.token.refresh_token, "refresh");
    }

    #[test]
    fn reported_page_total_preallocation_is_bounded_by_the_work_cap() {
        assert_eq!(reported_reserve_target(0), 0);
        assert_eq!(reported_reserve_target(237), 237);
        assert_eq!(reported_reserve_target(u32::MAX), MAX_INITIAL_RESERVE_ITEMS);

        let mut rows = Vec::<u8>::new();
        reserve_reported_total(&mut rows, 237);
        assert!(rows.capacity() >= 237);
    }

    fn raw_track(name: &str) -> RawTrackItem {
        serde_json::from_value(serde_json::json!({
            "track": {
                "type": "track",
                "uri": format!("spotify:track:{name}"),
                "name": name,
                "artists": [],
                "album": { "name": "Album", "artists": [] }
            }
        }))
        .expect("raw track fixture")
    }

    #[test]
    fn bounded_playlist_page_fold_keeps_first_playable_rows_and_exact_skips() {
        let invalid = RawTrackItem::default();
        let mut out = Vec::new();
        let mut skipped = 0;

        extend_playlist_page(
            &mut out,
            &[raw_track("a"), invalid.clone(), raw_track("b")],
            Some(3),
            &mut skipped,
        );
        extend_playlist_page(
            &mut out,
            &[invalid, raw_track("c"), raw_track("beyond-cap")],
            Some(3),
            &mut skipped,
        );

        assert_eq!(
            out.iter()
                .map(|track| track.name.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b", "c"]
        );
        assert_eq!(skipped, 2);
    }

    #[test]
    fn liked_tail_page_fold_returns_oldest_selection_in_newest_first_api_order() {
        let invalid = RawTrackItem::default();
        let mut oldest_first = Vec::new();

        // Tail pages arrive newest-first while pagination walks from the end backwards.
        // Invalid old rows force another page; only the oldest three playable rows survive.
        extend_oldest_playable_from_newest_page(
            &mut oldest_first,
            &[raw_track("newer-tail"), invalid],
            3,
        );
        extend_oldest_playable_from_newest_page(
            &mut oldest_first,
            &[raw_track("newer"), raw_track("middle")],
            3,
        );
        assert_eq!(
            oldest_first
                .iter()
                .map(|track| track.name.as_str())
                .collect::<Vec<_>>(),
            vec!["newer-tail", "middle", "newer"]
        );

        oldest_first.reverse();
        assert_eq!(
            oldest_first
                .iter()
                .map(|track| track.name.as_str())
                .collect::<Vec<_>>(),
            vec!["newer", "middle", "newer-tail"]
        );
    }

    #[test]
    fn pagination_guards_require_contiguous_offsets_stable_totals_and_unique_urls() {
        let mut wrong_first = None;
        assert!(response_page_is_monotonic(&mut wrong_first, 5, 50, "test").is_err());

        let mut last_end = None;
        assert!(response_page_is_monotonic(&mut last_end, 0, 50, "test").is_ok());
        assert!(response_page_is_monotonic(&mut last_end, 50, 50, "test").is_ok());
        assert!(response_page_is_monotonic(&mut last_end, 75, 50, "test").is_err());
        assert!(response_page_is_monotonic(&mut last_end, 150, 50, "test").is_err());

        let mut expected_total = None;
        assert!(response_total_is_stable(&mut expected_total, 100, "test").is_ok());
        assert!(response_total_is_stable(&mut expected_total, 100, "test").is_ok());
        assert!(response_total_is_stable(&mut expected_total, 99, "test").is_err());

        let initial = "https://api.spotify.com/v1/me/tracks?offset=0&limit=50".to_owned();
        let next = "https://api.spotify.com/v1/me/tracks?offset=50&limit=50".to_owned();
        let mut visited = HashSet::from([initial]);
        assert_eq!(
            next_page_url(Some(next.clone()), 1, 0, 50, &mut visited, "test")
                .expect("monotonic continuation"),
            Some(next.clone())
        );
        assert!(
            next_page_url(Some(next), 2, 50, 50, &mut visited, "test").is_err(),
            "a repeated next URL must stop pagination"
        );

        let mut visited = HashSet::new();
        assert!(
            next_page_url(
                Some("https://api.spotify.com/v1/me/tracks?offset=49&limit=50".to_owned()),
                1,
                0,
                50,
                &mut visited,
                "test"
            )
            .is_err(),
            "overlapping offset must stop pagination"
        );
        let mut visited = HashSet::new();
        assert!(
            next_page_url(
                Some("https://api.spotify.com/v1/me/tracks?offset=100&limit=50".to_owned()),
                1,
                0,
                50,
                &mut visited,
                "test"
            )
            .is_err(),
            "a continuation gap must stop pagination"
        );
    }

    #[test]
    fn retry_after_and_transient_retry_policies_are_bounded_and_replay_safe() {
        let mut headers = reqwest::header::HeaderMap::new();
        assert_eq!(retry_after_wait(&headers), Some(Duration::from_secs(2)));

        headers.insert(reqwest::header::RETRY_AFTER, "0".parse().unwrap());
        assert_eq!(retry_after_wait(&headers), Some(Duration::from_secs(1)));
        headers.insert(reqwest::header::RETRY_AFTER, "60".parse().unwrap());
        assert_eq!(retry_after_wait(&headers), Some(RATE_WAIT_MAX));
        headers.insert(reqwest::header::RETRY_AFTER, "61".parse().unwrap());
        assert_eq!(retry_after_wait(&headers), None);
        headers.insert(
            reqwest::header::RETRY_AFTER,
            "not-delay-seconds".parse().unwrap(),
        );
        assert_eq!(retry_after_wait(&headers), None);

        assert!(can_replay_after_transient_failure(&reqwest::Method::GET));
        assert!(!can_replay_after_transient_failure(&reqwest::Method::POST));
        let first = transient_backoff(0);
        assert!(first >= TRANSIENT_BACKOFF_BASE);
        assert!(
            first
                <= TRANSIENT_BACKOFF_BASE + Duration::from_millis(TRANSIENT_BACKOFF_JITTER_MAX_MS)
        );
        let second = transient_backoff(1);
        assert!(second >= TRANSIENT_BACKOFF_BASE.saturating_mul(2));
        assert!(
            second
                <= TRANSIENT_BACKOFF_BASE.saturating_mul(2)
                    + Duration::from_millis(TRANSIENT_BACKOFF_JITTER_MAX_MS)
        );
    }

    #[test]
    fn playlist_item_batches_enforce_spotifys_hundred_uri_limit() {
        assert!(validate_playlist_item_batch(&[]).is_ok());
        let hundred: [String; 100] = std::array::from_fn(|_| "uri".to_owned());
        assert!(validate_playlist_item_batch(&hundred).is_ok());
        let oversized: [String; 101] = std::array::from_fn(|_| "uri".to_owned());
        let error = validate_playlist_item_batch(&oversized)
            .expect_err("oversized write must fail before making a request");
        assert!(matches!(
            error,
            SpotifyError::Api {
                status: 400,
                ref message
            } if message.contains("at most 100")
        ));
    }

    async fn read_spotify_test_request(stream: &mut tokio::net::TcpStream) -> String {
        let mut request = Vec::new();
        let mut buf = [0u8; 1024];
        loop {
            let n = stream.read(&mut buf).await.expect("read spotify request");
            if n == 0 {
                break;
            }
            request.extend_from_slice(&buf[..n]);
            if let Some(head_end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&request[..head_end + 4]);
                let content_len = head
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                if request.len() >= head_end + 4 + content_len {
                    break;
                }
            }
        }
        String::from_utf8_lossy(&request).into_owned()
    }

    async fn write_spotify_test_response(
        stream: &mut tokio::net::TcpStream,
        status: &str,
        headers: &[(String, String)],
        body: &str,
    ) {
        let mut response = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n",
            body.len()
        );
        for (name, value) in headers {
            response.push_str(&format!("{name}: {value}\r\n"));
        }
        response.push_str("\r\n");
        response.push_str(body);
        stream
            .write_all(response.as_bytes())
            .await
            .expect("write spotify response");
        let _ = stream.shutdown().await;
    }

    async fn serve_spotify_once(
        status: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> (String, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind spotify test server");
        let addr = listener.local_addr().expect("spotify test server address");
        let status = status.to_owned();
        let headers: Vec<(String, String)> = headers
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect();
        let body = body.to_owned();
        let handle = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept spotify request");
            let request = read_spotify_test_request(&mut stream).await;
            write_spotify_test_response(&mut stream, &status, &headers, &body).await;
            request
        });
        (format!("http://{addr}/v1/test"), handle)
    }

    async fn serve_transient_then_success() -> (String, tokio::task::JoinHandle<Vec<String>>) {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind spotify retry test server");
        let addr = listener
            .local_addr()
            .expect("spotify retry test server address");
        let handle = tokio::spawn(async move {
            let mut requests = Vec::with_capacity(2);
            for (status, body) in [
                (
                    "503 Service Unavailable",
                    r#"{"error":{"message":"temporary"}}"#,
                ),
                ("200 OK", r#"{"ok":true}"#),
            ] {
                let (mut stream, _) = listener.accept().await.expect("accept retry request");
                requests.push(read_spotify_test_request(&mut stream).await);
                write_spotify_test_response(
                    &mut stream,
                    status,
                    &[("Content-Type".to_owned(), "application/json".to_owned())],
                    body,
                )
                .await;
            }
            requests
        });
        (format!("http://{addr}/v1/test"), handle)
    }

    #[tokio::test]
    async fn request_json_sends_bearer_json_and_decodes_success() {
        let (url, request) = serve_spotify_once(
            "201 Created",
            &[("Content-Type", "application/json")],
            r#"{"ok":true,"id":"new-playlist"}"#,
        )
        .await;
        let mut client = SpotifyClient::with_token(token());

        let body: serde_json::Value = client
            .request_json(
                reqwest::Method::POST,
                url,
                Some(serde_json::json!({"name": "Roadtrip"})),
            )
            .await
            .expect("successful response decodes");

        assert_eq!(body["id"], "new-playlist");
        assert!(client.last_call.is_some());
        let request = request.await.expect("captured request");
        assert!(request.starts_with("POST /v1/test HTTP/1.1"));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer access")
        );
        assert!(request.contains(r#""name":"Roadtrip""#));
    }

    #[tokio::test]
    async fn request_json_sends_put_and_decodes_playlist_snapshot() {
        let (url, request) = serve_spotify_once(
            "200 OK",
            &[("Content-Type", "application/json")],
            r#"{"snapshot_id":"successor-snapshot"}"#,
        )
        .await;
        let mut client = SpotifyClient::with_token(token());

        let snapshot: PlaylistItemsSnapshot = client
            .request_json(
                reqwest::Method::PUT,
                url,
                Some(serde_json::json!({"uris": Vec::<String>::new()})),
            )
            .await
            .expect("empty replacement request succeeds");

        assert_eq!(snapshot.snapshot_id.as_deref(), Some("successor-snapshot"));
        let request = request.await.expect("captured PUT request");
        assert!(request.starts_with("PUT /v1/test HTTP/1.1"));
        assert!(request.contains(r#""uris":[]"#));
    }

    #[tokio::test]
    async fn request_json_retries_replay_safe_get_after_transient_server_failure() {
        let (url, requests) = serve_transient_then_success().await;
        let mut client = SpotifyClient::with_token(token());

        let body: serde_json::Value = client
            .request_json(reqwest::Method::GET, url, None)
            .await
            .expect("GET should recover after one bounded transient retry");

        assert_eq!(body["ok"], true);
        let requests = requests.await.expect("captured retry requests");
        assert_eq!(requests.len(), 2);
        assert!(requests.iter().all(|request| request.starts_with("GET ")));
    }

    #[tokio::test]
    async fn request_json_maps_forbidden_api_decode_and_rate_errors() {
        let (url, request) = serve_spotify_once(
            "403 Forbidden",
            &[("Content-Type", "application/json")],
            r#"{"error":{"message":"insufficient scope; access_token=secret-value"}}"#,
        )
        .await;
        let mut client = SpotifyClient::with_token(token());
        let err = client
            .request_json::<serde_json::Value>(reqwest::Method::GET, url, None)
            .await
            .expect_err("403 remains a message-preserving API error");
        assert!(matches!(
            err,
            SpotifyError::Api {
                status: 403,
                ref message
            } if message.contains("insufficient scope")
                && message.contains("access_token=<redacted>")
                && !message.contains("secret-value")
        ));
        assert!(request.await.expect("captured 403 request").contains("GET"));

        let (url, _) = serve_spotify_once(
            "418 I'm a teapot",
            &[("Content-Type", "application/json")],
            r#"{"error":{"message":"short and stout"}}"#,
        )
        .await;
        let mut client = SpotifyClient::with_token(token());
        let err = client
            .request_json::<serde_json::Value>(reqwest::Method::GET, url, None)
            .await
            .expect_err("non-special HTTP error maps to Api");
        assert!(matches!(
            err,
            SpotifyError::Api {
                status: 418,
                ref message
            } if message == "short and stout"
        ));

        let (url, _) = serve_spotify_once(
            "200 OK",
            &[("Content-Type", "application/json")],
            "not-json",
        )
        .await;
        let mut client = SpotifyClient::with_token(token());
        let err = client
            .request_json::<serde_json::Value>(reqwest::Method::GET, url, None)
            .await
            .expect_err("bad JSON maps to Decode");
        assert!(matches!(err, SpotifyError::Decode(_)));

        let (url, _) = serve_spotify_once(
            "429 Too Many Requests",
            &[("Content-Type", "application/json"), ("Retry-After", "1")],
            r#"{"error":{"message":"slow down"}}"#,
        )
        .await;
        let mut client = SpotifyClient::with_token(token());
        client.rate_waited = RATE_BUDGET;
        let err = client
            .request_json::<serde_json::Value>(reqwest::Method::GET, url, None)
            .await
            .expect_err("exhausted rate budget returns immediately");
        assert!(matches!(err, SpotifyError::RateLimited));
    }

    #[tokio::test]
    async fn request_json_aborts_when_retry_after_exceeds_single_wait_cap() {
        let (url, request) = serve_spotify_once(
            "429 Too Many Requests",
            &[("Content-Type", "application/json"), ("Retry-After", "61")],
            r#"{"error":{"message":"wait longer"}}"#,
        )
        .await;
        let mut client = SpotifyClient::with_token(token());

        let err = client
            .request_json::<serde_json::Value>(reqwest::Method::GET, url, None)
            .await
            .expect_err("over-cap Retry-After must defer instead of retrying early");

        assert!(matches!(err, SpotifyError::RateLimited));
        assert_eq!(client.rate_waited, Duration::ZERO);
        assert!(request.await.expect("captured 429 request").contains("GET"));
    }

    #[tokio::test]
    async fn request_json_does_not_replay_ambiguous_post_on_server_failure() {
        let (url, request) = serve_spotify_once(
            "503 Service Unavailable",
            &[("Content-Type", "application/json")],
            r#"{"error":{"message":"try later"}}"#,
        )
        .await;
        let mut client = SpotifyClient::with_token(token());

        let err = client
            .request_json::<serde_json::Value>(
                reqwest::Method::POST,
                url,
                Some(serde_json::json!({"uris": ["spotify:track:test"]})),
            )
            .await
            .expect_err("ambiguous POST failure must not be replayed automatically");

        assert!(matches!(
            err,
            SpotifyError::Api {
                status: 503,
                ref message
            } if message == "try later"
        ));
        assert!(
            request
                .await
                .expect("captured 503 request")
                .contains("POST")
        );
    }
}
