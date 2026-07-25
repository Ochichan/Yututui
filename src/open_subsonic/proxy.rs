//! Credential-owning, one-episode loopback playback proxy.
//!
//! mpv receives only an unguessable `127.0.0.1` route. The upstream implementation owns
//! credentials and redirect policy; this boundary independently checks the final origin and
//! forwards only a small allowlist of response headers.

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use futures::StreamExt as _;
use reqwest::header::{ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, watch};
use tokio::task::JoinHandle;

use super::{AccountScopeId, BackendId, ItemId, OpenSubsonicItemRef};
use crate::playback_target::{
    CredentialedPlaybackRef, PlaybackRouteError, PlaybackRouteFuture, PlaybackRouteLease,
    PlaybackRouteProvider, PlaybackRouteProviderHandle, PlaybackRouteRevocation, RoutedPlayback,
};

const MAX_REQUEST_HEADER_BYTES: usize = 16 * 1024;
const MAX_REQUEST_HEADERS: usize = 64;
const MAX_SAFE_HEADER_BYTES: usize = 256;
const MAX_ACTIVE_ROUTES: usize = 64;
const MAX_CONNECTIONS: usize = 16;
const REQUEST_HEADER_TIMEOUT: Duration = Duration::from_secs(5);
const UPSTREAM_OPEN_TIMEOUT: Duration = Duration::from_secs(15);
const BODY_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const UNUSED_ROUTE_TTL: Duration = Duration::from_secs(60);
const TOKEN_BYTES: usize = 32;
const TOKEN_HEX_BYTES: usize = TOKEN_BYTES * 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StreamMethod {
    Head,
    Get,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ByteRange {
    pub start: Option<u64>,
    pub end: Option<u64>,
}

impl ByteRange {
    pub fn to_header_value(self) -> String {
        match (self.start, self.end) {
            (Some(start), Some(end)) => format!("bytes={start}-{end}"),
            (Some(start), None) => format!("bytes={start}-"),
            (None, Some(suffix)) => format!("bytes=-{suffix}"),
            (None, None) => unreachable!("validated range has one bound"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StreamRequest {
    pub method: StreamMethod,
    pub range: Option<ByteRange>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProxyUpstreamError {
    reason: &'static str,
}

impl ProxyUpstreamError {
    pub const fn new(reason: &'static str) -> Self {
        Self { reason }
    }

    pub const fn reason(self) -> &'static str {
        self.reason
    }
}

impl std::fmt::Display for ProxyUpstreamError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.reason)
    }
}

impl std::error::Error for ProxyUpstreamError {}

#[derive(Clone, Eq, PartialEq)]
pub struct ProxyOrigin {
    scheme: String,
    host: String,
    port: u16,
}

impl std::fmt::Debug for ProxyOrigin {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ProxyOrigin(<redacted>)")
    }
}

impl ProxyOrigin {
    pub fn from_url(url: &reqwest::Url) -> Result<Self, ProxyUpstreamError> {
        let scheme = url.scheme();
        if !matches!(scheme, "http" | "https")
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(ProxyUpstreamError::new("invalid_upstream_origin"));
        }
        let host = url
            .host_str()
            .ok_or_else(|| ProxyUpstreamError::new("invalid_upstream_origin"))?;
        let port = url
            .port_or_known_default()
            .ok_or_else(|| ProxyUpstreamError::new("invalid_upstream_origin"))?;
        Ok(Self {
            scheme: scheme.to_owned(),
            host: host.trim_end_matches('.').to_ascii_lowercase(),
            port,
        })
    }

    fn matches(&self, url: &reqwest::Url) -> bool {
        url.scheme() == self.scheme
            && url
                .host_str()
                .map(|host| host.trim_end_matches('.').to_ascii_lowercase())
                .as_deref()
                == Some(self.host.as_str())
            && url.port_or_known_default() == Some(self.port)
            && url.username().is_empty()
            && url.password().is_none()
    }
}

/// Unbuffered upstream response paired with the exact origin snapshot used for that request.
pub struct UpstreamStream {
    response: reqwest::Response,
    origin: ProxyOrigin,
}

impl UpstreamStream {
    pub fn new(response: reqwest::Response, origin: ProxyOrigin) -> Self {
        Self { response, origin }
    }
}

pub type StreamSourceFuture =
    Pin<Box<dyn Future<Output = Result<UpstreamStream, ProxyUpstreamError>> + Send + 'static>>;

/// Auth-owning OpenSubsonic client/actor hook.
pub trait StreamSource: Send + Sync {
    fn open_stream(&self, item: OpenSubsonicItemRef, request: StreamRequest) -> StreamSourceFuture;
}

struct RouteState {
    item: OpenSubsonicItemRef,
    revoked: AtomicBool,
    revoked_tx: watch::Sender<bool>,
    admission: Mutex<RouteAdmission>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RouteAdmission {
    WaitingUntil(tokio::time::Instant),
    Admitted,
}

impl RouteState {
    fn new(item: OpenSubsonicItemRef, unused_ttl: Duration) -> Self {
        let (revoked_tx, _) = watch::channel(false);
        Self {
            item,
            revoked: AtomicBool::new(false),
            revoked_tx,
            admission: Mutex::new(RouteAdmission::WaitingUntil(
                tokio::time::Instant::now() + unused_ttl,
            )),
        }
    }

