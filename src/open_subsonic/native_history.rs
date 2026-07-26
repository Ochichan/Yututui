//! Bounded, credential-owning access to Navidrome's experimental native history API.

use std::collections::BTreeSet;
use std::error::Error as _;
use std::time::Duration;

use age::secrecy::{ExposeSecret as _, SecretString};
use reqwest::header::{ACCEPT, CONTENT_TYPE, LOCATION};
use reqwest::{Response, StatusCode};
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, Zeroizing};

use super::model::ItemId;
use super::origin::{OriginError, PinnedOriginClient};
use super::profile::OpenSubsonicProfile;

pub const NATIVE_HISTORY_PAGE_SIZE: usize = 200;
pub const MAX_NATIVE_HISTORY_PAGES: usize = 100;
pub const MAX_NATIVE_HISTORY_ROWS: usize = NATIVE_HISTORY_PAGE_SIZE * MAX_NATIVE_HISTORY_PAGES;
const NATIVE_HISTORY_HEAD_PAGE_BUDGET: usize = MAX_NATIVE_HISTORY_PAGES / 2;
/// Offset remains bounded independently from the per-refresh row budget so a large same-second
/// slice can make progress over several refreshes without relaxing that budget.
pub(crate) const MAX_NATIVE_HISTORY_OFFSET: usize = 20_000_000;

const MAX_LOGIN_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_PAGE_RESPONSE_BYTES: usize = 512 * 1024;
const MAX_LOCATION_BYTES: usize = 4_096;
const MAX_USERNAME_BYTES: usize = 1_024;
const MAX_PASSWORD_BYTES: usize = 64 * 1024;
const MAX_TOKEN_BYTES: usize = 16 * 1024;
const MAX_REDIRECTS: usize = 3;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const AUTHORIZATION_HEADER: &str = "x-nd-authorization";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeHistoryError {
    InvalidCredential,
    InvalidRequest,
    Offline,
    CertificateFailed,
    OriginRejected,
    AuthenticationRequired,
    PermissionDenied,
    UnsupportedFeature,
    InvalidResponse,
    ResponseTooLarge,
    TemporarilyUnavailable,
}

impl std::fmt::Display for NativeHistoryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::InvalidCredential => "native history credentials are invalid",
            Self::InvalidRequest => "native history request is invalid",
            Self::Offline => "music server is offline",
            Self::CertificateFailed => "music server certificate could not be verified",
            Self::OriginRejected => "music server address was rejected",
            Self::AuthenticationRequired => "native history password needs updating",
            Self::PermissionDenied => "music server denied native history access",
            Self::UnsupportedFeature => "music server does not support detailed history",
            Self::InvalidResponse => "music server returned invalid detailed history",
            Self::ResponseTooLarge => "music server history exceeded the safety limit",
            Self::TemporarilyUnavailable => "detailed history is temporarily unavailable",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for NativeHistoryError {}

/// Password authentication for Navidrome's native API. Deliberately not `Debug` or `Clone`.
pub struct NativeHistoryCredential {
    username: SecretString,
    password: SecretString,
}

impl NativeHistoryCredential {
    pub fn new(
        username: impl Into<String>,
        password: SecretString,
    ) -> Result<Self, NativeHistoryError> {
        let username = username.into();
        validate_secret_part(&username, MAX_USERNAME_BYTES)?;
        validate_secret_part(password.expose_secret(), MAX_PASSWORD_BYTES)?;
        Ok(Self {
            username: SecretString::from(username),
            password,
        })
    }
}

/// An in-memory native API session. The JWT is never serialized and the type is not `Debug`.
pub struct NativeHistorySession {
    token: Zeroizing<String>,
}

impl NativeHistorySession {
    fn new(token: String) -> Result<Self, NativeHistoryError> {
        validate_token(&token)?;
        Ok(Self {
            token: Zeroizing::new(token),
        })
    }

    fn authorization(&self) -> Result<Zeroizing<String>, NativeHistoryError> {
        let value = Zeroizing::new(format!("Bearer {}", self.token.as_str()));
        reqwest::header::HeaderValue::from_str(&value)
            .map_err(|_| NativeHistoryError::InvalidResponse)?;
        Ok(value)
    }

