//! Bounded, redacted OpenSubsonic HTTP transport.

use std::error::Error as _;
use std::time::Duration;

use reqwest::header::{CONTENT_TYPE, LOCATION, RANGE};
use reqwest::{Method, Response, StatusCode};

use super::auth::{AuthParameters, common_parameters};
use super::model::{AccountScopeId, BackendId, CoverArtId, OpenSubsonicItemRef};
use super::origin::{OriginError, PinnedOriginClient};
use super::private_store::ServerCredential;
use super::profile::OpenSubsonicProfile;
use super::proxy::{ProxyOrigin, StreamMethod, StreamRequest};
use super::wire::{RawResponse, WireError};

#[path = "endpoint.rs"]
mod endpoint;
pub(crate) use endpoint::Endpoint;

const MAX_REDIRECTS: usize = 3;
const MAX_JSON_BYTES: usize = 8 * 1024 * 1024;
const MAX_COVER_ART_BYTES: usize = 32 * 1024 * 1024;
const MAX_LOCATION_BYTES: usize = 4_096;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerError {
    Offline,
    AuthenticationRequired,
    PermissionDenied,
    CertificateFailed,
    OriginRejected,
    RateLimited(Option<Duration>),
    UnsupportedFeature,
    NotFound,
    InvalidResponse,
    ResponseTooLarge,
    TemporarilyUnavailable,
    WrongAccountScope,
}

/// Delivery evidence for a non-idempotent server mutation.
///
/// Only failures which prove that the server did not apply the mutation may be retried
/// automatically. Every other transport failure is ambiguous, even when its redacted public
/// error is merely `Offline`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MutationDeliveryError {
    DefinitelyNotApplied(ServerError),
    Ambiguous(ServerError),
}

impl MutationDeliveryError {
    pub(crate) const fn server_error(self) -> ServerError {
        match self {
            Self::DefinitelyNotApplied(error) | Self::Ambiguous(error) => error,
        }
    }
}