    fn is_live(&self) -> bool {
        if self.revoked.load(Ordering::Acquire) {
            return false;
        }
        let expired = matches!(
            *self
                .admission
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            RouteAdmission::WaitingUntil(deadline) if tokio::time::Instant::now() >= deadline
        );
        if expired {
            self.revoke();
            false
        } else {
            true
        }
    }

    /// Admit the first valid HTTP request before the short bearer-token deadline.
    ///
    /// Once admitted, the route remains reusable for HEAD/GET and Range reconnects while its
    /// playback lease is alive. This keeps the pre-admission token exposure short without
    /// imposing an absolute deadline on a long track.
    fn try_admit(&self) -> bool {
        if self.revoked.load(Ordering::Acquire) {
            return false;
        }
        let mut admission = self
            .admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.revoked.load(Ordering::Acquire) {
            return false;
        }
        match *admission {
            RouteAdmission::Admitted => true,
            RouteAdmission::WaitingUntil(deadline) if tokio::time::Instant::now() < deadline => {
                *admission = RouteAdmission::Admitted;
                true
            }
            RouteAdmission::WaitingUntil(_) => {
                drop(admission);
                self.revoke();
                false
            }
        }
    }

    fn subscribe(&self) -> watch::Receiver<bool> {
        self.revoked_tx.subscribe()
    }
}

impl PlaybackRouteRevocation for RouteState {
    fn revoke(&self) {
        if !self.revoked.swap(true, Ordering::AcqRel) {
            self.revoked_tx.send_replace(true);
        }
    }
}

struct ProxyInner {
    port: u16,
    body_idle_timeout: Duration,
    unused_route_ttl: Duration,
    source: Arc<dyn StreamSource>,
    routes: Mutex<HashMap<String, Arc<RouteState>>>,
    closed: AtomicBool,
}

impl ProxyInner {
    fn mint_route(
        &self,
        target: CredentialedPlaybackRef,
    ) -> Result<RoutedPlayback, PlaybackRouteError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(PlaybackRouteError::new("route_provider_unavailable"));
        }
        let item = credentialed_item(target)?;
        let mut routes = self
            .routes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        routes.retain(|_, route| {
            let keep = route.is_live();
            if !keep {
                route.revoke();
            }
            keep
        });
        if routes.len() >= MAX_ACTIVE_ROUTES {
            return Err(PlaybackRouteError::new("route_capacity_reached"));
        }
        for _ in 0..4 {
            let token = random_token()?;
            if routes.contains_key(&token) {
                continue;
            }
            let route = Arc::new(RouteState::new(item.clone(), self.unused_route_ttl));
            routes.insert(token.clone(), Arc::clone(&route));
            let revocation: Arc<dyn PlaybackRouteRevocation> = route;
            return RoutedPlayback::new(self.port, token, PlaybackRouteLease::new(revocation));
        }
        Err(PlaybackRouteError::new("route_token_collision"))
    }

    fn find_route(&self, token: &str) -> Option<Arc<RouteState>> {
        let mut routes = self
            .routes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let route = routes.get(token).cloned()?;
        if route.try_admit() {
            Some(route)
        } else {
            route.revoke();
            routes.remove(token);
            None
        }
    }

    fn revoke_all(&self) {
        self.closed.store(true, Ordering::Release);
        let mut routes = self
            .routes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for route in routes.values() {
            route.revoke();
        }
        routes.clear();
    }
}

#[derive(Clone)]
pub struct OpenSubsonicProxyHandle {
    inner: Arc<ProxyInner>,
}

impl OpenSubsonicProxyHandle {
    pub fn route_provider(&self) -> PlaybackRouteProviderHandle {
        PlaybackRouteProviderHandle::new(Arc::new(self.clone()))
    }

    pub fn revoke_all(&self) {
        self.inner.revoke_all();
    }
}

impl std::fmt::Debug for OpenSubsonicProxyHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("OpenSubsonicProxyHandle(<redacted>)")
    }
}

impl PlaybackRouteProvider for OpenSubsonicProxyHandle {
    fn open_route(
        &self,
        target: CredentialedPlaybackRef,
        _file_generation: u64,
    ) -> PlaybackRouteFuture {
        let result = self.inner.mint_route(target);
        Box::pin(async move { result })
    }
}

pub struct OpenSubsonicProxyGuard {
    inner: Arc<ProxyInner>,
    shutdown_tx: watch::Sender<bool>,
    task: Option<JoinHandle<()>>,
}