    fn replace_token(&mut self, token: &str) -> Result<(), NativeHistoryError> {
        validate_token(token)?;
        self.token.zeroize();
        self.token.push_str(token);
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeScrobbleRow {
    pub id: u64,
    pub media_file_id: ItemId,
    /// Seconds since the Unix epoch, as supplied by Navidrome's native API.
    pub submission_time_unix: u64,
}

impl NativeScrobbleRow {
    pub fn submission_time_unix_millis(&self) -> u64 {
        self.submission_time_unix * 1_000
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeScrobblePageRequest {
    start: usize,
    limit: usize,
    from_unix: Option<u64>,
    through_unix: Option<u64>,
}

impl NativeScrobblePageRequest {
    pub fn new(start: usize, limit: usize) -> Result<Self, NativeHistoryError> {
        if limit == 0
            || limit > NATIVE_HISTORY_PAGE_SIZE
            || start >= MAX_NATIVE_HISTORY_OFFSET
            || start.saturating_add(limit) > MAX_NATIVE_HISTORY_OFFSET
        {
            return Err(NativeHistoryError::InvalidRequest);
        }
        Ok(Self {
            start,
            limit,
            from_unix: None,
            through_unix: None,
        })
    }

    pub fn with_time_window(
        mut self,
        from_unix: Option<u64>,
        through_unix: Option<u64>,
    ) -> Result<Self, NativeHistoryError> {
        if matches!((from_unix, through_unix), (Some(from), Some(through)) if from > through) {
            return Err(NativeHistoryError::InvalidRequest);
        }
        self.from_unix = from_unix;
        self.through_unix = through_unix;
        Ok(self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeScrobblePage {
    pub rows: Vec<NativeScrobbleRow>,
    pub next_start: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeScrobbleScan {
    pub rows: Vec<NativeScrobbleRow>,
    /// Largest immutable row ID observed during the scan. Submission timestamps are
    /// client-supplied, so the first timestamp-sorted row is not necessarily the newest ID.
    pub next_high_water_id: Option<u64>,
    pub reached_high_water: bool,
    pub truncated: bool,
    pub continuation: Option<NativeScrobbleScanContinuation>,
}

/// Resume point for a scan that used its bounded page/row budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeScrobbleScanContinuation {
    pub candidate_high_water_id: Option<u64>,
    pub next_start: usize,
    pub through_unix: Option<u64>,
    pub reached_high_water: bool,
    /// The next inclusive time window can repeat these preceding-page rows.
    pub overlap_row_ids: Vec<u64>,
    /// The original backlog may finish while a newer head sweep is still in progress.
    pub backlog_complete: bool,
    /// Stable insertion-ID threshold for a durable head sweep. The candidate high-water is not
    /// promoted into this threshold until the entire bounded overlap has been scanned.
    pub head_anchor_high_water_id: Option<u64>,
    /// `Some` means a head sweep is active; zero is therefore distinct from no sweep.
    pub head_next_start: Option<usize>,
    /// Fixed lower time bound captured when the head sweep started.
    pub head_from_unix: Option<u64>,
    pub head_through_unix: Option<u64>,
    pub head_overlap_row_ids: Vec<u64>,
}

fn valid_continuation(
    continuation: &NativeScrobbleScanContinuation,
    high_water_id: Option<u64>,
) -> bool {
    let valid_overlap = |ids: &[u64]| {
        ids.len() <= NATIVE_HISTORY_PAGE_SIZE
            && ids.iter().copied().collect::<BTreeSet<_>>().len() == ids.len()
    };
    if continuation.next_start >= MAX_NATIVE_HISTORY_OFFSET
        || !valid_overlap(&continuation.overlap_row_ids)
        || matches!(
            (continuation.candidate_high_water_id, high_water_id),
            (Some(candidate), Some(high_water)) if candidate < high_water
        )
    {
        return false;
    }
    let head_active = continuation.head_next_start.is_some();
    if !head_active
        && (continuation.head_anchor_high_water_id.is_some()
            || continuation.head_from_unix.is_some()
            || continuation.head_through_unix.is_some()
            || !continuation.head_overlap_row_ids.is_empty())
    {
        return false;
    }
    if let Some(head_next_start) = continuation.head_next_start
        && (head_next_start >= MAX_NATIVE_HISTORY_OFFSET
            || !valid_overlap(&continuation.head_overlap_row_ids)
            || matches!(
                (
                    continuation.head_anchor_high_water_id,
                    continuation.candidate_high_water_id,
                ),
                (Some(anchor), Some(candidate)) if anchor > candidate
            )
            || matches!(
                (
                    continuation.head_anchor_high_water_id,
                    continuation.candidate_high_water_id,
                ),
                (Some(_), None)
            )
            || matches!(
                (continuation.head_from_unix, continuation.head_through_unix),
                (Some(from), Some(through)) if from > through
            ))
    {
        return false;
    }
    true
}

fn collect_scan_row(
    row: NativeScrobbleRow,
    new_after_id: Option<u64>,
    high_water_id: Option<u64>,
    next_high_water_id: &mut Option<u64>,
    reached_high_water: &mut bool,
    seen: &mut BTreeSet<u64>,
    rows: &mut Vec<NativeScrobbleRow>,
) {
    *next_high_water_id = Some(next_high_water_id.map_or(row.id, |current| current.max(row.id)));
    if high_water_id == Some(row.id) {
        *reached_high_water = true;
    }
    if new_after_id.is_none_or(|known| row.id > known)
        && seen.insert(row.id)
        && rows.len() < MAX_NATIVE_HISTORY_ROWS
    {
        rows.push(row);
    }
}

fn advance_scan_lane(
    start: &mut usize,
    through_unix: &mut Option<u64>,
    oldest_timestamp: Option<u64>,
) -> Result<(), NativeHistoryError> {
    let oldest_timestamp = oldest_timestamp.ok_or(NativeHistoryError::InvalidResponse)?;
    if through_unix.is_none_or(|through| oldest_timestamp < through) {
        // Re-anchor at the oldest timestamp from the preceding page. Inclusive overlap makes
        // head insertions unable to shift unseen older rows past the next offset.
        *through_unix = Some(oldest_timestamp);
        *start = 0;
    } else {
        // More than one page may share the same second. Advance inside that stable slice.
        *start = start
            .checked_add(NATIVE_HISTORY_PAGE_SIZE)
            .ok_or(NativeHistoryError::InvalidRequest)?;
    }
    Ok(())
}

/// DNS-pinned Navidrome native transport. It owns no long-lived password or JWT.
pub struct NavidromeNativeClient {
    transport: PinnedOriginClient,
}

impl NavidromeNativeClient {
    pub async fn connect(profile: &OpenSubsonicProfile) -> Result<Self, NativeHistoryError> {
        let transport = profile
            .origin()
            .build_pinned_client(profile.custom_ca_pem())
            .await
            .map_err(map_origin_error)?;
        Ok(Self { transport })
    }

    pub async fn login(
        &self,
        credential: &NativeHistoryCredential,
    ) -> Result<NativeHistorySession, NativeHistoryError> {
        let mut target = self
            .transport
            .origin()
            .native_endpoint("auth/login")
            .map_err(map_origin_error)?;
        for redirects in 0..=MAX_REDIRECTS {
            let body = LoginRequest {
                username: credential.username.expose_secret(),
                password: credential.password.expose_secret(),
            };
            let response = self
                .transport
                .client()
                .post(target.clone())
                .timeout(REQUEST_TIMEOUT)
                .header(ACCEPT, "application/json")
                .header(CONTENT_TYPE, "application/json")
                .json(&body)
                .send()
                .await
                .map_err(|error| classify_request_error(&error))?;
            if response.status().is_redirection() {
                if redirects == MAX_REDIRECTS {
                    return Err(NativeHistoryError::OriginRejected);
                }
                target = redirect_target(&self.transport, &response)?;
                continue;
            }
            if !response.status().is_success() {
                return Err(status_error(response.status()));
            }
            let bytes = read_limited_secret(response, MAX_LOGIN_RESPONSE_BYTES).await?;
            let mut decoded: LoginResponse =
                serde_json::from_slice(&bytes).map_err(|_| NativeHistoryError::InvalidResponse)?;
            let token = std::mem::take(&mut decoded.token);
            return NativeHistorySession::new(token);
        }
        Err(NativeHistoryError::OriginRejected)
    }

    /// A bounded capability probe. Status errors deliberately do not affect standard API use.
    pub async fn probe(
        &self,
        session: &mut NativeHistorySession,
    ) -> Result<(), NativeHistoryError> {
        let request = NativeScrobblePageRequest::new(0, 1)?;
        self.fetch_page(session, request).await.map(|_| ())
    }

    pub async fn fetch_page(
        &self,
        session: &mut NativeHistorySession,
        request: NativeScrobblePageRequest,
    ) -> Result<NativeScrobblePage, NativeHistoryError> {
        let mut parameters = vec![
            ("_sort", "submission_time".to_owned()),
            ("_order", "DESC".to_owned()),
            ("_start", request.start.to_string()),
            (
                "_end",
                request.start.saturating_add(request.limit).to_string(),
            ),
        ];
        if let Some(from) = request.from_unix {
            parameters.push(("from", from.to_string()));
        }
        if let Some(through) = request.through_unix {
            parameters.push(("to", through.to_string()));
        }
        let response = self
            .authorized_get(session, "api/scrobble", &parameters)
            .await?;
        if !response.status().is_success() {
            return Err(status_error(response.status()));
        }
        let bytes = read_limited(response, MAX_PAGE_RESPONSE_BYTES).await?;
        let raw: Vec<RawScrobbleRow> =
            serde_json::from_slice(&bytes).map_err(|_| NativeHistoryError::InvalidResponse)?;
        if raw.len() > request.limit {
            return Err(NativeHistoryError::InvalidResponse);
        }
        let mut rows = Vec::with_capacity(raw.len());
        for row in raw {
            if row.submission_time > u64::MAX / 1_000 {
                return Err(NativeHistoryError::InvalidResponse);
            }
            rows.push(NativeScrobbleRow {
                id: row.id,
                media_file_id: row.media_file_id,
                submission_time_unix: row.submission_time,
            });
        }
        if rows
            .windows(2)
            .any(|pair| pair[0].submission_time_unix < pair[1].submission_time_unix)
        {
            return Err(NativeHistoryError::InvalidResponse);
        }
        let next_start = (rows.len() == request.limit
            && request.start.saturating_add(request.limit) < MAX_NATIVE_HISTORY_ROWS)
            .then_some(request.start.saturating_add(request.limit));
        Ok(NativeScrobblePage { rows, next_start })
    }

    /// Fetches at most 20,000 newest rows and stops as soon as the known row is observed.
    pub async fn scan_recent(
        &self,
        session: &mut NativeHistorySession,
        high_water_id: Option<u64>,
    ) -> Result<NativeScrobbleScan, NativeHistoryError> {
        self.scan_recent_from(session, high_water_id, None).await
    }

    /// Continue a timestamp-stable scan while keeping each invocation below the public
    /// 100-page/20,000-row limit. The committed high-water remains external to this cursor.
    pub async fn scan_recent_from(
        &self,
        session: &mut NativeHistorySession,
        high_water_id: Option<u64>,
        continuation: Option<NativeScrobbleScanContinuation>,
    ) -> Result<NativeScrobbleScan, NativeHistoryError> {
        self.scan_recent_from_window(session, high_water_id, None, continuation)
            .await
    }

    /// Scan a bounded overlap window. While an older continuation is active, half of every
    /// refresh remains available to a durable current-head sweep and half to the old backlog.
    /// The two lanes share the public 100-page/20,000-row budget.
    pub(crate) async fn scan_recent_from_window(
        &self,
        session: &mut NativeHistorySession,
        high_water_id: Option<u64>,
        overlap_from_unix: Option<u64>,
        continuation: Option<NativeScrobbleScanContinuation>,
    ) -> Result<NativeScrobbleScan, NativeHistoryError> {
        if continuation
            .as_ref()
            .is_some_and(|continuation| !valid_continuation(continuation, high_water_id))
        {
            return Err(NativeHistoryError::InvalidRequest);
        }
        let resuming = continuation.is_some();
        let mut start = continuation.as_ref().map_or(0, |continuation| {
            continuation
                .next_start
                .saturating_sub(NATIVE_HISTORY_PAGE_SIZE)
        });
        let mut through_unix = continuation
            .as_ref()
            .and_then(|continuation| continuation.through_unix);
        let mut pages = 0;
        let mut rows = Vec::new();
        let mut seen = BTreeSet::new();
        if let Some(continuation) = &continuation {
            seen.extend(continuation.overlap_row_ids.iter().copied());
            seen.extend(
                continuation
                    .head_overlap_row_ids
                    .iter()
                    .copied()
                    .filter(|id| {
                        continuation
                            .head_anchor_high_water_id
                            .is_none_or(|anchor| *id > anchor)
                    }),
            );
        }
        let mut next_high_water_id = continuation
            .as_ref()
            .and_then(|continuation| continuation.candidate_high_water_id)
            .or(high_water_id);
        let mut reached_high_water = continuation
            .as_ref()
            .is_some_and(|continuation| continuation.reached_high_water);
        let mut overlap_row_ids = continuation
            .as_ref()
            .map(|continuation| continuation.overlap_row_ids.clone())
            .unwrap_or_default();
        let mut backlog_complete = continuation
            .as_ref()
            .is_some_and(|continuation| continuation.backlog_complete);

        let mut head_complete = !resuming;
        let mut head_anchor_high_water_id = None;
        let mut head_start = 0;
        let mut head_from_unix = None;
        let mut head_through_unix = None;
        let mut head_overlap_row_ids = Vec::new();
        if let Some(progress) = continuation.as_ref() {
            head_complete = false;
            head_anchor_high_water_id = progress
                .head_next_start
                .is_some()
                .then_some(progress.head_anchor_high_water_id)
                .flatten()
                .or(progress.candidate_high_water_id)
                .or(high_water_id);
            head_start = progress.head_next_start.map_or(0, |next_start| {
                next_start.saturating_sub(NATIVE_HISTORY_PAGE_SIZE)
            });
            head_from_unix = progress
                .head_next_start
                .is_some()
                .then_some(progress.head_from_unix)
                .flatten()
                .or(overlap_from_unix);
            head_through_unix = progress.head_next_start.and(progress.head_through_unix);
            if progress.head_next_start.is_some() {
                head_overlap_row_ids = progress.head_overlap_row_ids.clone();
            }
        }

        let mut head_pages = 0;
        while !head_complete
            && head_pages < NATIVE_HISTORY_HEAD_PAGE_BUDGET
            && pages < MAX_NATIVE_HISTORY_PAGES
            && rows.len() < MAX_NATIVE_HISTORY_ROWS
        {
            let request = NativeScrobblePageRequest::new(head_start, NATIVE_HISTORY_PAGE_SIZE)?
                .with_time_window(head_from_unix, head_through_unix)?;
            let page = self.fetch_page(session, request).await?;
            pages += 1;
            head_pages += 1;
            let page_was_full = page.rows.len() == NATIVE_HISTORY_PAGE_SIZE;
            let oldest_timestamp = page.rows.last().map(|row| row.submission_time_unix);
            head_overlap_row_ids = page.rows.iter().map(|row| row.id).collect();
            for row in page.rows {
                collect_scan_row(
                    row,
                    head_anchor_high_water_id,
                    high_water_id,
                    &mut next_high_water_id,
                    &mut reached_high_water,
                    &mut seen,
                    &mut rows,
                );
            }
            if !page_was_full {
                head_complete = true;
                break;
            }
            advance_scan_lane(&mut head_start, &mut head_through_unix, oldest_timestamp)?;
        }

        while !backlog_complete
            && pages < MAX_NATIVE_HISTORY_PAGES
            && rows.len() < MAX_NATIVE_HISTORY_ROWS
        {
            let from_unix = if resuming { None } else { overlap_from_unix };
            let request = NativeScrobblePageRequest::new(start, NATIVE_HISTORY_PAGE_SIZE)?
                .with_time_window(from_unix, through_unix)?;
            let page = self.fetch_page(session, request).await?;
            pages += 1;
            let page_was_full = page.rows.len() == NATIVE_HISTORY_PAGE_SIZE;
            let oldest_timestamp = page.rows.last().map(|row| row.submission_time_unix);
            overlap_row_ids = page.rows.iter().map(|row| row.id).collect();
            for row in page.rows {
                // Row IDs are immutable insertion identities, but submission time is supplied by
                // the client. A newly inserted offline scrobble can therefore sort well below the
                // prior high-water row. Scan the bounded timestamp window to completion and use
                // the ID only as the newness predicate; stopping at the known row would lose that
                // legitimate backfill.
                collect_scan_row(
                    row,
                    high_water_id,
                    high_water_id,
                    &mut next_high_water_id,
                    &mut reached_high_water,
                    &mut seen,
                    &mut rows,
                );
            }
            if !page_was_full {
                backlog_complete = true;
                break;
            }
            advance_scan_lane(&mut start, &mut through_unix, oldest_timestamp)?;
        }
        rows.sort_by(|left, right| {
            right
                .submission_time_unix
                .cmp(&left.submission_time_unix)
                .then_with(|| right.id.cmp(&left.id))
        });
        let complete = backlog_complete && head_complete;
        let continuation = (!complete).then_some(NativeScrobbleScanContinuation {
            candidate_high_water_id: next_high_water_id,
            next_start: start,
            through_unix,
            reached_high_water,
            overlap_row_ids,
            backlog_complete,
            head_anchor_high_water_id: (!head_complete)
                .then_some(head_anchor_high_water_id)
                .flatten(),
            head_next_start: (!head_complete).then_some(head_start),
            head_from_unix: (!head_complete).then_some(head_from_unix).flatten(),
            head_through_unix: (!head_complete).then_some(head_through_unix).flatten(),
            head_overlap_row_ids: if head_complete {
                Vec::new()
            } else {
                head_overlap_row_ids
            },
        });
        Ok(NativeScrobbleScan {
            rows,
            next_high_water_id,
            reached_high_water,
            truncated: !complete,
            continuation,
        })
    }

    async fn authorized_get(
        &self,
        session: &mut NativeHistorySession,
        path: &str,
        parameters: &[(&str, String)],
    ) -> Result<Response, NativeHistoryError> {
        let mut target = self
            .transport
            .origin()
            .native_endpoint(path)
            .map_err(map_origin_error)?;
        for redirects in 0..=MAX_REDIRECTS {
            let authorization = session.authorization()?;
            let response = self
                .transport
                .client()
                .get(target.clone())
                .timeout(REQUEST_TIMEOUT)
                .header(ACCEPT, "application/json")
                .header(AUTHORIZATION_HEADER, authorization.as_str())
                .query(parameters)
                .send()
                .await
                .map_err(|error| classify_request_error(&error))?;
            refresh_session(session, &response)?;
            if !response.status().is_redirection() {
                return Ok(response);
            }
            if redirects == MAX_REDIRECTS {
                return Err(NativeHistoryError::OriginRejected);
            }
            target = redirect_target(&self.transport, &response)?;
        }
        Err(NativeHistoryError::OriginRejected)
    }
}

#[derive(Serialize)]
struct LoginRequest<'a> {
    username: &'a str,
    password: &'a str,
}

#[derive(Deserialize)]
struct LoginResponse {
    token: String,
}

impl Drop for LoginResponse {
    fn drop(&mut self) {
        self.token.zeroize();
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawScrobbleRow {
    id: u64,
    media_file_id: ItemId,
    submission_time: u64,
}

fn refresh_session(
    session: &mut NativeHistorySession,
    response: &Response,
) -> Result<(), NativeHistoryError> {
    let Some(value) = response.headers().get(AUTHORIZATION_HEADER) else {
        return Ok(());
    };
    let token = value
        .to_str()
        .map_err(|_| NativeHistoryError::InvalidResponse)?;
    session.replace_token(token)
}

fn redirect_target(
    transport: &PinnedOriginClient,
    response: &Response,
) -> Result<reqwest::Url, NativeHistoryError> {
    let location = response
        .headers()
        .get(LOCATION)
        .and_then(|value| value.to_str().ok())
        .filter(|value| value.len() <= MAX_LOCATION_BYTES)
        .ok_or(NativeHistoryError::OriginRejected)?;
    transport
        .origin()
        .validate_redirect(response.url(), location)
        .map_err(map_origin_error)
}

fn validate_secret_part(value: &str, max_bytes: usize) -> Result<(), NativeHistoryError> {
    if value.is_empty()
        || value.len() > max_bytes
        || value.chars().any(|character| {
            character.is_control()
                || matches!(
                    character,
                    '\u{200b}'
                        | '\u{200c}'
                        | '\u{200d}'
                        | '\u{200e}'
                        | '\u{200f}'
                        | '\u{202a}'..='\u{202e}'
                        | '\u{2066}'..='\u{2069}'
                        | '\u{feff}'
                )
        })
    {
        return Err(NativeHistoryError::InvalidCredential);
    }
    Ok(())
}

fn validate_token(token: &str) -> Result<(), NativeHistoryError> {
    if token.is_empty()
        || token.len() > MAX_TOKEN_BYTES
        || !token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'='))
    {
        return Err(NativeHistoryError::InvalidResponse);
    }
    Ok(())
}

fn status_error(status: StatusCode) -> NativeHistoryError {
    match status {
        StatusCode::UNAUTHORIZED => NativeHistoryError::AuthenticationRequired,
        StatusCode::FORBIDDEN => NativeHistoryError::PermissionDenied,
        StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED | StatusCode::NOT_IMPLEMENTED => {
            NativeHistoryError::UnsupportedFeature
        }
        status if status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS => {
            NativeHistoryError::TemporarilyUnavailable
        }
        _ => NativeHistoryError::InvalidResponse,
    }
}

fn map_origin_error(error: OriginError) -> NativeHistoryError {
    match error {
        OriginError::InvalidCertificate => NativeHistoryError::CertificateFailed,
        OriginError::ResolutionFailed | OriginError::ClientBuildFailed => {
            NativeHistoryError::Offline
        }
        OriginError::InvalidOrigin
        | OriginError::InsecureOrigin
        | OriginError::DestinationRejected
        | OriginError::RedirectRejected => NativeHistoryError::OriginRejected,
    }
}

fn classify_request_error(error: &reqwest::Error) -> NativeHistoryError {
    let mut source = error.source();
    while let Some(current) = source {
        if current.is::<native_tls::Error>() {
            return NativeHistoryError::CertificateFailed;
        }
        source = current.source();
    }
    NativeHistoryError::Offline
}

async fn read_limited(
    mut response: Response,
    max_bytes: usize,
) -> Result<Vec<u8>, NativeHistoryError> {
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes as u64)
    {
        return Err(NativeHistoryError::ResponseTooLarge);
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| classify_request_error(&error))?
    {
        if bytes.len().saturating_add(chunk.len()) > max_bytes {
            return Err(NativeHistoryError::ResponseTooLarge);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

async fn read_limited_secret(
    mut response: Response,
    max_bytes: usize,
) -> Result<Zeroizing<Vec<u8>>, NativeHistoryError> {
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes as u64)
    {
        return Err(NativeHistoryError::ResponseTooLarge);
    }
    let mut bytes = Zeroizing::new(Vec::new());
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| classify_request_error(&error))?
    {
        if bytes.len().saturating_add(chunk.len()) > max_bytes {
            return Err(NativeHistoryError::ResponseTooLarge);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    use super::*;
    use crate::open_subsonic::ConfiguredPrivateOrigin;

    struct HttpRequest {
        head: String,
        body: Vec<u8>,
    }

    async fn read_request(stream: &mut tokio::net::TcpStream) -> HttpRequest {
        let mut head = Vec::new();
        let mut byte = [0_u8; 1];
        while head.len() < 32 * 1024 {
            assert_ne!(stream.read(&mut byte).await.unwrap(), 0);
            head.push(byte[0]);
            if head.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        let head = String::from_utf8(head).unwrap();
        let content_length = head
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .and_then(|value| value.trim().parse::<usize>().ok())
            })
            .unwrap_or(0);
        let mut body = vec![0; content_length];
        stream.read_exact(&mut body).await.unwrap();
        HttpRequest { head, body }
    }

    async fn write_json(
        stream: &mut tokio::net::TcpStream,
        status: &str,
        headers: &str,
        body: &[u8],
    ) {
        let head = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{headers}Connection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(head.as_bytes()).await.unwrap();
        stream.write_all(body).await.unwrap();
    }

    async fn test_client(port: u16, base: &str) -> NavidromeNativeClient {
        let profile = OpenSubsonicProfile::new(
            "Test server",
            ConfiguredPrivateOrigin::new(&format!("http://127.0.0.1:{port}/{base}"), true).unwrap(),
            None,
        )
        .unwrap();
        NavidromeNativeClient::connect(&profile).await.unwrap()
    }

    fn test_session() -> NativeHistorySession {
        NativeHistorySession::new("test.jwt-token".to_owned()).unwrap()
    }

    #[tokio::test]
    async fn login_body_and_history_authorization_are_kept_out_of_urls() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut login, _) = listener.accept().await.unwrap();
            let request = read_request(&mut login).await;
            assert!(request.head.starts_with("POST /base/auth/login HTTP/1.1"));
            assert!(!request.head.contains("sentinel-password"));
            assert!(!request.head.contains("sentinel-user"));
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            assert_eq!(body["username"], "sentinel-user");
            assert_eq!(body["password"], "sentinel-password");
            write_json(
                &mut login,
                "200 OK",
                "",
                br#"{"token":"first.jwt-token","ignored":"safe"}"#,
            )
            .await;

            let (mut history, _) = listener.accept().await.unwrap();
            let request = read_request(&mut history).await;
            assert!(request.head.starts_with("GET /base/api/scrobble?"));
            assert!(request.head.contains("_sort=submission_time"));
            assert!(request.head.contains("_order=DESC"));
            assert!(request.head.contains("_start=0"));
            assert!(request.head.contains("_end=2"));
            assert!(
                request
                    .head
                    .to_ascii_lowercase()
                    .contains("x-nd-authorization: bearer first.jwt-token")
            );
            assert!(!request.head.contains("sentinel-password"));
            write_json(
                &mut history,
                "200 OK",
                "X-ND-Authorization: refreshed.jwt-token\r\n",
                br#"[{"id":9,"mediaFileId":"song-9","submissionTime":1720000000},{"id":8,"mediaFileId":"song-8","submissionTime":1719999999}]"#,
            )
            .await;
        });

        let client = test_client(port, "base/").await;
        let credential = NativeHistoryCredential::new(
            "sentinel-user",
            SecretString::from("sentinel-password".to_owned()),
        )
        .unwrap();
        let mut session = client.login(&credential).await.unwrap();
        let page = client
            .fetch_page(&mut session, NativeScrobblePageRequest::new(0, 2).unwrap())
            .await
            .unwrap();
        assert_eq!(page.rows.len(), 2);
        assert_eq!(page.rows[0].media_file_id.as_str(), "song-9");
        assert_eq!(
            page.rows[0].submission_time_unix_millis(),
            1_720_000_000_000
        );
        assert_eq!(session.token.as_str(), "refreshed.jwt-token");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn status_schema_and_size_failures_are_classified() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            for status in [
                "404 Not Found",
                "405 Method Not Allowed",
                "401 Unauthorized",
                "403 Forbidden",
                "500 Internal Server Error",
            ] {
                let (mut stream, _) = listener.accept().await.unwrap();
                let _ = read_request(&mut stream).await;
                write_json(&mut stream, status, "", b"").await;
            }
            let (mut invalid, _) = listener.accept().await.unwrap();
            let _ = read_request(&mut invalid).await;
            write_json(&mut invalid, "200 OK", "", br#"{"not":"a list"}"#).await;

            let (mut oversized, _) = listener.accept().await.unwrap();
            let _ = read_request(&mut oversized).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                MAX_PAGE_RESPONSE_BYTES + 1
            );
            oversized.write_all(response.as_bytes()).await.unwrap();
        });
        let client = test_client(port, "").await;
        let mut session = test_session();
        let request = NativeScrobblePageRequest::new(0, 1).unwrap();
        for expected in [
            NativeHistoryError::UnsupportedFeature,
            NativeHistoryError::UnsupportedFeature,
            NativeHistoryError::AuthenticationRequired,
            NativeHistoryError::PermissionDenied,
            NativeHistoryError::TemporarilyUnavailable,
            NativeHistoryError::InvalidResponse,
            NativeHistoryError::ResponseTooLarge,
        ] {
            assert_eq!(
                client.fetch_page(&mut session, request).await,
                Err(expected)
            );
        }
        server.await.unwrap();
    }

    #[tokio::test]
    async fn redirects_are_same_origin_and_bounded() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            for _ in 0..=MAX_REDIRECTS {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = read_request(&mut stream).await;
                assert!(
                    request
                        .head
                        .to_ascii_lowercase()
                        .contains("x-nd-authorization: bearer test.jwt-token")
                );
                write_json(
                    &mut stream,
                    "307 Temporary Redirect",
                    "Location: /base/api/scrobble-loop\r\n",
                    b"",
                )
                .await;
            }
        });
        let client = test_client(port, "base/").await;
        let mut session = test_session();
        assert_eq!(
            client.probe(&mut session).await,
            Err(NativeHistoryError::OriginRejected)
        );
        server.await.unwrap();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = read_request(&mut stream).await;
            write_json(
                &mut stream,
                "307 Temporary Redirect",
                "Location: http://127.0.0.1:9/steal\r\n",
                b"",
            )
            .await;
        });
        let client = test_client(port, "").await;
        let mut session = test_session();
        assert_eq!(
            client.probe(&mut session).await,
            Err(NativeHistoryError::OriginRejected)
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn timestamp_sorted_scan_accepts_backfilled_id_inversion_across_pages() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let first_rows = (101_u64..=300)
                .rev()
                .map(|id| {
                    serde_json::json!({
                        "id": id,
                        "mediaFileId": format!("song-{id}"),
                        "submissionTime": 1_720_000_000_u64 + id,
                    })
                })
                .collect::<Vec<_>>();
            let second_rows = (95_u64..=101)
                .rev()
                .map(|id| {
                    // An offline submission is inserted later (ID 500) with the older
                    // client-supplied timestamp that row 100 would have occupied.
                    let inserted_id = if id == 100 { 500 } else { id };
                    serde_json::json!({
                        "id": inserted_id,
                        "mediaFileId": format!("song-{inserted_id}"),
                        "submissionTime": 1_720_000_000_u64 + id,
                    })
                })
                .collect::<Vec<_>>();
            for (index, rows) in [first_rows, second_rows].into_iter().enumerate() {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = read_request(&mut stream).await;
                assert!(request.head.contains("_start=0"));
                if index == 1 {
                    assert!(request.head.contains("to=1720000101"));
                }
                let body = serde_json::to_vec(&rows).unwrap();
                write_json(&mut stream, "200 OK", "", &body).await;
            }
        });
        let client = test_client(port, "").await;
        let mut session = test_session();
        let scan = client.scan_recent(&mut session, Some(98)).await.unwrap();
        assert!(scan.reached_high_water);
        assert!(!scan.truncated);
        assert_eq!(scan.rows.len(), 202);
        assert_eq!(scan.rows.first().unwrap().id, 300);
        assert_eq!(scan.rows.last().unwrap().id, 99);
        assert!(scan.rows.iter().any(|row| row.id == 500));
        assert_eq!(scan.next_high_water_id, Some(500));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn same_second_rows_advance_inside_the_timestamp_overlap() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let page = |first: u64, last: u64| {
            (last..=first)
                .rev()
                .map(|id| {
                    serde_json::json!({
                        "id": id,
                        "mediaFileId": format!("song-{id}"),
                        "submissionTime": 1_720_000_000_u64,
                    })
                })
                .collect::<Vec<_>>()
        };
        let first = page(400, 201);
        let duplicate = first.clone();
        let third = page(200, 190);
        let server = tokio::spawn(async move {
            for (expected_start, rows) in [(0, first), (0, duplicate), (200, third)] {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = read_request(&mut stream).await;
                assert!(request.head.contains(&format!("_start={expected_start}")));
                let body = serde_json::to_vec(&rows).unwrap();
                write_json(&mut stream, "200 OK", "", &body).await;
            }
        });
        let client = test_client(port, "").await;
        let mut session = test_session();
        let scan = client.scan_recent(&mut session, Some(198)).await.unwrap();
        assert!(scan.reached_high_water);
        assert_eq!(scan.rows.len(), 202);
        assert_eq!(scan.rows.first().unwrap().id, 400);
        assert_eq!(scan.rows.last().unwrap().id, 199);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn resumed_same_second_scan_overlaps_page_to_absorb_head_insert_race() {
        const SUBMITTED_AT: u64 = 1_720_000_000;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let all_rows = (1_u64..=650).rev().collect::<Vec<_>>();
            // A durable head sweep completes before the old backlog resumes. Both lanes overlap
            // their preceding page at the inclusive same-second boundary.
            for (index, expected_start) in [0, 0, 200, 400, 600, 0, 200, 400, 600]
                .into_iter()
                .enumerate()
            {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = read_request(&mut stream).await;
                assert_eq!(query_number(&request.head, "_start"), Some(expected_start));
                assert_eq!(
                    query_number(&request.head, "to"),
                    (index != 0).then_some(SUBMITTED_AT)
                );
                let rows = all_rows
                    .iter()
                    .skip(expected_start as usize)
                    .take(NATIVE_HISTORY_PAGE_SIZE)
                    .map(|id| {
                        serde_json::json!({
                            "id": id,
                            "mediaFileId": format!("song-{id}"),
                            "submissionTime": SUBMITTED_AT,
                        })
                    })
                    .collect::<Vec<_>>();
                let body = serde_json::to_vec(&rows).unwrap();
                write_json(&mut stream, "200 OK", "", &body).await;
            }
        });
        let client = test_client(port, "").await;
        let mut session = test_session();
        let continuation = NativeScrobbleScanContinuation {
            candidate_high_water_id: Some(400),
            next_start: 200,
            through_unix: Some(SUBMITTED_AT),
            reached_high_water: false,
            overlap_row_ids: (201..=400).rev().collect(),
            backlog_complete: false,
            head_anchor_high_water_id: None,
            head_next_start: None,
            head_from_unix: None,
            head_through_unix: None,
            head_overlap_row_ids: Vec::new(),
        };
        let scan = client
            .scan_recent_from(&mut session, None, Some(continuation))
            .await
            .unwrap();
        assert!(!scan.truncated);
        assert_eq!(scan.next_high_water_id, Some(650));
        let ids = scan.rows.iter().map(|row| row.id).collect::<BTreeSet<_>>();
        assert_eq!(ids.len(), 450);
        assert!((1..=200).all(|id| ids.contains(&id)));
        assert!((401..=650).all(|id| ids.contains(&id)));
        assert!((201..=400).all(|id| !ids.contains(&id)));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn durable_head_and_backlog_both_progress_across_a_twenty_thousand_row_burst() {
        const BASE_TIME: u64 = 1_720_000_000;
        const PRIOR_CANDIDATE: u64 = 20_250;
        const TOTAL_ROWS: u64 = 40_300;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let server_requests = requests.clone();
        let (shutdown, mut stop) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            loop {
                let accepted = tokio::select! {
                    _ = &mut stop => break,
                    accepted = listener.accept() => accepted,
                };
                let (mut stream, _) = accepted.unwrap();
                server_requests.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let request = read_request(&mut stream).await;
                let start = query_number(&request.head, "_start").unwrap_or(0) as usize;
                let newest = query_number(&request.head, "to")
                    .map(|through| through.saturating_sub(BASE_TIME).min(TOTAL_ROWS))
                    .unwrap_or(TOTAL_ROWS);
                let rows = (1..=newest)
                    .rev()
                    .skip(start)
                    .take(NATIVE_HISTORY_PAGE_SIZE)
                    .map(|id| {
                        serde_json::json!({
                            "id": id,
                            "mediaFileId": format!("song-{id}"),
                            "submissionTime": BASE_TIME + id,
                        })
                    })
                    .collect::<Vec<_>>();
                let body = serde_json::to_vec(&rows).unwrap();
                write_json(&mut stream, "200 OK", "", &body).await;
            }
        });

        let client = test_client(port, "").await;
        let mut session = test_session();
        let persisted = NativeScrobbleScanContinuation {
            candidate_high_water_id: Some(PRIOR_CANDIDATE),
            next_start: 0,
            through_unix: Some(BASE_TIME + 500),
            reached_high_water: false,
            overlap_row_ids: Vec::new(),
            backlog_complete: false,
            head_anchor_high_water_id: None,
            head_next_start: None,
            head_from_unix: None,
            head_through_unix: None,
            head_overlap_row_ids: Vec::new(),
        };
        let before = requests.load(std::sync::atomic::Ordering::Relaxed);
        let mut scan = client
            .scan_recent_from(&mut session, Some(1), Some(persisted))
            .await
            .unwrap();
        let used = requests
            .load(std::sync::atomic::Ordering::Relaxed)
            .saturating_sub(before);
        assert!(used <= MAX_NATIVE_HISTORY_PAGES);
        assert!(scan.truncated);
        let progress = scan.continuation.as_ref().unwrap();
        assert!(
            progress.backlog_complete,
            "the old backlog must progress while the new head exceeds one refresh"
        );
        assert!(progress.head_next_start.is_some());
        assert!(
            scan.rows.iter().any(|row| row.id == 2)
                && scan.rows.iter().any(|row| row.id > PRIOR_CANDIDATE),
            "one refresh must advance both the old tail and the newly inserted head"
        );

        // IDs 501..=20,250 model rows returned before the serialized crash point. Every later
        // invocation receives only a cloned cursor; no in-memory seen set survives the restart.
        let mut ids = (501..=PRIOR_CANDIDATE).collect::<BTreeSet<_>>();
        for row in &scan.rows {
            assert!(ids.insert(row.id), "a resumed scan replayed row {}", row.id);
        }
        for _ in 0..8 {
            let Some(continuation) = scan.continuation.clone() else {
                break;
            };
            let before = requests.load(std::sync::atomic::Ordering::Relaxed);
            scan = client
                .scan_recent_from(&mut session, Some(1), Some(continuation))
                .await
                .unwrap();
            let used = requests
                .load(std::sync::atomic::Ordering::Relaxed)
                .saturating_sub(before);
            assert!(used <= MAX_NATIVE_HISTORY_PAGES);
            for row in &scan.rows {
                assert!(ids.insert(row.id), "a resumed scan replayed row {}", row.id);
            }
        }
        assert!(!scan.truncated);
        assert!(scan.reached_high_water);
        assert_eq!(scan.next_high_water_id, Some(TOTAL_ROWS));
        assert_eq!(ids.len(), (TOTAL_ROWS - 1) as usize);
        assert_eq!(ids.first(), Some(&2));
        assert_eq!(ids.last(), Some(&TOTAL_ROWS));
        shutdown.send(()).unwrap();
        server.await.unwrap();
    }

    fn query_number(head: &str, key: &str) -> Option<u64> {
        let target = head.lines().next()?.split_whitespace().nth(1)?;
        let query = target.split_once('?')?.1;
        query.split('&').find_map(|pair| {
            let (name, value) = pair.split_once('=')?;
            (name == key).then(|| value.parse().ok()).flatten()
        })
    }

    #[test]
    fn credential_page_and_token_validation_fail_closed() {
        assert!(
            NativeHistoryCredential::new("", SecretString::from("password".to_owned())).is_err()
        );
        assert!(NativeHistoryCredential::new("user", SecretString::from(String::new())).is_err());
        assert_eq!(
            NativeScrobblePageRequest::new(0, 0),
            Err(NativeHistoryError::InvalidRequest)
        );
        assert_eq!(
            NativeScrobblePageRequest::new(0, NATIVE_HISTORY_PAGE_SIZE + 1),
            Err(NativeHistoryError::InvalidRequest)
        );
        assert!(NativeHistorySession::new("unsafe token".to_owned()).is_err());
        let rendered = format!(
            "{:?} {}",
            NativeHistoryError::AuthenticationRequired,
            NativeHistoryError::AuthenticationRequired
        );
        assert!(!rendered.contains("sentinel-password"));
        assert!(!rendered.contains("http://"));
    }
}