impl std::fmt::Display for ServerError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::Offline => "music server is offline",
            Self::AuthenticationRequired => "music server password needs updating",
            Self::PermissionDenied => "music server denied this action",
            Self::CertificateFailed => "music server certificate could not be verified",
            Self::OriginRejected => "music server address was rejected",
            Self::RateLimited(_) => "music server asked us to retry later",
            Self::UnsupportedFeature => "music server does not support this feature",
            Self::NotFound => "music server item was not found",
            Self::InvalidResponse => "music server returned an invalid response",
            Self::ResponseTooLarge => "music server response exceeded the safety limit",
            Self::TemporarilyUnavailable => "music server is temporarily unavailable",
            Self::WrongAccountScope => "music server item belongs to another profile",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for ServerError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerInfo {
    pub api_version: Option<String>,
    pub server_type: Option<String>,
    pub server_version: Option<String>,
    pub advertises_open_subsonic: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinaryPayload {
    pub bytes: Vec<u8>,
    pub content_type: Option<String>,
}

/// DNS-pinned transport. Credentials remain in the caller/actor.
pub struct OpenSubsonicClient {
    transport: PinnedOriginClient,
    backend_id: BackendId,
    account_scope_id: AccountScopeId,
}

impl OpenSubsonicClient {
    pub async fn connect(profile: &OpenSubsonicProfile) -> Result<Self, ServerError> {
        let transport = profile
            .origin()
            .build_pinned_client(profile.custom_ca_pem())
            .await
            .map_err(map_origin_error)?;
        Ok(Self {
            transport,
            backend_id: profile.backend_id().clone(),
            account_scope_id: profile.account_scope_id().clone(),
        })
    }

    pub async fn ping(&self, credential: &ServerCredential) -> Result<ServerInfo, ServerError> {
        let response = self.request_json(credential, Endpoint::Ping, &[]).await?;
        Ok(ServerInfo {
            api_version: sanitize_optional(response.version, 64),
            server_type: sanitize_optional(response.server_type, 100),
            server_version: sanitize_optional(response.server_version, 100),
            advertises_open_subsonic: response.open_subsonic.unwrap_or(false),
        })
    }

    pub async fn get_cover_art(
        &self,
        credential: &ServerCredential,
        id: &CoverArtId,
    ) -> Result<BinaryPayload, ServerError> {
        let parameters = [("id", id.as_str().to_owned())];
        let response = self
            .request_response(
                Some(credential),
                Endpoint::CoverArt,
                &parameters,
                Method::GET,
                None,
            )
            .await?;
        if !response.status().is_success() {
            return Err(status_error_for(Endpoint::CoverArt, &response));
        }
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(sanitize_content_type);
        let bytes = read_limited(response, MAX_COVER_ART_BYTES).await?;
        if content_type
            .as_deref()
            .is_some_and(|content_type| content_type.starts_with("application/json"))
        {
            return match super::wire::decode(&bytes) {
                Err(error) => Err(map_wire_error(error)),
                Ok(_) => Err(ServerError::InvalidResponse),
            };
        }
        Ok(BinaryPayload {
            bytes,
            content_type,
        })
    }

    pub async fn open_stream(
        &self,
        credential: &ServerCredential,
        item: &OpenSubsonicItemRef,
        request: StreamRequest,
    ) -> Result<Response, ServerError> {
        self.validate_item_scope(item)?;
        if request.range.is_some_and(|range| {
            matches!((range.start, range.end), (None, None))
                || matches!((range.start, range.end), (Some(a), Some(b)) if a > b)
        }) {
            return Err(ServerError::InvalidResponse);
        }
        let method = match request.method {
            StreamMethod::Head => Method::HEAD,
            StreamMethod::Get => Method::GET,
        };
        let range = request.range.map(|range| range.to_header_value());
        let parameters = [("id", item.item_id().as_str().to_owned())];
        let response = self
            .request_response(
                Some(credential),
                Endpoint::Stream,
                &parameters,
                method,
                range,
            )
            .await?;
        if response.status() == StatusCode::OK
            || response.status() == StatusCode::PARTIAL_CONTENT
            || response.status() == StatusCode::RANGE_NOT_SATISFIABLE
        {
            Ok(response)
        } else {
            Err(status_error_for(Endpoint::Stream, &response))
        }
    }

    pub(crate) async fn request_json(
        &self,
        credential: &ServerCredential,
        endpoint: Endpoint,
        parameters: &[(&str, String)],
    ) -> Result<RawResponse, ServerError> {
        let response = self
            .request_response(Some(credential), endpoint, parameters, Method::GET, None)
            .await?;
        if !response.status().is_success() {
            return Err(status_error_for(endpoint, &response));
        }
        let bytes = read_limited(response, MAX_JSON_BYTES).await?;
        super::wire::decode(&bytes).map_err(map_wire_error)
    }

    pub(crate) async fn request_public_json(
        &self,
        endpoint: Endpoint,
        parameters: &[(&str, String)],
    ) -> Result<RawResponse, ServerError> {
        let response = self
            .request_response(None, endpoint, parameters, Method::GET, None)
            .await?;
        if !response.status().is_success() {
            return Err(status_error_for(endpoint, &response));
        }
        let bytes = read_limited(response, MAX_JSON_BYTES).await?;
        super::wire::decode(&bytes).map_err(map_wire_error)
    }

    pub(crate) fn proxy_origin(&self) -> Result<ProxyOrigin, ServerError> {
        let url = self
            .transport
            .origin()
            .endpoint("stream")
            .map_err(map_origin_error)?;
        ProxyOrigin::from_url(&url).map_err(|_| ServerError::OriginRejected)
    }

    async fn request_response(
        &self,
        credential: Option<&ServerCredential>,
        endpoint: Endpoint,
        parameters: &[(&str, String)],
        method: Method,
        range: Option<String>,
    ) -> Result<Response, ServerError> {
        self.request_response_with_delivery(credential, endpoint, parameters, method, range)
            .await
            .map_err(MutationDeliveryError::server_error)
    }

    pub(super) async fn request_response_with_delivery(
        &self,
        credential: Option<&ServerCredential>,
        endpoint: Endpoint,
        parameters: &[(&str, String)],
        method: Method,
        range: Option<String>,
    ) -> Result<Response, MutationDeliveryError> {
        let mut target = self
            .transport
            .origin()
            .endpoint(endpoint.method_name())
            .map_err(map_origin_error)
            .map_err(MutationDeliveryError::DefinitelyNotApplied)?;
        for redirects in 0..=MAX_REDIRECTS {
            let mut request = self
                .transport
                .client()
                .request(method.clone(), target.clone());
            if request_has_total_timeout(endpoint, &method) {
                request = request.timeout(REQUEST_TIMEOUT);
            }
            request = request.query(&common_parameters());
            let auth = credential
                .map(AuthParameters::fresh)
                .transpose()
                .map_err(|_| MutationDeliveryError::DefinitelyNotApplied(ServerError::Offline))?;
            if let Some(auth) = &auth {
                request = request.query(auth.fields());
            }
            request = request.query(parameters);
            if let Some(range) = &range {
                request = request.header(RANGE, range);
            }
            let response = request
                .send()
                .await
                .map_err(classify_mutation_request_error)?;
            if !response.status().is_redirection() {
                return Ok(response);
            }
            if matches!(endpoint, Endpoint::Scrobble | Endpoint::StartScan) {
                // A non-conforming server could commit this GET-style mutation before returning
                // its redirect. Following even a same-origin Location could submit it twice: a
                // duplicated scrobble, or a second full library scan on a server that just
                // started one.
                return Err(MutationDeliveryError::Ambiguous(
                    ServerError::OriginRejected,
                ));
            }
            if redirects == MAX_REDIRECTS {
                return Err(MutationDeliveryError::DefinitelyNotApplied(
                    ServerError::OriginRejected,
                ));
            }
            let location = response
                .headers()
                .get(LOCATION)
                .and_then(|value| value.to_str().ok())
                .filter(|value| value.len() <= MAX_LOCATION_BYTES)
                .ok_or(MutationDeliveryError::DefinitelyNotApplied(
                    ServerError::OriginRejected,
                ))?;
            target = self
                .transport
                .origin()
                .validate_redirect(response.url(), location)
                .map_err(map_origin_error)
                .map_err(MutationDeliveryError::DefinitelyNotApplied)?;
        }
        Err(MutationDeliveryError::DefinitelyNotApplied(
            ServerError::OriginRejected,
        ))
    }

    pub(crate) fn validate_item_scope(
        &self,
        item: &OpenSubsonicItemRef,
    ) -> Result<(), ServerError> {
        if item.backend_id() != &self.backend_id
            || item.account_scope_id() != &self.account_scope_id
        {
            return Err(ServerError::WrongAccountScope);
        }
        Ok(())
    }
}

fn request_has_total_timeout(endpoint: Endpoint, method: &Method) -> bool {
    !matches!(endpoint, Endpoint::Stream) || *method != Method::GET
}

fn map_origin_error(error: OriginError) -> ServerError {
    match error {
        OriginError::InvalidCertificate => ServerError::CertificateFailed,
        OriginError::ResolutionFailed => ServerError::Offline,
        OriginError::ClientBuildFailed => ServerError::Offline,
        OriginError::InvalidOrigin
        | OriginError::InsecureOrigin
        | OriginError::DestinationRejected
        | OriginError::RedirectRejected => ServerError::OriginRejected,
    }
}

fn classify_request_error(error: &reqwest::Error) -> ServerError {
    let mut source = error.source();
    while let Some(current) = source {
        if current.is::<native_tls::Error>() {
            return ServerError::CertificateFailed;
        }
        source = current.source();
    }
    ServerError::Offline
}

fn classify_mutation_request_error(error: reqwest::Error) -> MutationDeliveryError {
    let classified = classify_request_error(&error);
    if error.is_connect() || error.is_builder() || classified == ServerError::CertificateFailed {
        MutationDeliveryError::DefinitelyNotApplied(classified)
    } else {
        // A timeout, request-body failure, or response-header loss can happen after the server
        // committed a GET-style OpenSubsonic mutation.
        MutationDeliveryError::Ambiguous(classified)
    }
}

fn status_error_for(endpoint: Endpoint, response: &Response) -> ServerError {
    match response.status() {
        StatusCode::UNAUTHORIZED => ServerError::AuthenticationRequired,
        StatusCode::FORBIDDEN => ServerError::PermissionDenied,
        StatusCode::NOT_FOUND
            if matches!(
                endpoint,
                Endpoint::Extensions
                    | Endpoint::Search3
                    | Endpoint::AlbumList2
                    | Endpoint::Artists
                    | Endpoint::Playlists
                    // `startScan` predates OpenSubsonic and is absent from older Subsonic servers
                    // and from proxies that expose only part of the API. Treating that as a
                    // missing feature is what lets a publish degrade to an advisory instead of
                    // reporting a failure for work that already succeeded.
                    | Endpoint::StartScan
            ) =>
        {
            ServerError::UnsupportedFeature
        }
        StatusCode::NOT_FOUND => ServerError::NotFound,
        StatusCode::METHOD_NOT_ALLOWED | StatusCode::NOT_IMPLEMENTED => {
            ServerError::UnsupportedFeature
        }
        StatusCode::TOO_MANY_REQUESTS => ServerError::RateLimited(retry_after(response.headers())),
        StatusCode::SERVICE_UNAVAILABLE => retry_after(response.headers())
            .map_or(ServerError::TemporarilyUnavailable, |delay| {
                ServerError::RateLimited(Some(delay))
            }),
        status if status.is_server_error() => ServerError::TemporarilyUnavailable,
        _ => ServerError::InvalidResponse,
    }
}

fn map_wire_error(error: WireError) -> ServerError {
    match error {
        WireError::InvalidResponse => ServerError::InvalidResponse,
        WireError::ApiFailure(Some(40..=44)) => ServerError::AuthenticationRequired,
        WireError::ApiFailure(Some(50)) => ServerError::PermissionDenied,
        WireError::ApiFailure(Some(70)) => ServerError::NotFound,
        WireError::ApiFailure(_) => ServerError::InvalidResponse,
    }
}

async fn read_limited(mut response: Response, max_bytes: usize) -> Result<Vec<u8>, ServerError> {
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes as u64)
    {
        return Err(ServerError::ResponseTooLarge);
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| classify_request_error(&error))?
    {
        if bytes.len().saturating_add(chunk.len()) > max_bytes {
            return Err(ServerError::ResponseTooLarge);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let raw = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim();
    if !raw.is_empty() && raw.bytes().all(|byte| byte.is_ascii_digit()) {
        let seconds = raw.bytes().fold(0_u64, |value, byte| {
            value
                .saturating_mul(10)
                .saturating_add(u64::from(byte - b'0'))
        });
        return Some(Duration::from_secs(seconds));
    }
    let target = parse_imf_fixdate(raw)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    Some(Duration::from_secs(target.saturating_sub(now)))
}

fn parse_imf_fixdate(value: &str) -> Option<u64> {
    let bytes = value.as_bytes();
    if bytes.len() != 29
        || !matches!(
            &bytes[0..3],
            b"Mon" | b"Tue" | b"Wed" | b"Thu" | b"Fri" | b"Sat" | b"Sun"
        )
        || &bytes[3..5] != b", "
        || bytes[7] != b' '
        || bytes[11] != b' '
        || bytes[16] != b' '
        || bytes[19] != b':'
        || bytes[22] != b':'
        || &bytes[25..29] != b" GMT"
    {
        return None;
    }
    let day = decimal(&bytes[5..7])?;
    let month = match &bytes[8..11] {
        b"Jan" => 1,
        b"Feb" => 2,
        b"Mar" => 3,
        b"Apr" => 4,
        b"May" => 5,
        b"Jun" => 6,
        b"Jul" => 7,
        b"Aug" => 8,
        b"Sep" => 9,
        b"Oct" => 10,
        b"Nov" => 11,
        b"Dec" => 12,
        _ => return None,
    };
    let year = decimal(&bytes[12..16])?;
    let hour = decimal(&bytes[17..19])?;
    let minute = decimal(&bytes[20..22])?;
    let second = decimal(&bytes[23..25])?;
    if year < 1970
        || day == 0
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    let years = year.checked_sub(1970)?;
    let leap_days = leap_years_before(year).checked_sub(leap_years_before(1970))?;
    let mut days = years.checked_mul(365)?.checked_add(leap_days)?;
    for prior_month in 1..month {
        days = days.checked_add(days_in_month(year, prior_month))?;
    }
    days = days.checked_add(day.checked_sub(1)?)?;
    days.checked_mul(86_400)?
        .checked_add(hour.checked_mul(3_600)?)?
        .checked_add(minute.checked_mul(60)?)?
        .checked_add(second)
}

fn decimal(bytes: &[u8]) -> Option<u64> {
    bytes.iter().try_fold(0_u64, |value, byte| {
        value
            .checked_mul(10)?
            .checked_add(u64::from(byte.checked_sub(b'0')?))
            .filter(|_| byte.is_ascii_digit())
    })
}

fn days_in_month(year: u64, month: u64) -> u64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

fn is_leap_year(year: u64) -> bool {
    year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400))
}