impl OpenSubsonicProxyGuard {
    pub async fn shutdown(mut self) {
        self.inner.revoke_all();
        self.shutdown_tx.send_replace(true);
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for OpenSubsonicProxyGuard {
    fn drop(&mut self) {
        self.inner.revoke_all();
        self.shutdown_tx.send_replace(true);
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

pub async fn start(
    source: Arc<dyn StreamSource>,
) -> io::Result<(OpenSubsonicProxyHandle, OpenSubsonicProxyGuard)> {
    start_with_limits(source, BODY_IDLE_TIMEOUT, UNUSED_ROUTE_TTL).await
}

async fn start_with_limits(
    source: Arc<dyn StreamSource>,
    body_idle_timeout: Duration,
    unused_route_ttl: Duration,
) -> io::Result<(OpenSubsonicProxyHandle, OpenSubsonicProxyGuard)> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();
    let inner = Arc::new(ProxyInner {
        port,
        body_idle_timeout,
        unused_route_ttl,
        source,
        routes: Mutex::new(HashMap::new()),
        closed: AtomicBool::new(false),
    });
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let task = tokio::spawn(run(listener, Arc::clone(&inner), shutdown_rx));
    Ok((
        OpenSubsonicProxyHandle {
            inner: Arc::clone(&inner),
        },
        OpenSubsonicProxyGuard {
            inner,
            shutdown_tx,
            task: Some(task),
        },
    ))
}

async fn run(
    listener: TcpListener,
    inner: Arc<ProxyInner>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let permits = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    loop {
        tokio::select! {
            changed = shutdown_rx.changed() => {
                if changed.is_err() || *shutdown_rx.borrow() {
                    return;
                }
            }
            accepted = listener.accept() => {
                let Ok((stream, peer)) = accepted else {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                };
                if !peer.ip().is_loopback() {
                    continue;
                }
                let Ok(permit) = Arc::clone(&permits).try_acquire_owned() else {
                    tokio::spawn(write_empty_response(stream, 503, "Service Unavailable"));
                    continue;
                };
                let inner = Arc::clone(&inner);
                tokio::spawn(async move {
                    let _permit = permit;
                    let _ = serve_connection(stream, inner).await;
                });
            }
        }
    }
}

struct ParsedRequest {
    method: StreamMethod,
    token: String,
    range: Option<ByteRange>,
}

async fn serve_connection(mut stream: TcpStream, inner: Arc<ProxyInner>) -> io::Result<()> {
    let request = match tokio::time::timeout(
        REQUEST_HEADER_TIMEOUT,
        read_request(&mut stream, inner.port),
    )
    .await
    {
        Ok(Ok(request)) => request,
        Ok(Err(status)) => {
            return write_empty_response(stream, status.0, status.1).await;
        }
        Err(_) => {
            return write_empty_response(stream, 408, "Request Timeout").await;
        }
    };
    let Some(route) = inner.find_route(&request.token) else {
        return write_empty_response(stream, 404, "Not Found").await;
    };
    let mut revoked_rx = route.subscribe();
    if *revoked_rx.borrow_and_update() || !route.is_live() {
        return write_empty_response(stream, 404, "Not Found").await;
    }
    let upstream = inner.source.open_stream(
        route.item.clone(),
        StreamRequest {
            method: request.method,
            range: request.range,
        },
    );
    tokio::pin!(upstream);
    let open_timeout = tokio::time::sleep(UPSTREAM_OPEN_TIMEOUT);
    tokio::pin!(open_timeout);
    let upstream = tokio::select! {
        changed = revoked_rx.changed() => {
            let _ = changed;
            return Ok(());
        }
        _ = &mut open_timeout => {
            return write_empty_response(stream, 504, "Gateway Timeout").await;
        }
        upstream = &mut upstream => upstream,
    };
    let upstream = match upstream {
        Ok(upstream) => upstream,
        Err(_) => return write_empty_response(stream, 502, "Bad Gateway").await,
    };
    if !upstream.origin.matches(upstream.response.url()) {
        return write_empty_response(stream, 502, "Bad Gateway").await;
    }
    forward_response(
        stream,
        upstream.response,
        request.method,
        &mut revoked_rx,
        inner.body_idle_timeout,
    )
    .await
}

async fn read_request(
    stream: &mut TcpStream,
    expected_port: u16,
) -> Result<ParsedRequest, (u16, &'static str)> {
    let mut bytes = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        if bytes.len() >= MAX_REQUEST_HEADER_BYTES {
            return Err((431, "Request Header Fields Too Large"));
        }
        let remaining = MAX_REQUEST_HEADER_BYTES - bytes.len();
        let read_capacity = remaining.min(chunk.len());
        let count = stream
            .read(&mut chunk[..read_capacity])
            .await
            .map_err(|_| (400, "Bad Request"))?;
        if count == 0 {
            return Err((400, "Bad Request"));
        }
        bytes.extend_from_slice(&chunk[..count]);
        if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    if !bytes.is_ascii() {
        return Err((400, "Bad Request"));
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| (400, "Bad Request"))?;
    let mut lines = text.split("\r\n");
    let mut request_line = lines
        .next()
        .ok_or((400, "Bad Request"))?
        .split_ascii_whitespace();
    let method = match request_line.next() {
        Some("GET") => StreamMethod::Get,
        Some("HEAD") => StreamMethod::Head,
        Some(_) => return Err((405, "Method Not Allowed")),
        None => return Err((400, "Bad Request")),
    };
    let path = request_line.next().ok_or((400, "Bad Request"))?;
    if request_line.next() != Some("HTTP/1.1") || request_line.next().is_some() {
        return Err((400, "Bad Request"));
    }
    let token = path
        .strip_prefix("/stream/")
        .filter(|token| {
            token.len() == TOKEN_HEX_BYTES
                && token.bytes().all(|byte| byte.is_ascii_hexdigit())
                && !path.contains('?')
                && !path.contains('#')
        })
        .ok_or((404, "Not Found"))?
        .to_owned();
    let mut host_seen = false;
    let mut range = None;
    let mut header_count = 0usize;
    for line in lines {
        if line.is_empty() {
            break;
        }
        header_count += 1;
        if header_count > MAX_REQUEST_HEADERS || line.len() > MAX_SAFE_HEADER_BYTES {
            return Err((431, "Request Header Fields Too Large"));
        }
        let (name, value) = line.split_once(':').ok_or((400, "Bad Request"))?;
        let value = value.trim();
        if name.eq_ignore_ascii_case("host") {
            if host_seen || !host_is_loopback(value, expected_port) {
                return Err((400, "Bad Request"));
            }
            host_seen = true;
        } else if name.eq_ignore_ascii_case("range") {
            if range.is_some() {
                return Err((400, "Bad Request"));
            }
            range = Some(parse_range(value)?);
        }
    }
    if !host_seen {
        return Err((400, "Bad Request"));
    }
    Ok(ParsedRequest {
        method,
        token,
        range,
    })
}

fn host_is_loopback(value: &str, expected_port: u16) -> bool {
    value
        .split_once(':')
        .is_some_and(|(host, port)| host == "127.0.0.1" && port.parse::<u16>() == Ok(expected_port))
}

fn parse_range(value: &str) -> Result<ByteRange, (u16, &'static str)> {
    let range = value
        .strip_prefix("bytes=")
        .filter(|value| !value.contains(','))
        .ok_or((416, "Range Not Satisfiable"))?;
    let (start, end) = range
        .split_once('-')
        .ok_or((416, "Range Not Satisfiable"))?;
    let start = (!start.is_empty())
        .then(|| start.parse::<u64>())
        .transpose()
        .map_err(|_| (416, "Range Not Satisfiable"))?;
    let end = (!end.is_empty())
        .then(|| end.parse::<u64>())
        .transpose()
        .map_err(|_| (416, "Range Not Satisfiable"))?;
    if start.is_none() && end.is_none()
        || matches!((start, end), (Some(start), Some(end)) if start > end)
        || matches!((start, end), (None, Some(0)))
    {
        return Err((416, "Range Not Satisfiable"));
    }
    Ok(ByteRange { start, end })
}

async fn forward_response(
    mut downstream: TcpStream,
    upstream: reqwest::Response,
    method: StreamMethod,
    revoked_rx: &mut watch::Receiver<bool>,
    body_idle_timeout: Duration,
) -> io::Result<()> {
    let status = upstream.status();
    let (code, reason) = match status.as_u16() {
        200 => (200, "OK"),
        206 => (206, "Partial Content"),
        416 => (416, "Range Not Satisfiable"),
        _ => return write_empty_response(downstream, 502, "Bad Gateway").await,
    };
    if code != 416 && structured_error_content_type(upstream.headers()) {
        return write_empty_response(downstream, 502, "Bad Gateway").await;
    }
    let headers = safe_response_headers(upstream.headers());
    let mut response = format!("HTTP/1.1 {code} {reason}\r\nConnection: close\r\n");
    for (name, value) in headers {
        response.push_str(name);
        response.push_str(": ");
        response.push_str(&value);
        response.push_str("\r\n");
    }
    response.push_str("\r\n");
    downstream.write_all(response.as_bytes()).await?;
    if method == StreamMethod::Head || code == 416 {
        return downstream.shutdown().await;
    }
    let mut body = upstream.bytes_stream();
    loop {
        let idle = tokio::time::sleep(body_idle_timeout);
        tokio::pin!(idle);
        tokio::select! {
            changed = revoked_rx.changed() => {
                if changed.is_err() || *revoked_rx.borrow_and_update() {
                    return Ok(());
                }
            }
            _ = &mut idle => return Ok(()),
            chunk = body.next() => match chunk {
                Some(Ok(chunk)) => {
                    let write_idle = tokio::time::sleep(body_idle_timeout);
                    tokio::pin!(write_idle);
                    tokio::select! {
                        changed = revoked_rx.changed() => {
                            let _ = changed;
                            return Ok(());
                        }
                        _ = &mut write_idle => return Ok(()),
                        result = downstream.write_all(&chunk) => result?,
                    }
                }
                Some(Err(_)) => return Ok(()),
                None => return downstream.shutdown().await,
            }
        }
    }
}

fn structured_error_content_type(headers: &reqwest::header::HeaderMap) -> bool {
    let Some(content_type) = headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| {
            value
                .split(';')
                .next()
                .unwrap_or(value)
                .trim()
                .to_ascii_lowercase()
        })
    else {
        return false;
    };
    content_type == "application/json"
        || content_type.ends_with("+json")
        || content_type == "application/xml"
        || content_type.ends_with("+xml")
        || matches!(content_type.as_str(), "text/xml" | "text/html")
}

fn safe_response_headers(headers: &reqwest::header::HeaderMap) -> Vec<(&'static str, String)> {
    let mut safe = Vec::with_capacity(4);
    if let Some(value) = safe_header(headers, CONTENT_TYPE) {
        safe.push(("Content-Type", value));
    }
    if let Some(value) =
        safe_header(headers, CONTENT_LENGTH).filter(|value| value.parse::<u64>().is_ok())
    {
        safe.push(("Content-Length", value));
    }
    if let Some(value) =
        safe_header(headers, CONTENT_RANGE).filter(|value| valid_content_range(value))
    {
        safe.push(("Content-Range", value));
    }
    if safe_header(headers, ACCEPT_RANGES).as_deref() == Some("bytes") {
        safe.push(("Accept-Ranges", "bytes".to_owned()));
    }
    safe
}

fn safe_header(
    headers: &reqwest::header::HeaderMap,
    name: reqwest::header::HeaderName,
) -> Option<String> {
    let value = headers.get(name)?.to_str().ok()?;
    (value.len() <= MAX_SAFE_HEADER_BYTES && !value.bytes().any(|byte| byte.is_ascii_control()))
        .then(|| value.to_owned())
}

fn valid_content_range(value: &str) -> bool {
    value
        .strip_prefix("bytes ")
        .and_then(|value| value.split_once('/'))
        .is_some_and(|(range, total)| {
            let total_valid = total == "*" || total.parse::<u64>().is_ok();
            let range_valid = range == "*"
                || range.split_once('-').is_some_and(|(start, end)| {
                    start
                        .parse::<u64>()
                        .ok()
                        .zip(end.parse::<u64>().ok())
                        .is_some_and(|(start, end)| start <= end)
                });
            total_valid && range_valid
        })
}

async fn write_empty_response(
    mut stream: TcpStream,
    status: u16,
    reason: &'static str,
) -> io::Result<()> {
    stream
        .write_all(
            format!("HTTP/1.1 {status} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await?;
    stream.shutdown().await
}

fn credentialed_item(
    target: CredentialedPlaybackRef,
) -> Result<OpenSubsonicItemRef, PlaybackRouteError> {
    match target {
        CredentialedPlaybackRef::OpenSubsonic {
            backend_id,
            account_scope_id,
            item_id,
        } => Ok(OpenSubsonicItemRef::new(
            BackendId::new(backend_id)
                .map_err(|_| PlaybackRouteError::new("invalid_catalog_identity"))?,
            AccountScopeId::new(account_scope_id)
                .map_err(|_| PlaybackRouteError::new("invalid_catalog_identity"))?,
            ItemId::new(item_id)
                .map_err(|_| PlaybackRouteError::new("invalid_catalog_identity"))?,
        )),
    }
}

fn random_token() -> Result<String, PlaybackRouteError> {
    let mut bytes = [0u8; TOKEN_BYTES];
    getrandom::fill(&mut bytes)
        .map_err(|_| PlaybackRouteError::new("secure_random_unavailable"))?;
    let mut token = String::with_capacity(TOKEN_HEX_BYTES);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in bytes {
        token.push(char::from(HEX[usize::from(byte >> 4)]));
        token.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    #[derive(Clone)]
    struct HttpStreamSource {
        client: reqwest::Client,
        url: reqwest::Url,
        origin: ProxyOrigin,
        requests: mpsc::UnboundedSender<StreamRequest>,
    }

    impl StreamSource for HttpStreamSource {
        fn open_stream(
            &self,
            _item: OpenSubsonicItemRef,
            request: StreamRequest,
        ) -> StreamSourceFuture {
            let client = self.client.clone();
            let url = self.url.clone();
            let origin = self.origin.clone();
            let requests = self.requests.clone();
            Box::pin(async move {
                let _ = requests.send(request);
                let method = match request.method {
                    StreamMethod::Head => reqwest::Method::HEAD,
                    StreamMethod::Get => reqwest::Method::GET,
                };
                let mut request_builder = client.request(method, url);
                if let Some(range) = request.range {
                    request_builder =
                        request_builder.header(reqwest::header::RANGE, range.to_header_value());
                }
                let response = request_builder
                    .send()
                    .await
                    .map_err(|_| ProxyUpstreamError::new("test_upstream_failed"))?;
                Ok(UpstreamStream::new(response, origin))
            })
        }
    }

    fn item() -> OpenSubsonicItemRef {
        OpenSubsonicItemRef::new(
            BackendId::new("backend").unwrap(),
            AccountScopeId::new("account").unwrap(),
            ItemId::new("song").unwrap(),
        )
    }

    fn credentialed_item() -> CredentialedPlaybackRef {
        CredentialedPlaybackRef::OpenSubsonic {
            backend_id: "backend".to_owned(),
            account_scope_id: "account".to_owned(),
            item_id: "song".to_owned(),
        }
    }

    async fn source_for(
        url: reqwest::Url,
        origin: ProxyOrigin,
    ) -> (
        Arc<dyn StreamSource>,
        mpsc::UnboundedReceiver<StreamRequest>,
    ) {
        let (requests_tx, requests_rx) = mpsc::unbounded_channel();
        let source = HttpStreamSource {
            client: reqwest::Client::builder()
                .no_proxy()
                .build()
                .expect("test client"),
            url,
            origin,
            requests: requests_tx,
        };
        (Arc::new(source), requests_rx)
    }

    async fn static_upstream(response: &'static [u8]) -> (reqwest::Url, JoinHandle<()>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 4096];
            let _ = stream.read(&mut request).await.unwrap();
            stream.write_all(response).await.unwrap();
            stream.shutdown().await.unwrap();
        });
        (
            reqwest::Url::parse(&format!("http://{address}/stream")).unwrap(),
            task,
        )
    }

    async fn lingering_upstream() -> (reqwest::Url, JoinHandle<()>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let upstream_url =
            reqwest::Url::parse(&format!("http://{}/stream", listener.local_addr().unwrap()))
                .unwrap();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 4096];
            let _ = stream.read(&mut request).await.unwrap();
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: audio/mpeg\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n3\r\nabc\r\n",
                )
                .await
                .unwrap();
            std::future::pending::<()>().await;
        });
        (upstream_url, task)
    }

    async fn controlled_upstream() -> (reqwest::Url, mpsc::UnboundedSender<()>, JoinHandle<()>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let upstream_url =
            reqwest::Url::parse(&format!("http://{}/stream", listener.local_addr().unwrap()))
                .unwrap();
        let (finish_tx, mut finish_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 4096];
            let _ = stream.read(&mut request).await.unwrap();
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: audio/mpeg\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n3\r\nabc\r\n",
                )
                .await
                .unwrap();
            finish_rx.recv().await.expect("finish signal");
            stream.write_all(b"3\r\ndef\r\n0\r\n\r\n").await.unwrap();
            stream.shutdown().await.unwrap();
        });
        (upstream_url, finish_tx, task)
    }

    async fn mint_route(handle: &OpenSubsonicProxyHandle) -> (reqwest::Url, PlaybackRouteLease) {
        let route = handle
            .open_route(credentialed_item(), 1)
            .await
            .expect("route");
        let (url, lease) = route.into_parts();
        (
            reqwest::Url::parse(&url.into_string()).expect("loopback URL"),
            lease,
        )
    }

    async fn raw_proxy_request(url: &reqwest::Url, headers: &str) -> Vec<u8> {
        let mut stream = open_raw_proxy_request(url, headers).await;
        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut response))
            .await
            .expect("proxy response timeout")
            .unwrap();
        response
    }

    async fn open_raw_proxy_request(url: &reqwest::Url, headers: &str) -> TcpStream {
        let mut stream =
            TcpStream::connect((url.host_str().expect("host"), url.port().expect("port")))
                .await
                .unwrap();
        let request = format!(
            "GET {} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n{headers}\r\n",
            url.path(),
            url.port().unwrap()
        );
        stream.write_all(request.as_bytes()).await.unwrap();
        stream
    }

    #[test]
    fn accepts_only_one_well_formed_byte_range() {
        assert_eq!(
            parse_range("bytes=10-20").unwrap(),
            ByteRange {
                start: Some(10),
                end: Some(20)
            }
        );
        assert_eq!(
            parse_range("bytes=10-").unwrap(),
            ByteRange {
                start: Some(10),
                end: None
            }
        );
        assert_eq!(
            parse_range("bytes=-20").unwrap(),
            ByteRange {
                start: None,
                end: Some(20)
            }
        );
        for invalid in [
            "bytes=",
            "bytes=20-10",
            "bytes=0-1,4-5",
            "items=0-1",
            "bytes=-0",
        ] {
            assert!(parse_range(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn structured_api_content_types_are_not_media() {
        let mut headers = reqwest::header::HeaderMap::new();
        for blocked in [
            "application/json",
            "application/problem+json; charset=utf-8",
            "application/xml",
            "text/xml",
            "text/html",
        ] {
            headers.insert(CONTENT_TYPE, blocked.parse().unwrap());
            assert!(structured_error_content_type(&headers), "{blocked}");
        }
        headers.insert(CONTENT_TYPE, "audio/mpeg".parse().unwrap());
        assert!(!structured_error_content_type(&headers));
    }

    #[tokio::test]
    #[cfg_attr(
        windows,
        ignore = "GitHub Windows loopback can abort or stall this raw-socket fixture"
    )]
    async fn forwards_single_range_and_only_safe_response_headers() {
        let (upstream_url, upstream_task) = static_upstream(
            b"HTTP/1.1 206 Partial Content\r\nContent-Type: audio/mpeg\r\nContent-Length: 4\r\nContent-Range: bytes 2-5/10\r\nAccept-Ranges: bytes\r\nX-Upstream-Secret: hidden\r\nConnection: close\r\n\r\ndata",
        )
        .await;
        let origin = ProxyOrigin::from_url(&upstream_url).unwrap();
        let (source, mut requests) = source_for(upstream_url, origin).await;
        let (handle, guard) = start(source).await.unwrap();
        let (route_url, _lease) = mint_route(&handle).await;

        let response = raw_proxy_request(&route_url, "Range: bytes=2-5\r\n").await;
        let response = String::from_utf8(response).unwrap();
        assert!(response.starts_with("HTTP/1.1 206 Partial Content\r\n"));
        assert!(response.contains("Content-Type: audio/mpeg\r\n"));
        assert!(response.contains("Content-Range: bytes 2-5/10\r\n"));
        assert!(response.ends_with("\r\n\r\ndata"));
        assert!(!response.contains("X-Upstream-Secret"));
        assert_eq!(
            requests.recv().await.unwrap().range,
            Some(ByteRange {
                start: Some(2),
                end: Some(5)
            })
        );

        guard.shutdown().await;
        upstream_task.await.unwrap();
    }

    #[tokio::test]
    #[cfg_attr(
        windows,
        ignore = "GitHub Windows loopback can abort or stall this raw-socket fixture"
    )]
    async fn structured_error_body_never_reaches_mpv_route() {
        let (upstream_url, upstream_task) = static_upstream(
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 25\r\nConnection: close\r\n\r\n{\"token\":\"server-secret\"}",
        )
        .await;
        let origin = ProxyOrigin::from_url(&upstream_url).unwrap();
        let (source, _requests) = source_for(upstream_url, origin).await;
        let (handle, guard) = start(source).await.unwrap();
        let (route_url, _lease) = mint_route(&handle).await;

        let response = raw_proxy_request(&route_url, "").await;
        let response = String::from_utf8(response).unwrap();
        assert!(response.starts_with("HTTP/1.1 502 Bad Gateway\r\n"));
        assert!(!response.contains("server-secret"));

        guard.shutdown().await;
        upstream_task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    #[cfg_attr(
        windows,
        ignore = "GitHub Windows loopback can abort or stall this raw-socket fixture"
    )]
    async fn revoked_episode_returns_not_found() {
        let (upstream_url, upstream_task) = static_upstream(
            b"HTTP/1.1 200 OK\r\nContent-Type: audio/mpeg\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )
        .await;
        let origin = ProxyOrigin::from_url(&upstream_url).unwrap();
        let (source, _requests) = source_for(upstream_url, origin).await;
        let (handle, guard) = start(source).await.unwrap();
        let (route_url, lease) = mint_route(&handle).await;
        drop(lease);

        let response = raw_proxy_request(&route_url, "").await;
        assert!(response.starts_with(b"HTTP/1.1 404 Not Found\r\n"));

        guard.shutdown().await;
        upstream_task.abort();
    }

    #[tokio::test(start_paused = true)]
    #[cfg_attr(
        windows,
        ignore = "GitHub Windows loopback can abort or stall this raw-socket fixture"
    )]
    async fn unused_route_expiry_returns_not_found_and_reclaims_capacity() {
        let upstream_url = reqwest::Url::parse("http://127.0.0.1:9/unused").unwrap();
        let origin = ProxyOrigin::from_url(&upstream_url).unwrap();
        let (source, _requests) = source_for(upstream_url, origin).await;
        let unused_ttl = Duration::from_secs(5);
        let (handle, guard) = start_with_limits(source, BODY_IDLE_TIMEOUT, unused_ttl)
            .await
            .unwrap();
        let mut routes = Vec::with_capacity(MAX_ACTIVE_ROUTES);
        for _ in 0..MAX_ACTIVE_ROUTES {
            routes.push(mint_route(&handle).await);
        }
        let capacity_error = handle
            .open_route(credentialed_item(), 1)
            .await
            .expect_err("unexpired routes must consume capacity");
        assert_eq!(capacity_error.reason(), "route_capacity_reached");

        tokio::time::advance(unused_ttl + Duration::from_millis(1)).await;
        let response = raw_proxy_request(&routes[0].0, "").await;
        assert!(response.starts_with(b"HTTP/1.1 404 Not Found\r\n"));

        let replacement = handle
            .open_route(credentialed_item(), 2)
            .await
            .expect("expired routes must release capacity");
        assert_eq!(
            handle
                .inner
                .routes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            1
        );
        drop(replacement);
        drop(routes);
        guard.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    #[cfg_attr(
        windows,
        ignore = "GitHub Windows loopback can abort or stall this raw-socket fixture"
    )]
    async fn admitted_body_stream_outlives_unused_route_deadline() {
        let (upstream_url, finish_tx, upstream_task) = controlled_upstream().await;
        let origin = ProxyOrigin::from_url(&upstream_url).unwrap();
        let (source, _requests) = source_for(upstream_url, origin).await;
        let unused_ttl = Duration::from_secs(5);
        let (handle, guard) = start_with_limits(source, Duration::from_secs(60 * 60), unused_ttl)
            .await
            .unwrap();
        let (route_url, _lease) = mint_route(&handle).await;
        let mut downstream = open_raw_proxy_request(&route_url, "").await;
        let mut response = Vec::new();
        // Give the OS-backed loopback sockets real time to become readable. The admission expiry
        // itself remains deterministic below while Tokio's clock is paused.
        tokio::time::resume();
        tokio::time::timeout(Duration::from_secs(2), async {
            while !response.windows(3).any(|window| window == b"abc") {
                let mut chunk = [0u8; 1024];
                let count = downstream.read(&mut chunk).await.unwrap();
                assert_ne!(count, 0, "upstream closed before its first body chunk");
                response.extend_from_slice(&chunk[..count]);
            }
        })
        .await
        .expect("initial proxy response timeout");

        tokio::time::pause();
        tokio::time::advance(unused_ttl + Duration::from_secs(1)).await;
        tokio::time::resume();
        finish_tx.send(()).unwrap();
        tokio::time::timeout(
            Duration::from_secs(2),
            downstream.read_to_end(&mut response),
        )
        .await
        .expect("admitted route expired during a live body")
        .unwrap();
        assert!(response.windows(6).any(|window| window == b"abcdef"));

        guard.shutdown().await;
        upstream_task.await.unwrap();
    }

    #[tokio::test]
    #[cfg_attr(
        windows,
        ignore = "GitHub Windows loopback can abort or stall this raw-socket fixture"
    )]
    async fn dropping_episode_lease_closes_an_open_body_stream() {
        let (upstream_url, upstream_task) = lingering_upstream().await;
        let origin = ProxyOrigin::from_url(&upstream_url).unwrap();
        let (source, _requests) = source_for(upstream_url, origin).await;
        let (handle, guard) = start(source).await.unwrap();
        let (route_url, lease) = mint_route(&handle).await;
        let mut downstream = open_raw_proxy_request(&route_url, "").await;
        let mut initial = [0u8; 1024];
        let read = tokio::time::timeout(Duration::from_secs(2), downstream.read(&mut initial))
            .await
            .expect("initial proxy response timeout")
            .unwrap();
        assert!(String::from_utf8_lossy(&initial[..read]).starts_with("HTTP/1.1 200 OK\r\n"));

        drop(lease);
        let mut remainder = Vec::new();
        tokio::time::timeout(
            Duration::from_secs(1),
            downstream.read_to_end(&mut remainder),
        )
        .await
        .expect("revoked route did not close its open body")
        .unwrap();

        guard.shutdown().await;
        upstream_task.abort();
    }

    #[tokio::test]
    #[cfg_attr(
        windows,
        ignore = "GitHub Windows loopback can abort or stall this raw-socket fixture"
    )]
    async fn stalled_upstream_body_is_closed_after_idle_timeout() {
        let (upstream_url, upstream_task) = lingering_upstream().await;
        let origin = ProxyOrigin::from_url(&upstream_url).unwrap();
        let (source, _requests) = source_for(upstream_url, origin).await;
        let (handle, guard) =
            start_with_limits(source, Duration::from_millis(20), UNUSED_ROUTE_TTL)
                .await
                .unwrap();
        let (route_url, _lease) = mint_route(&handle).await;
        let mut downstream = open_raw_proxy_request(&route_url, "").await;
        let mut response = Vec::new();
        tokio::time::timeout(
            Duration::from_secs(1),
            downstream.read_to_end(&mut response),
        )
        .await
        .expect("idle upstream body kept the proxy route open")
        .unwrap();
        assert!(response.starts_with(b"HTTP/1.1 200 OK\r\n"));

        guard.shutdown().await;
        upstream_task.abort();
    }

    #[test]
    fn token_and_route_debug_output_are_redacted() {
        let (lease, _) = {
            let revoked = Arc::new(RouteState::new(item(), UNUSED_ROUTE_TTL));
            let revocation: Arc<dyn PlaybackRouteRevocation> = revoked;
            (
                PlaybackRouteLease::new(revocation),
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            )
        };
        let route = RoutedPlayback::new(
            1234,
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_owned(),
            lease,
        )
        .unwrap();
        let debug = format!("{route:?}");
        assert!(!debug.contains("0123456789abcdef"));
        assert_eq!(debug, "RoutedPlayback(<redacted>)");

        let origin = ProxyOrigin::from_url(
            &reqwest::Url::parse("https://private.example.test:8443/stream").unwrap(),
        )
        .unwrap();
        assert_eq!(format!("{origin:?}"), "ProxyOrigin(<redacted>)");
    }

    #[tokio::test(start_paused = true)]
    async fn a_live_episode_is_revoked_by_its_lease_not_wall_clock_time() {
        let route = Arc::new(RouteState::new(item(), UNUSED_ROUTE_TTL));
        let revocation: Arc<dyn PlaybackRouteRevocation> = route.clone();
        let lease = PlaybackRouteLease::new(revocation);

        assert!(route.try_admit());
        tokio::time::advance(Duration::from_secs(5 * 60 * 60)).await;
        assert!(route.is_live());
        assert!(
            route.try_admit(),
            "admitted route supports Range reconnects"
        );

        drop(lease);
        assert!(!route.is_live());
        assert!(!route.try_admit());
    }
}