fn leap_years_before(year: u64) -> u64 {
    let prior = year.saturating_sub(1);
    prior / 4 - prior / 100 + prior / 400
}

fn sanitize_optional(value: Option<String>, max_chars: usize) -> Option<String> {
    value
        .map(|value| crate::api::sanitize_metadata_text(&value, max_chars))
        .filter(|value| !value.is_empty())
}

fn sanitize_content_type(raw: &str) -> Option<String> {
    if raw.is_empty()
        || raw.len() > 256
        || !raw.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'/' | b'+' | b'-' | b'.' | b';' | b'=' | b' ')
        })
    {
        return None;
    }
    Some(raw.to_owned())
}

#[cfg(test)]
mod tests {
    use age::secrecy::SecretString;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    use super::*;
    use crate::open_subsonic::{ConfiguredPrivateOrigin, ServerCredential};

    async fn test_client(port: u16) -> (OpenSubsonicClient, ServerCredential) {
        let profile = OpenSubsonicProfile::new(
            "Test server",
            ConfiguredPrivateOrigin::new(&format!("http://127.0.0.1:{port}/"), true).unwrap(),
            None,
        )
        .unwrap();
        let client = OpenSubsonicClient::connect(&profile).await.unwrap();
        let credential =
            ServerCredential::api_key(SecretString::from("sentinel-api-key".to_owned())).unwrap();
        (client, credential)
    }

    async fn read_request(stream: &mut tokio::net::TcpStream) -> Vec<u8> {
        let mut request = Vec::new();
        let mut byte = [0_u8; 1];
        while request.len() < 32 * 1024 {
            if stream.read(&mut byte).await.unwrap() == 0 {
                break;
            }
            request.push(byte[0]);
            if request.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        request
    }

    #[tokio::test]
    async fn fake_server_observes_exclusive_api_key_auth() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = String::from_utf8(read_request(&mut stream).await).unwrap();
            assert!(request.contains("apiKey=sentinel-api-key"));
            assert!(!request.contains("&u="));
            assert!(!request.contains("&t="));
            assert!(!request.contains("&s="));
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{\"subsonic-response\":{\"status\":\"ok\",\"version\":\"1.16.1\",\"openSubsonic\":true}}",
                )
                .await
                .unwrap();
        });
        let (client, credential) = test_client(port).await;
        let info = client.ping(&credential).await.unwrap();
        assert!(info.advertises_open_subsonic);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn cross_origin_redirect_is_rejected_without_contacting_target() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = read_request(&mut stream).await;
            stream
                .write_all(
                    b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:9/steal\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
        });
        let (client, credential) = test_client(port).await;
        assert_eq!(
            client.ping(&credential).await,
            Err(ServerError::OriginRejected)
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn public_extension_probe_sends_no_credential() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = String::from_utf8(read_request(&mut stream).await).unwrap();
            assert!(request.contains("getOpenSubsonicExtensions.view"));
            assert!(!request.contains("apiKey="));
            assert!(!request.contains("&u="));
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{\"subsonic-response\":{\"status\":\"ok\",\"openSubsonicExtensions\":[]}}",
                )
                .await
                .unwrap();
        });
        let (client, _) = test_client(port).await;
        client
            .request_public_json(Endpoint::Extensions, &[])
            .await
            .unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn failed_success_envelope_and_oversized_body_are_bounded() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut first, _) = listener.accept().await.unwrap();
            let _ = read_request(&mut first).await;
            first
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{\"subsonic-response\":{\"status\":\"failed\",\"error\":{\"code\":40}}}",
                )
                .await
                .unwrap();
            drop(first);
            let (mut second, _) = listener.accept().await.unwrap();
            let _ = read_request(&mut second).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                MAX_JSON_BYTES + 1
            );
            second.write_all(response.as_bytes()).await.unwrap();
        });
        let (client, credential) = test_client(port).await;
        assert_eq!(
            client.ping(&credential).await,
            Err(ServerError::AuthenticationRequired)
        );
        assert_eq!(
            client.ping(&credential).await,
            Err(ServerError::ResponseTooLarge)
        );
        server.await.unwrap();
    }

    #[test]
    fn retry_after_accepts_delta_and_bounded_http_date() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "75".parse().unwrap());
        assert_eq!(retry_after(&headers), Some(Duration::from_secs(75)));
        headers.insert(
            reqwest::header::RETRY_AFTER,
            "Sun, 06 Nov 1994 08:49:37 GMT".parse().unwrap(),
        );
        assert_eq!(retry_after(&headers), Some(Duration::ZERO));
        headers.insert(reqwest::header::RETRY_AFTER, "not a date".parse().unwrap());
        assert_eq!(retry_after(&headers), None);
    }

    #[test]
    fn all_errors_are_redacted_fixed_messages() {
        let rendered = format!(
            "{:?} {}",
            ServerError::AuthenticationRequired,
            ServerError::AuthenticationRequired
        );
        assert!(!rendered.contains("sentinel-api-key"));
        assert!(!rendered.contains("http://"));
    }

    #[test]
    fn only_stream_get_omits_the_whole_response_deadline() {
        assert!(!request_has_total_timeout(Endpoint::Stream, &Method::GET));
        assert!(request_has_total_timeout(Endpoint::Stream, &Method::HEAD));
        assert!(request_has_total_timeout(Endpoint::Ping, &Method::GET));
        assert!(request_has_total_timeout(Endpoint::CoverArt, &Method::GET));
    }
}
