//! The `ytt -r <command>` client: a short-lived process that connects to the running
//! instance, sends one command, prints the result, and exits.
//!
//! Critically, this path NEVER touches terminal raw mode or the alternate screen (no
//! `tui::init`, no graphics probe) — it must leave the caller's terminal pristine so it's
//! safe to wire to a window-manager keybinding or a status-bar click.
//!
//! Exit codes follow the i3-msg / swaymsg convention:
//!   0 = applied, 1 = transport / no running instance, 2 = usage or semantic rejection.

use std::time::Duration;

use interprocess::local_socket::GenericFilePath;
use interprocess::local_socket::tokio::Stream;
use interprocess::local_socket::tokio::prelude::*;
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::time::timeout;

use super::args::{self, Invocation, ParseError, Parsed};
use super::endpoint;
use super::proto::{
    CONFIRMATION_LOST_REASON, InstanceFile, InstanceMode, MAX_ONESHOT_REPLY_BYTES,
    PROTOCOL_VERSION, PROTOCOL_VERSION_V7, RETAINED_REQUEST_OUTCOMES_CAPABILITY, RemoteCommand,
    RemoteRequest, RemoteResponse, RemoteResponseEnvelope, RequestRetryClass, StatusSnapshot,
};
use crate::search_source::SearchSource;
use crate::streaming::StreamingMode;

const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);

const EXIT_OK: i32 = 0;
const EXIT_TRANSPORT: i32 = 1;
const EXIT_USAGE: i32 = 2;

/// Transport-level failures from the local control socket. Semantic command rejection is
/// represented by an `ok: false` [`RemoteResponse`], not by this error type.
#[derive(Debug, PartialEq, Eq)]
pub enum ClientError {
    NoRunningInstance,
    CurrentDescriptor(endpoint::CurrentInstanceError),
    MalformedEndpoint,
    UnsupportedProtocol,
    ConnectFailed,
    EncodeFailed(String),
    WriteFailed(String),
    FlushFailed(String),
    NoResponse,
    MalformedResponse,
    /// A mutating request may have reached the owner, but neither the original exchange nor the
    /// same-ID retry produced an authoritative result.
    ConfirmationLost,
}

impl ClientError {
    pub fn human_message(&self) -> String {
        match self {
            ClientError::NoRunningInstance => {
                "no running ytt instance found — start one with `ytt`.".to_string()
            }
            ClientError::CurrentDescriptor(error) => error.to_string(),
            ClientError::MalformedEndpoint => {
                "malformed endpoint in the instance descriptor.".to_string()
            }
            ClientError::UnsupportedProtocol => {
                "the running ytt instance does not support remote protocol v7 or v8.".to_string()
            }
            ClientError::ConnectFailed => {
                "could not reach ytt (it may have exited) — start one with `ytt`.".to_string()
            }
            ClientError::EncodeFailed(e) => format!("could not encode request: {e}"),
            ClientError::WriteFailed(e) => format!("write failed: {e}"),
            ClientError::FlushFailed(e) => format!("flush failed: {e}"),
            ClientError::NoResponse => "no response from ytt.".to_string(),
            ClientError::MalformedResponse => "malformed response from ytt.".to_string(),
            ClientError::ConfirmationLost => {
                "confirmation lost — the action may have been applied; check current state before retrying."
                    .to_string()
            }
        }
    }
}

/// A descriptor exists, but its owner predates a caller-required additive capability.
/// Kept separate from [`ClientError`] so adding capability discovery does not add a variant to
/// the established transport-error enum and break exhaustive downstream matches.
#[derive(Debug, PartialEq, Eq)]
pub struct MissingCapability {
    capability: String,
}

/// Personal-export owner discovery must fail closed when the descriptor exists but cannot be
/// trusted; only a genuinely absent descriptor permits an offline disk snapshot.
#[derive(Debug, PartialEq, Eq)]
pub enum CapabilityLookupError {
    Missing(MissingCapability),
    InvalidDescriptor,
    UnpublishedOwner,
    OwnerProbeFailed,
}

impl CapabilityLookupError {
    pub fn human_message(&self) -> String {
        match self {
            Self::Missing(error) => error.human_message(),
            Self::InvalidDescriptor => "the ytt instance descriptor is unreadable or corrupt \
                 — restart ytt before exporting; refusing a possibly stale disk fallback."
                .to_string(),
            Self::UnpublishedOwner => "a primary ytt control socket is running without a usable \
                 instance descriptor — quit or restart ytt before exporting; refusing a stale \
                 disk fallback."
                .to_string(),
            Self::OwnerProbeFailed => "could not safely rule out a running primary ytt instance \
                 — quit or restart ytt before exporting, then retry."
                .to_string(),
        }
    }
}

impl MissingCapability {
    pub fn human_message(&self) -> String {
        format!(
            "the running ytt instance does not support `{}` \
             — restart it with this ytt version, then retry.",
            self.capability
        )
    }
}

/// Entry point from `main` for `ytt -r …`. Parses args, runs the exchange on a tiny
/// current-thread runtime, and returns the process exit code. Never returns to the normal
/// TUI startup path.
pub fn run(args_in: &[String]) -> i32 {
    let parsed = match args::parse(args_in) {
        Ok(p) => p,
        Err(ParseError::Usage(text)) => {
            print!("{text}");
            return EXIT_OK;
        }
        Err(ParseError::Invalid(msg)) => {
            eprintln!("ytt -r: {msg}");
            return EXIT_USAGE;
        }
    };

    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("ytt -r: could not start runtime: {e}");
            return EXIT_TRANSPORT;
        }
    };
    rt.block_on(exchange_for_cli(parsed))
}

/// Send one semantic command to the running instance and return its raw response.
///
/// This is the reusable control path for both `ytt -r` and desktop companions. It never touches
/// terminal state and never prints; callers decide how to render success, rejection, or transport
/// failures.
pub async fn send(command: RemoteCommand) -> Result<RemoteResponse, ClientError> {
    let instance = endpoint::read_current_instance().map_err(ClientError::CurrentDescriptor)?;
    send_to(instance, command).await
}

/// Resolve the advertised owner only when it explicitly supports `capability`.
///
/// `Ok(None)` is the only discovery result that directly permits an offline disk fallback. A
/// present-but-old descriptor is an error. A later transport failure from [`send_to`] does not by
/// itself permit fallback: stale-descriptor recovery must also hold the exclusive data guard and
/// use [`prove_primary_endpoints_absent`] before reading persisted state.
pub async fn instance_with_capability(
    capability: &str,
) -> Result<Option<InstanceFile>, CapabilityLookupError> {
    let instance = match endpoint::read_current_instance() {
        Ok(instance) => instance,
        Err(endpoint::CurrentInstanceError::NotFound) => {
            prove_primary_endpoints_absent().await?;
            return Ok(None);
        }
        Err(_) => return Err(CapabilityLookupError::InvalidDescriptor),
    };
    require_capability(instance, capability)
        .map(Some)
        .map_err(CapabilityLookupError::Missing)
}

/// Prove that neither the current nor legacy primary control endpoint has a listener.
///
/// This intentionally ignores the instance descriptor: callers use it only after a connection
/// to an otherwise valid advertised owner failed. `Ok(())` means every canonical endpoint
/// returned an OS error that proves absence; live, malformed, timed-out, permission-denied, and
/// otherwise ambiguous endpoints fail closed. The descriptor is never removed or rewritten.
pub async fn prove_primary_endpoints_absent() -> Result<(), CapabilityLookupError> {
    let primary =
        endpoint::socket_endpoint().map_err(|_| CapabilityLookupError::OwnerProbeFailed)?;
    let legacy = endpoint::legacy_primary_endpoint_for_probe();
    let endpoints = if legacy == primary {
        vec![primary]
    } else {
        vec![primary, legacy]
    };
    probe_primary_endpoints(&endpoints).await
}

async fn probe_primary_endpoints(endpoints: &[String]) -> Result<(), CapabilityLookupError> {
    for endpoint in endpoints {
        let name = endpoint
            .as_str()
            .to_fs_name::<GenericFilePath>()
            .map_err(|_| CapabilityLookupError::OwnerProbeFailed)?;
        match timeout(CONNECT_TIMEOUT, Stream::connect(name)).await {
            Ok(Ok(connection)) => {
                drop(connection);
                return Err(CapabilityLookupError::UnpublishedOwner);
            }
            Ok(Err(error)) if probe_error_proves_absence(endpoint, &error) => {}
            // A timeout, permission failure, busy named pipe, or any other ambiguous result is
            // not evidence that persisted state has no live owner. Personal export fails closed.
            _ => return Err(CapabilityLookupError::OwnerProbeFailed),
        }
    }
    Ok(())
}

fn probe_error_proves_absence(_endpoint: &str, error: &std::io::Error) -> bool {
    if matches!(
        error.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
    ) {
        return true;
    }

    #[cfg(unix)]
    {
        // GenericFilePath uses the endpoint verbatim. A NUL-free absolute filesystem path that
        // the OS rejects as InvalidInput (not PermissionDenied/TimedOut) cannot fit in sockaddr_un
        // and therefore cannot be the name of a live listener. The server uses the same mapping.
        if error.kind() == std::io::ErrorKind::InvalidInput
            && std::path::Path::new(_endpoint).is_absolute()
            && !_endpoint.as_bytes().contains(&0)
        {
            return true;
        }
    }

    false
}

fn require_capability(
    instance: InstanceFile,
    capability: &str,
) -> Result<InstanceFile, MissingCapability> {
    if instance
        .capabilities
        .iter()
        .any(|advertised| advertised == capability)
    {
        Ok(instance)
    } else {
        Err(MissingCapability {
            capability: capability.to_string(),
        })
    }
}

/// Send to a descriptor already selected by the caller. This keeps capability selection and
/// the exchange tied to the same owner descriptor instead of re-reading it between the checks.
async fn send_current(command: RemoteCommand) -> Result<RemoteResponse, ClientError> {
    let instance = endpoint::read_current_instance().map_err(ClientError::CurrentDescriptor)?;
    send_to(instance, command).await
}

pub async fn send_to(
    instance: InstanceFile,
    command: RemoteCommand,
) -> Result<RemoteResponse, ClientError> {
    let request_id = super::requests::fresh_request_id();
    send_to_instance_with_request_id(instance, command, &request_id).await
}

#[cfg(test)]
async fn send_to_instance(
    instance: InstanceFile,
    command: RemoteCommand,
) -> Result<RemoteResponse, ClientError> {
    send_to(instance, command).await
}

async fn send_to_instance_with_request_id(
    instance: InstanceFile,
    command: RemoteCommand,
    request_id: &str,
) -> Result<RemoteResponse, ClientError> {
    let version = negotiated_oneshot_version(instance.protocol_version)?;
    let first = send_attempt(&instance, &command, request_id, version).await;
    if attempt_is_ambiguous(&first) {
        let requires_confirmation = command.requires_confirmation();
        let retry_class = command.request_retry_class();
        if retry_class == RequestRetryClass::RetainedOutcome
            && (version < PROTOCOL_VERSION
                || !instance
                    .capabilities
                    .iter()
                    .any(|capability| capability == RETAINED_REQUEST_OUTCOMES_CAPABILITY))
        {
            // A v8 version number alone is not evidence of deduplication: older v8 servers ignore
            // the additive request ID. Retrying a mutation against one could apply it twice.
            return Err(ClientError::ConfirmationLost);
        }
        // Retained mutations can observe the first owner's late completion without a second
        // admission; explicitly reexecuted queries instead issue safe fresh work. Client and
        // server use the same nominal deadline, so a timeout can race the response write and
        // surface as `NoResponse`; read failures and malformed/truncated replies are equally
        // post-flush and therefore ambiguous. A write/flush failure can likewise happen after
        // the complete request reached the server. Every retry reuses this exact identity.
        let retry = send_attempt(&instance, &command, request_id, version).await;
        let retry_is_authoritative = match retry_class {
            RequestRetryClass::RetainedOutcome => attempt_is_retained_replay(&retry),
            // RunSearch is deliberately executed afresh: an `ok` acknowledgement proves at
            // least one dispatch, while a pre-admission rejection cannot resolve whether the
            // first attempt dispatched. Status does not require confirmation and accepts any
            // well-formed fresh retry response below.
            RequestRetryClass::ReexecuteReadOnly => {
                !requires_confirmation || attempt_confirms_fresh_dispatch(&retry)
            }
        };
        if !retry_is_authoritative {
            return Err(ClientError::ConfirmationLost);
        }
        return retry.map(|envelope| envelope.response);
    }
    first.map(|envelope| envelope.response)
}

fn attempt_is_ambiguous(result: &Result<RemoteResponseEnvelope, ClientError>) -> bool {
    matches!(
        result,
        Ok(envelope) if matches!(
            envelope.response.reason.as_deref(),
            Some("timeout" | CONFIRMATION_LOST_REASON)
        )
    ) || matches!(
        result,
        Err(ClientError::NoResponse
            | ClientError::MalformedResponse
            | ClientError::WriteFailed(_)
            | ClientError::FlushFailed(_))
    )
}

fn attempt_is_retained_replay(result: &Result<RemoteResponseEnvelope, ClientError>) -> bool {
    matches!(result, Ok(envelope) if envelope.retained_replay && !matches!(
        envelope.response.reason.as_deref(),
        Some("timeout" | CONFIRMATION_LOST_REASON)
    ))
}

fn attempt_confirms_fresh_dispatch(result: &Result<RemoteResponseEnvelope, ClientError>) -> bool {
    matches!(result, Ok(envelope) if envelope.response.ok)
}

async fn send_attempt(
    instance: &InstanceFile,
    command: &RemoteCommand,
    request_id: &str,
    version: u8,
) -> Result<RemoteResponseEnvelope, ClientError> {
    let Ok(name) = instance.endpoint.as_str().to_fs_name::<GenericFilePath>() else {
        return Err(ClientError::MalformedEndpoint);
    };

    let conn = match timeout(CONNECT_TIMEOUT, Stream::connect(name)).await {
        Ok(Ok(c)) => c,
        // Connect refused / timed out: the descriptor is stale or the instance just exited.
        _ => return Err(ClientError::ConnectFailed),
    };

    let reply_timeout = super::reply_timeout_for(command);
    let req = RemoteRequest {
        version,
        token: instance.token.clone(),
        request_id: (version >= PROTOCOL_VERSION).then(|| request_id.to_owned()),
        command: command.clone(),
    };
    let mut payload = match serde_json::to_vec(&req) {
        Ok(v) => v,
        Err(e) => return Err(ClientError::EncodeFailed(e.to_string())),
    };
    payload.push(b'\n');

    {
        let mut writer = &conn;
        write_request(&mut writer, &payload, WRITE_TIMEOUT).await?;
    }

    let mut reader = BufReader::new(&conn);
    let line = match timeout(reply_timeout, read_bounded_line(&mut reader)).await {
        Ok(Ok(Some(line))) => line,
        Ok(Ok(None)) => return Err(ClientError::NoResponse),
        Ok(Err(_)) => return Err(ClientError::MalformedResponse),
        Err(_) => return Err(ClientError::NoResponse),
    };
    serde_json::from_str(line.trim()).map_err(|_| ClientError::MalformedResponse)
}

async fn write_request<W: AsyncWrite + Unpin>(
    writer: &mut W,
    payload: &[u8],
    write_timeout: Duration,
) -> Result<(), ClientError> {
    let write = async {
        writer
            .write_all(payload)
            .await
            .map_err(|e| ClientError::WriteFailed(e.to_string()))?;
        writer
            .flush()
            .await
            .map_err(|e| ClientError::FlushFailed(e.to_string()))
    };
    match timeout(write_timeout, write).await {
        Ok(result) => result,
        Err(_) => Err(ClientError::WriteFailed(
            "remote request write timed out".to_string(),
        )),
    }
}

async fn exchange_for_cli(parsed: Parsed) -> i32 {
    let Parsed {
        invocation,
        quiet,
        json,
    } = parsed;
    match invocation {
        Invocation::Command(command) => {
            let current_only = matches!(command, RemoteCommand::QueuePlay { .. });
            let response = if current_only {
                send_current(command).await
            } else {
                send(command).await
            };
            exchange_default_response(response, json, quiet)
        }
        Invocation::Info => exchange_info(json, quiet).await,
        Invocation::QueueList => exchange_status_projection(json, quiet, queue_human).await,
        Invocation::SettingsShow => exchange_status_projection(json, quiet, settings_human).await,
        Invocation::Watch { topics } => super::watch::run(topics, json, quiet).await,
    }
}

fn exchange_default_response(
    response: Result<RemoteResponse, ClientError>,
    json: bool,
    quiet: bool,
) -> i32 {
    let resp = match response {
        Ok(r) => r,
        Err(e) => {
            eprintln!("ytt -r: {}", e.human_message());
            return EXIT_TRANSPORT;
        }
    };

    if resp.ok {
        if json {
            return print_json(&resp);
        }
        if !quiet && let Some(msg) = &resp.message {
            println!("{msg}");
        }
        EXIT_OK
    } else {
        // Errors always print, even under `-q`. The machine reason is the actionable bit.
        let reason = resp.reason.as_deref().unwrap_or("rejected");
        eprintln!("ytt -r: {reason}");
        EXIT_USAGE
    }
}

async fn exchange_info(json: bool, quiet: bool) -> i32 {
    let instance = match endpoint::read_current_instance() {
        Ok(instance) => instance,
        Err(error) => {
            eprintln!("ytt -r: {error}");
            return EXIT_TRANSPORT;
        }
    };
    // A descriptor can be stale. Authenticate a harmless status request before claiming that
    // its owner is alive, then render only the explicitly allow-listed descriptor fields.
    let response = match send_to(instance.clone(), RemoteCommand::Status).await {
        Ok(response) => response,
        Err(error) => {
            eprintln!("ytt -r: {}", error.human_message());
            return EXIT_TRANSPORT;
        }
    };
    if !response.ok {
        eprintln!("ytt -r: {}", info_rejection_message(&response));
        return EXIT_USAGE;
    }
    if response.status.is_none() {
        eprintln!("ytt -r: {}", ClientError::MalformedResponse.human_message());
        return EXIT_TRANSPORT;
    }

    let info = SanitizedInfo::from(instance);
    if json {
        print_json(&info)
    } else {
        if !quiet {
            println!("{}", info.human_line());
        }
        EXIT_OK
    }
}

async fn exchange_status_projection(
    json: bool,
    quiet: bool,
    formatter: fn(&StatusSnapshot) -> String,
) -> i32 {
    let response = match send_current(RemoteCommand::Status).await {
        Ok(response) => response,
        Err(error) => {
            eprintln!("ytt -r: {}", error.human_message());
            return EXIT_TRANSPORT;
        }
    };
    if !response.ok {
        return print_rejection(&response);
    }
    let Some(status) = response.status.as_ref() else {
        eprintln!("ytt -r: {}", ClientError::MalformedResponse.human_message());
        return EXIT_TRANSPORT;
    };
    if json {
        return print_json(&response);
    }
    if !quiet {
        println!("{}", formatter(status));
    }
    EXIT_OK
}

fn print_json<T: Serialize>(value: &T) -> i32 {
    match serde_json::to_string(value) {
        Ok(line) => {
            println!("{line}");
            EXIT_OK
        }
        Err(error) => {
            eprintln!("ytt -r: could not encode response: {error}");
            EXIT_TRANSPORT
        }
    }
}

fn print_rejection(response: &RemoteResponse) -> i32 {
    let reason = response.reason.as_deref().unwrap_or("rejected");
    eprintln!("ytt -r: {reason}");
    EXIT_USAGE
}

fn negotiated_oneshot_version(owner_version: u8) -> Result<u8, ClientError> {
    if owner_version < PROTOCOL_VERSION_V7 {
        Err(ClientError::UnsupportedProtocol)
    } else {
        Ok(owner_version.min(PROTOCOL_VERSION))
    }
}

#[derive(Debug, Serialize)]
struct SanitizedInfo {
    app_pid: u32,
    created_unix: u64,
    mode: InstanceMode,
    protocol_version: u8,
    capabilities: Vec<String>,
}

impl From<InstanceFile> for SanitizedInfo {
    fn from(instance: InstanceFile) -> Self {
        let InstanceFile {
            app_pid,
            endpoint,
            token,
            created_unix,
            mode,
            protocol_version,
            mut capabilities,
        } = instance;
        capabilities
            .retain(|capability| !reflects_descriptor_secret(capability, &endpoint, &token));
        capabilities.sort_unstable();
        Self {
            app_pid,
            created_unix,
            mode,
            protocol_version,
            capabilities,
        }
    }
}

fn reflects_descriptor_secret(capability: &str, endpoint: &str, token: &str) -> bool {
    [endpoint, token]
        .into_iter()
        .any(|secret| contains_ascii_case_insensitive(capability, secret))
}

fn contains_ascii_case_insensitive(value: &str, secret: &str) -> bool {
    !secret.is_empty()
        && value
            .as_bytes()
            .windows(secret.len())
            .any(|window| window.eq_ignore_ascii_case(secret.as_bytes()))
}

fn info_rejection_message(response: &RemoteResponse) -> &'static str {
    debug_assert!(!response.ok);
    "info_status_rejected"
}

impl SanitizedInfo {
    fn human_line(&self) -> String {
        let capabilities = if self.capabilities.is_empty() {
            "none".to_string()
        } else {
            self.capabilities
                .iter()
                .map(|capability| sanitize_human_text(capability))
                .collect::<Vec<_>>()
                .join(",")
        };
        format!(
            "pid {}  •  mode {}  •  protocol {}  •  capabilities {}",
            self.app_pid,
            instance_mode_name(self.mode),
            self.protocol_version,
            capabilities
        )
    }
}

fn queue_human(status: &StatusSnapshot) -> String {
    if status.queue.is_empty() {
        return "queue empty".to_string();
    }

    status
        .queue
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let marker = if item.current { '>' } else { ' ' };
            let title = sanitize_human_text(&item.title);
            let artist = sanitize_human_text(&item.artist);
            let duration = sanitize_human_text(&item.duration);
            let mut track = if title.is_empty() {
                "(untitled)".to_string()
            } else {
                title
            };
            if !artist.is_empty() {
                track.push_str(" — ");
                track.push_str(&artist);
            }
            if !duration.is_empty() {
                track.push_str("  [");
                track.push_str(&duration);
                track.push(']');
            }
            format!("{marker} {}. {track}", index + 1)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn settings_human(status: &StatusSnapshot) -> String {
    let settings = &status.settings;
    format!(
        "autoplay={}  •  source={}  •  mode={}  •  speed={}.{}x  •  seek={}s  •  normalize={}  •  gapless={}  •  ai={}  •  radio-mode={}",
        on_off(settings.autoplay_streaming),
        search_source_name(settings.streaming_source),
        streaming_mode_name(settings.streaming_mode),
        settings.speed_tenths / 10,
        settings.speed_tenths % 10,
        settings.seek_seconds,
        on_off(settings.normalize),
        on_off(settings.gapless),
        on_off(settings.ai_enabled),
        on_off(settings.radio_mode),
    )
}

fn sanitize_human_text(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut pending_space = false;
    for ch in input.chars() {
        let unsafe_format = ch.is_control()
            || matches!(
                ch,
                '\u{200e}'
                    | '\u{200f}'
                    | '\u{2028}'..='\u{202e}'
                    | '\u{2066}'..='\u{2069}'
            );
        if unsafe_format || ch.is_whitespace() {
            pending_space = !output.is_empty();
        } else {
            if pending_space {
                output.push(' ');
                pending_space = false;
            }
            output.push(ch);
        }
    }
    output
}

fn on_off(value: bool) -> &'static str {
    if value { "on" } else { "off" }
}

fn instance_mode_name(mode: InstanceMode) -> &'static str {
    match mode {
        InstanceMode::StandaloneTui => "standalone_tui",
        InstanceMode::Daemon => "daemon",
    }
}

fn streaming_mode_name(mode: StreamingMode) -> &'static str {
    match mode {
        StreamingMode::Focused => "focused",
        StreamingMode::Balanced => "balanced",
        StreamingMode::Discovery => "discovery",
    }
}

fn search_source_name(source: SearchSource) -> &'static str {
    match source {
        SearchSource::Youtube => "youtube",
        SearchSource::SoundCloud => "soundcloud",
        SearchSource::Audius => "audius",
        SearchSource::Jamendo => "jamendo",
        SearchSource::InternetArchive => "internet_archive",
        SearchSource::RadioBrowser => "radio_browser",
        SearchSource::All => "all",
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::remote::proto::{InstanceMode, PROTOCOL_VERSION_V7};
    use crate::remote::requests::{CommandDeduper, RequestKey};
    use interprocess::local_socket::ListenerOptions;
    use interprocess::local_socket::tokio::Listener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::AsyncBufReadExt;

    fn test_endpoint(name: &str) -> String {
        std::env::temp_dir()
            .join(format!("yututui-client-{name}-{}.sock", std::process::id()))
            .to_string_lossy()
            .into_owned()
    }

    fn test_instance(endpoint: String) -> InstanceFile {
        InstanceFile {
            app_pid: std::process::id(),
            endpoint,
            token: "secret".to_string(),
            created_unix: 1,
            mode: InstanceMode::StandaloneTui,
            protocol_version: PROTOCOL_VERSION,
            capabilities: vec![
                "remote-control".to_string(),
                "status".to_string(),
                RETAINED_REQUEST_OUTCOMES_CAPABILITY.to_string(),
            ],
        }
    }

    fn legacy_v8_instance(endpoint: String) -> InstanceFile {
        let mut instance = test_instance(endpoint);
        instance
            .capabilities
            .retain(|capability| capability != RETAINED_REQUEST_OUTCOMES_CAPABILITY);
        instance
    }

    fn bind_test_listener(endpoint: &str) -> Listener {
        let _ = std::fs::remove_file(endpoint);
        let name = endpoint.to_fs_name::<GenericFilePath>().unwrap();
        ListenerOptions::new()
            .name(name)
            .reclaim_name(false)
            .create_tokio()
            .unwrap()
    }

    async fn serve_one_response(listener: Listener, response_line: String, expected_version: u8) {
        let conn = listener.accept().await.unwrap();
        {
            let mut reader = BufReader::new(&conn);
            let mut request = String::new();
            reader.read_line(&mut request).await.unwrap();
            let request: RemoteRequest = serde_json::from_str(request.trim()).unwrap();
            assert_eq!(request.version, expected_version);
            assert_eq!(request.token, "secret");
            assert_eq!(
                request.request_id.is_some(),
                expected_version >= PROTOCOL_VERSION
            );
        }
        let mut writer = &conn;
        writer.write_all(response_line.as_bytes()).await.unwrap();
        writer.write_all(b"\n").await.unwrap();
        writer.flush().await.unwrap();
    }

    async fn accept_requests_without_response(listener: Listener, expected_requests: usize) {
        for _ in 0..expected_requests {
            let conn = listener.accept().await.unwrap();
            let mut reader = BufReader::new(&conn);
            let mut request = String::new();
            reader.read_line(&mut request).await.unwrap();
            let _: RemoteRequest = serde_json::from_str(request.trim()).unwrap();
        }
    }

    async fn serve_timeout_then_success(listener: Listener) -> [Option<String>; 2] {
        let responses = [
            RemoteResponseEnvelope {
                response: RemoteResponse::err(CONFIRMATION_LOST_REASON),
                retained_replay: false,
            },
            RemoteResponseEnvelope {
                response: RemoteResponse::ok("late success".to_owned()),
                retained_replay: true,
            },
        ];
        let mut request_ids: [Option<String>; 2] = [None, None];
        for (index, response) in responses.into_iter().enumerate() {
            let conn = listener.accept().await.unwrap();
            let mut reader = BufReader::new(&conn);
            let mut request = String::new();
            reader.read_line(&mut request).await.unwrap();
            let request: RemoteRequest = serde_json::from_str(request.trim()).unwrap();
            request_ids[index] = request.request_id;

            let mut writer = &conn;
            let mut response = serde_json::to_vec(&response).unwrap();
            response.push(b'\n');
            writer.write_all(&response).await.unwrap();
            writer.flush().await.unwrap();
        }
        request_ids
    }

    async fn serve_eof_then_success(listener: Listener) -> [Option<String>; 2] {
        let mut request_ids: [Option<String>; 2] = [None, None];
        for (index, request_id) in request_ids.iter_mut().enumerate() {
            let conn = listener.accept().await.unwrap();
            let mut reader = BufReader::new(&conn);
            let mut request = String::new();
            reader.read_line(&mut request).await.unwrap();
            let request: RemoteRequest = serde_json::from_str(request.trim()).unwrap();
            *request_id = request.request_id;

            if index == 1 {
                let mut writer = &conn;
                let mut response = serde_json::to_vec(&RemoteResponseEnvelope {
                    response: RemoteResponse::ok("recovered after ambiguous EOF".to_owned()),
                    retained_replay: true,
                })
                .unwrap();
                response.push(b'\n');
                writer.write_all(&response).await.unwrap();
                writer.flush().await.unwrap();
            }
        }
        request_ids
    }

    async fn serve_corrupt_then_cached_then_fresh(
        listener: Listener,
    ) -> ([Option<String>; 3], usize) {
        let requests = CommandDeduper::default();
        let executions = Arc::new(AtomicUsize::new(0));
        let mut request_ids: [Option<String>; 3] = [None, None, None];

        for (index, request_id) in request_ids.iter_mut().enumerate() {
            let conn = listener.accept().await.unwrap();
            let mut reader = BufReader::new(&conn);
            let mut request = String::new();
            reader.read_line(&mut request).await.unwrap();
            let request: RemoteRequest = serde_json::from_str(request.trim()).unwrap();
            *request_id = request.request_id.clone();

            let key = RequestKey::stable(request.request_id.unwrap()).unwrap();
            let executions_for_emit = Arc::clone(&executions);
            let response = requests
                .execute_with_replay_proof(
                    key,
                    &request.command,
                    Duration::from_secs(1),
                    move |reply| {
                        let execution = executions_for_emit.fetch_add(1, Ordering::Relaxed) + 1;
                        reply
                            .send(RemoteResponse::ok(format!("execution {execution}")))
                            .unwrap();
                        true
                    },
                )
                .await;

            let mut writer = &conn;
            if index == 0 {
                // The mutation completed and its response is cached, but the first caller sees
                // an invalid response. Its retry must join that cache entry instead of executing
                // the mutation again.
                writer.write_all(b"{truncated\n").await.unwrap();
            } else {
                let response = RemoteResponseEnvelope {
                    response: response.response,
                    retained_replay: response.retained_replay,
                };
                let mut response = serde_json::to_vec(&response).unwrap();
                response.push(b'\n');
                writer.write_all(&response).await.unwrap();
            }
            writer.flush().await.unwrap();
        }

        (request_ids, executions.load(Ordering::Relaxed))
    }

    async fn serve_two_malformed_responses(listener: Listener) -> [Option<String>; 2] {
        let mut request_ids: [Option<String>; 2] = [None, None];
        for request_id in &mut request_ids {
            let conn = listener.accept().await.unwrap();
            let mut reader = BufReader::new(&conn);
            let mut request = String::new();
            reader.read_line(&mut request).await.unwrap();
            let request: RemoteRequest = serde_json::from_str(request.trim()).unwrap();
            *request_id = request.request_id;

            let mut writer = &conn;
            writer.write_all(b"{not json}\n").await.unwrap();
            writer.flush().await.unwrap();
        }
        request_ids
    }

    async fn serve_eof_then_fresh_response(
        listener: Listener,
        response: RemoteResponse,
    ) -> [Option<String>; 2] {
        let mut request_ids: [Option<String>; 2] = [None, None];
        for (index, request_id) in request_ids.iter_mut().enumerate() {
            let conn = listener.accept().await.unwrap();
            let mut reader = BufReader::new(&conn);
            let mut request = String::new();
            reader.read_line(&mut request).await.unwrap();
            let request: RemoteRequest = serde_json::from_str(request.trim()).unwrap();
            *request_id = request.request_id;

            if index == 1 {
                let mut writer = &conn;
                let envelope = RemoteResponseEnvelope {
                    response: response.clone(),
                    retained_replay: false,
                };
                let mut response = serde_json::to_vec(&envelope).unwrap();
                response.push(b'\n');
                writer.write_all(&response).await.unwrap();
                writer.flush().await.unwrap();
            }
        }
        request_ids
    }

    async fn serve_legacy_eof_and_detect_retry(listener: Listener) -> usize {
        let conn = listener.accept().await.unwrap();
        let mut reader = BufReader::new(&conn);
        let mut request = String::new();
        reader.read_line(&mut request).await.unwrap();
        let _: RemoteRequest = serde_json::from_str(request.trim()).unwrap();
        drop(reader);
        drop(conn);

        if timeout(Duration::from_millis(100), listener.accept())
            .await
            .is_ok()
        {
            2
        } else {
            1
        }
    }

    #[tokio::test]
    async fn send_to_instance_round_trips_status_response() {
        let endpoint = test_endpoint("status");
        let listener = bind_test_listener(&endpoint);
        let snapshot = crate::remote::proto::StatusSnapshot {
            title: Some("Song".to_string()),
            artist: Some("Artist".to_string()),
            paused: false,
            volume: 75,
            position: 1,
            total: 2,
            streaming: true,
            owner_mode: InstanceMode::StandaloneTui,
            settings: Default::default(),
            queue: Vec::new(),
            shuffle: false,
            repeat: Default::default(),
            elapsed_ms: None,
            duration_ms: None,
            is_live: false,
            queue_rev: None,
            track_id: None,
            position_epoch: 0,
            artwork: None,
        };
        let response = serde_json::to_string(&RemoteResponse::status(snapshot.clone())).unwrap();
        let server = tokio::spawn(serve_one_response(listener, response, PROTOCOL_VERSION));

        let resp = send_to(test_instance(endpoint.clone()), RemoteCommand::Status)
            .await
            .unwrap();
        server.await.unwrap();
        let _ = std::fs::remove_file(endpoint);

        assert!(resp.ok);
        assert_eq!(resp.status, Some(snapshot));
    }

    #[tokio::test]
    async fn send_to_instance_preserves_semantic_rejection_response() {
        let endpoint = test_endpoint("rejected");
        let listener = bind_test_listener(&endpoint);
        let response = serde_json::to_string(&RemoteResponse::err("queue_empty")).unwrap();
        let server = tokio::spawn(serve_one_response(listener, response, PROTOCOL_VERSION));

        let resp = send_to(test_instance(endpoint.clone()), RemoteCommand::Next)
            .await
            .unwrap();
        server.await.unwrap();
        let _ = std::fs::remove_file(endpoint);

        assert!(!resp.ok);
        assert_eq!(resp.reason.as_deref(), Some("queue_empty"));
    }

    #[tokio::test]
    async fn semantic_timeout_retries_once_with_the_same_request_identity() {
        let endpoint = test_endpoint("retry-identity");
        let listener = bind_test_listener(&endpoint);
        let server = tokio::spawn(serve_timeout_then_success(listener));

        let resp = send_to_instance(test_instance(endpoint.clone()), RemoteCommand::TogglePause)
            .await
            .unwrap();
        let request_ids = server.await.unwrap();
        let _ = std::fs::remove_file(endpoint);

        assert!(resp.ok);
        assert_eq!(resp.message.as_deref(), Some("late success"));
        assert!(request_ids[0].is_some());
        assert_eq!(request_ids[0], request_ids[1]);
    }

    #[tokio::test]
    async fn no_response_retries_once_with_the_same_request_identity() {
        let endpoint = test_endpoint("retry-no-response");
        let listener = bind_test_listener(&endpoint);
        let server = tokio::spawn(serve_eof_then_success(listener));

        let resp = send_to_instance(test_instance(endpoint.clone()), RemoteCommand::TogglePause)
            .await
            .unwrap();
        let request_ids = server.await.unwrap();
        let _ = std::fs::remove_file(endpoint);

        assert!(resp.ok);
        assert_eq!(
            resp.message.as_deref(),
            Some("recovered after ambiguous EOF")
        );
        assert!(request_ids[0].is_some());
        assert_eq!(request_ids[0], request_ids[1]);
    }

    #[tokio::test]
    async fn old_v8_owner_without_retained_outcome_capability_is_not_retried() {
        let endpoint = test_endpoint("legacy-v8-no-retry");
        let listener = bind_test_listener(&endpoint);
        let server = tokio::spawn(serve_legacy_eof_and_detect_retry(listener));

        let err = send_to_instance(
            legacy_v8_instance(endpoint.clone()),
            RemoteCommand::TogglePause,
        )
        .await
        .unwrap_err();
        let accepted = server.await.unwrap();
        let _ = std::fs::remove_file(endpoint);

        assert_eq!(err, ClientError::ConfirmationLost);
        assert_eq!(accepted, 1, "an old v8 owner must not receive a retry");
    }

    #[tokio::test]
    async fn quiescing_owner_rejection_does_not_overwrite_ambiguous_mutation() {
        let endpoint = test_endpoint("retry-quiescing-owner");
        let listener = bind_test_listener(&endpoint);
        let server = tokio::spawn(serve_eof_then_fresh_response(
            listener,
            RemoteResponse::err("shutting_down"),
        ));

        let err = send_to_instance(test_instance(endpoint.clone()), RemoteCommand::TogglePause)
            .await
            .unwrap_err();
        let request_ids = server.await.unwrap();
        let _ = std::fs::remove_file(endpoint);

        assert_eq!(err, ClientError::ConfirmationLost);
        assert_eq!(request_ids[0], request_ids[1]);
    }

    #[tokio::test]
    async fn successor_bad_token_rejection_does_not_overwrite_ambiguous_mutation() {
        let endpoint = test_endpoint("retry-successor-bad-token");
        let listener = bind_test_listener(&endpoint);
        let server = tokio::spawn(serve_eof_then_fresh_response(
            listener,
            RemoteResponse::err("bad_token"),
        ));

        let err = send_to_instance(test_instance(endpoint.clone()), RemoteCommand::TogglePause)
            .await
            .unwrap_err();
        let request_ids = server.await.unwrap();
        let _ = std::fs::remove_file(endpoint);

        assert_eq!(err, ClientError::ConfirmationLost);
        assert_eq!(request_ids[0], request_ids[1]);
    }

    #[tokio::test]
    async fn run_search_accepts_a_fresh_retry_ack_without_retained_capability_or_proof() {
        let endpoint = test_endpoint("search-fresh-retry-ack");
        let listener = bind_test_listener(&endpoint);
        let server = tokio::spawn(serve_eof_then_fresh_response(
            listener,
            RemoteResponse::ok("search dispatched".to_owned()),
        ));
        let command = RemoteCommand::RunSearch {
            ticket: 7,
            query: "query".to_owned(),
            source: crate::search_source::SearchSource::Youtube,
        };

        let response = send_to_instance(legacy_v8_instance(endpoint.clone()), command)
            .await
            .unwrap();
        let request_ids = server.await.unwrap();
        let _ = std::fs::remove_file(endpoint);

        assert!(response.ok);
        assert_eq!(response.message.as_deref(), Some("search dispatched"));
        assert_eq!(request_ids[0], request_ids[1]);
    }

    #[tokio::test]
    async fn run_search_fresh_retry_rejection_preserves_confirmation_lost() {
        let endpoint = test_endpoint("search-fresh-retry-rejected");
        let listener = bind_test_listener(&endpoint);
        let server = tokio::spawn(serve_eof_then_fresh_response(
            listener,
            RemoteResponse::err("bad_token"),
        ));
        let command = RemoteCommand::RunSearch {
            ticket: 8,
            query: "query".to_owned(),
            source: crate::search_source::SearchSource::Youtube,
        };

        let err = send_to_instance(legacy_v8_instance(endpoint.clone()), command)
            .await
            .unwrap_err();
        let request_ids = server.await.unwrap();
        let _ = std::fs::remove_file(endpoint);

        assert_eq!(err, ClientError::ConfirmationLost);
        assert_eq!(request_ids[0], request_ids[1]);
    }

    #[tokio::test]
    async fn status_accepts_a_fresh_retry_rejection_without_confirmation_semantics() {
        let endpoint = test_endpoint("status-fresh-retry-rejected");
        let listener = bind_test_listener(&endpoint);
        let server = tokio::spawn(serve_eof_then_fresh_response(
            listener,
            RemoteResponse::err("shutting_down"),
        ));

        let response =
            send_to_instance(legacy_v8_instance(endpoint.clone()), RemoteCommand::Status)
                .await
                .unwrap();
        let request_ids = server.await.unwrap();
        let _ = std::fs::remove_file(endpoint);

        assert!(!response.ok);
        assert_eq!(response.reason.as_deref(), Some("shutting_down"));
        assert_eq!(request_ids[0], request_ids[1]);
    }

    #[tokio::test]
    async fn malformed_response_retries_once_and_cached_result_executes_only_once() {
        let endpoint = test_endpoint("malformed-cached");
        let listener = bind_test_listener(&endpoint);
        let server = tokio::spawn(serve_corrupt_then_cached_then_fresh(listener));

        let instance = test_instance(endpoint.clone());
        let recovered = send_to_instance(instance.clone(), RemoteCommand::TogglePause)
            .await
            .unwrap();
        let later = send_to_instance(instance, RemoteCommand::TogglePause)
            .await
            .unwrap();
        let (request_ids, executions) = server.await.unwrap();
        let _ = std::fs::remove_file(endpoint);

        assert!(recovered.ok);
        assert_eq!(recovered.message.as_deref(), Some("execution 1"));
        assert!(later.ok);
        assert_eq!(later.message.as_deref(), Some("execution 2"));
        assert_eq!(executions, 2, "the same-ID retry must not re-execute");
        assert!(request_ids[0].is_some());
        assert_eq!(request_ids[0], request_ids[1]);
        assert_ne!(request_ids[1], request_ids[2]);
    }

    #[tokio::test]
    async fn repeated_malformed_response_stops_after_one_retry() {
        let endpoint = test_endpoint("malformed-twice");
        let listener = bind_test_listener(&endpoint);
        let server = tokio::spawn(serve_two_malformed_responses(listener));

        let err = send_to(test_instance(endpoint.clone()), RemoteCommand::Status)
            .await
            .unwrap_err();
        let request_ids = server.await.unwrap();
        let _ = std::fs::remove_file(endpoint);

        assert_eq!(err, ClientError::MalformedResponse);
        assert!(request_ids[0].is_some());
        assert_eq!(request_ids[0], request_ids[1]);
    }

    #[tokio::test]
    async fn repeated_lost_mutation_replies_surface_confirmation_lost() {
        let endpoint = test_endpoint("mutation-confirmation-lost");
        let listener = bind_test_listener(&endpoint);
        let server = tokio::spawn(serve_two_malformed_responses(listener));

        let err = send_to_instance(test_instance(endpoint.clone()), RemoteCommand::TogglePause)
            .await
            .unwrap_err();
        let request_ids = server.await.unwrap();
        let _ = std::fs::remove_file(endpoint);

        assert_eq!(err, ClientError::ConfirmationLost);
        assert_eq!(request_ids[0], request_ids[1]);
        assert!(
            err.human_message().contains("may have been applied"),
            "the CLI must tell users to inspect state before retrying"
        );
    }

    #[tokio::test]
    async fn request_write_is_deadline_bounded_for_a_non_reading_peer() {
        let (mut writer, _non_reading_peer) = tokio::io::duplex(1);

        let error = write_request(
            &mut writer,
            &vec![b'x'; 64 * 1024],
            Duration::from_millis(5),
        )
        .await
        .unwrap_err();

        assert_eq!(
            error,
            ClientError::WriteFailed("remote request write timed out".to_string())
        );
    }

    #[test]
    fn only_possibly_delivered_or_post_flush_failures_are_ambiguous() {
        assert!(attempt_is_ambiguous(&Err(ClientError::WriteFailed(
            "partial".to_string()
        ))));
        assert!(attempt_is_ambiguous(&Err(ClientError::FlushFailed(
            "lost ack".to_string()
        ))));
        assert!(attempt_is_ambiguous(&Err(ClientError::NoResponse)));
        assert!(attempt_is_ambiguous(&Err(ClientError::MalformedResponse)));
        assert!(attempt_is_ambiguous(&Ok(RemoteResponseEnvelope {
            response: RemoteResponse::err(CONFIRMATION_LOST_REASON),
            retained_replay: false,
        })));
        assert!(!attempt_is_ambiguous(&Err(ClientError::ConnectFailed)));
        assert!(!attempt_is_ambiguous(&Err(ClientError::MalformedEndpoint)));
        assert!(!attempt_is_ambiguous(&Err(ClientError::EncodeFailed(
            "before write".to_string()
        ))));
    }

    #[test]
    fn only_explicit_retained_replay_proves_a_retry_outcome() {
        let plain_success = Ok(RemoteResponseEnvelope {
            response: RemoteResponse::ok("fresh owner response".to_owned()),
            retained_replay: false,
        });
        assert!(!attempt_is_retained_replay(&plain_success));

        let replayed_success = Ok(RemoteResponseEnvelope {
            response: RemoteResponse::ok("retained owner response".to_owned()),
            retained_replay: true,
        });
        assert!(attempt_is_retained_replay(&replayed_success));

        let replay_wait_timed_out = Ok(RemoteResponseEnvelope {
            response: RemoteResponse::err(CONFIRMATION_LOST_REASON),
            retained_replay: true,
        });
        assert!(!attempt_is_retained_replay(&replay_wait_timed_out));
    }

    #[tokio::test]
    async fn send_to_instance_preserves_no_response() {
        let endpoint = test_endpoint("no-response");
        let listener = bind_test_listener(&endpoint);
        // Status retries once after an ambiguous EOF. Keep the listener alive for both
        // exchanges so the second attempt observes the same semantic no-response outcome
        // instead of replacing it with a connect failure after the fixture exits.
        let server = tokio::spawn(accept_requests_without_response(listener, 2));

        let err = send_to(test_instance(endpoint.clone()), RemoteCommand::Status)
            .await
            .unwrap_err();
        server.await.unwrap();
        let _ = std::fs::remove_file(endpoint);

        assert_eq!(err, ClientError::NoResponse);
    }

    #[tokio::test]
    async fn send_to_instance_rejects_a_malformed_endpoint_before_connecting() {
        let err = send_to(
            test_instance("invalid\0endpoint".to_string()),
            RemoteCommand::Status,
        )
        .await
        .unwrap_err();

        assert_eq!(err, ClientError::MalformedEndpoint);
    }

    #[tokio::test]
    async fn primary_probe_rejects_a_live_socket_without_a_descriptor() {
        let endpoint = test_endpoint("owner-without-descriptor");
        let listener = bind_test_listener(&endpoint);

        let error = probe_primary_endpoints(std::slice::from_ref(&endpoint))
            .await
            .unwrap_err();

        assert_eq!(error, CapabilityLookupError::UnpublishedOwner);
        drop(listener);
        let _ = std::fs::remove_file(endpoint);
    }

    #[tokio::test]
    async fn primary_probe_accepts_only_a_definitely_absent_socket() {
        let endpoint = test_endpoint("absent-owner");
        let _ = std::fs::remove_file(&endpoint);

        probe_primary_endpoints(std::slice::from_ref(&endpoint))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn primary_probe_accepts_an_unbindable_overlong_filesystem_path() {
        let endpoint = std::env::temp_dir()
            .join("x".repeat(512))
            .to_string_lossy()
            .into_owned();

        probe_primary_endpoints(std::slice::from_ref(&endpoint))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn primary_probe_fails_closed_for_an_invalid_endpoint() {
        let endpoint = "invalid\0endpoint".to_string();

        let error = probe_primary_endpoints(std::slice::from_ref(&endpoint))
            .await
            .unwrap_err();

        assert_eq!(error, CapabilityLookupError::OwnerProbeFailed);
    }

    #[test]
    fn capability_gate_distinguishes_old_and_capable_instances() {
        let mut old = test_instance("unused".to_string());
        let error =
            require_capability(old.clone(), super::super::PERSONAL_EXPORT_CAPABILITY).unwrap_err();
        assert_eq!(
            error,
            MissingCapability {
                capability: super::super::PERSONAL_EXPORT_CAPABILITY.to_string()
            }
        );

        old.capabilities
            .push(super::super::PERSONAL_EXPORT_CAPABILITY.to_string());
        assert!(require_capability(old, super::super::PERSONAL_EXPORT_CAPABILITY).is_ok());
    }

    #[tokio::test]
    async fn send_to_instance_uses_v7_for_v7_owner() {
        let endpoint = test_endpoint("v7");
        let listener = bind_test_listener(&endpoint);
        let response = serde_json::to_string(&RemoteResponse::ok("ok".to_string())).unwrap();
        let server = tokio::spawn(serve_one_response(listener, response, PROTOCOL_VERSION_V7));
        let mut instance = test_instance(endpoint.clone());
        instance.protocol_version = PROTOCOL_VERSION_V7;

        let response = send_to(instance, RemoteCommand::Next).await.unwrap();
        server.await.unwrap();
        let _ = std::fs::remove_file(endpoint);

        assert!(response.ok);
    }

    #[test]
    fn one_shot_version_negotiates_v7_v8_and_future() {
        assert_eq!(
            negotiated_oneshot_version(PROTOCOL_VERSION_V7),
            Ok(PROTOCOL_VERSION_V7)
        );
        assert_eq!(
            negotiated_oneshot_version(PROTOCOL_VERSION),
            Ok(PROTOCOL_VERSION)
        );
        assert_eq!(negotiated_oneshot_version(u8::MAX), Ok(PROTOCOL_VERSION));
        assert_eq!(
            negotiated_oneshot_version(PROTOCOL_VERSION_V7 - 1),
            Err(ClientError::UnsupportedProtocol)
        );
    }

    fn snapshot(queue: Vec<crate::remote::proto::QueueItemSnapshot>) -> StatusSnapshot {
        StatusSnapshot {
            title: None,
            artist: None,
            paused: true,
            volume: 50,
            position: usize::from(!queue.is_empty()),
            total: queue.len(),
            streaming: false,
            owner_mode: InstanceMode::Daemon,
            settings: crate::remote::proto::SettingsSnapshot::default(),
            queue,
            shuffle: false,
            repeat: Default::default(),
            elapsed_ms: None,
            duration_ms: None,
            is_live: false,
            queue_rev: None,
            track_id: None,
            position_epoch: 0,
            artwork: None,
        }
    }

    #[test]
    fn queue_formatter_is_one_based_marks_current_and_sanitizes_controls() {
        let status = snapshot(vec![
            crate::remote::proto::QueueItemSnapshot {
                title: "첫 곡\n\u{1b}[31m".to_string(),
                artist: "가수\r이름".to_string(),
                duration: "3:14\t".to_string(),
                current: true,
            },
            crate::remote::proto::QueueItemSnapshot {
                title: "Second".to_string(),
                artist: String::new(),
                duration: String::new(),
                current: false,
            },
        ]);

        let line = queue_human(&status);
        assert_eq!(line, "> 1. 첫 곡 [31m — 가수 이름  [3:14]\n  2. Second");
        assert!(!line.contains('\u{1b}'));
        assert_eq!(line.lines().count(), 2);
    }

    #[test]
    fn queue_formatter_has_stable_empty_message() {
        assert_eq!(queue_human(&snapshot(Vec::new())), "queue empty");
    }

    #[test]
    fn settings_formatter_includes_only_summary_fields() {
        let mut status = snapshot(Vec::new());
        status.settings = crate::remote::proto::SettingsSnapshot {
            autoplay_streaming: true,
            streaming_mode: StreamingMode::Discovery,
            streaming_source: SearchSource::InternetArchive,
            speed_tenths: 15,
            seek_seconds: 12,
            normalize: true,
            gapless: false,
            ai_enabled: true,
            radio_mode: false,
        };

        assert_eq!(
            settings_human(&status),
            "autoplay=on  •  source=internet_archive  •  mode=discovery  •  speed=1.5x  •  seek=12s  •  normalize=on  •  gapless=off  •  ai=on  •  radio-mode=off"
        );
    }

    #[test]
    fn queue_and_settings_projection_json_retains_full_remote_response() {
        let mut status = snapshot(vec![crate::remote::proto::QueueItemSnapshot {
            title: "곡".to_string(),
            artist: "가수".to_string(),
            duration: "2:34".to_string(),
            current: true,
        }]);
        status.settings.autoplay_streaming = true;
        let response = RemoteResponse::status(status.clone());

        // Both projection branches pass this whole response to `print_json`; they do not
        // serialize only the selected human projection.
        let value = serde_json::to_value(&response).unwrap();
        assert_eq!(value["ok"], true);
        assert_eq!(value["reason"], "ok");
        assert!(value.get("message").is_some());
        assert_eq!(value["status"]["queue"][0]["title"], "곡");
        assert_eq!(value["status"]["settings"]["autoplay_streaming"], true);
        assert_eq!(response.status, Some(status));
    }

    #[test]
    fn info_output_is_sorted_and_structurally_omits_credentials() {
        let mut instance = test_instance("/private/secret-endpoint".to_string());
        instance.token = "super-secret-token".to_string();
        instance.created_unix = 99;
        instance.mode = InstanceMode::Daemon;
        instance.capabilities = vec![
            "status".to_string(),
            "events-v8".to_string(),
            "remote-control".to_string(),
            "socket=/PRIVATE/SECRET-ENDPOINT".to_string(),
            "token=SUPER-SECRET-TOKEN".to_string(),
        ];

        let info = SanitizedInfo::from(instance);
        let value = serde_json::to_value(&info).unwrap();
        let object = value.as_object().unwrap();
        assert_eq!(object.len(), 5);
        assert_eq!(
            object["capabilities"],
            serde_json::json!(["events-v8", "remote-control", "status"])
        );
        let line = serde_json::to_string(&info).unwrap();
        let human = info.human_line();
        assert!(!line.contains("secret-endpoint"));
        assert!(!line.contains("super-secret-token"));
        assert!(!human.to_ascii_lowercase().contains("secret-endpoint"));
        assert!(!human.to_ascii_lowercase().contains("super-secret-token"));
        assert!(human.contains("events-v8,remote-control,status"));
        assert!(!object.contains_key("endpoint"));
        assert!(!object.contains_key("token"));
    }

    #[test]
    fn info_rejection_never_reflects_owner_text() {
        let response = RemoteResponse {
            ok: false,
            reason: Some(
                "0123456789abcdef0123456789abcdef\n\u{1b}[31m/private/secret-endpoint".to_string(),
            ),
            message: Some("another owner-controlled field".to_string()),
            status: None,
        };

        let message = info_rejection_message(&response);
        assert_eq!(message, "info_status_rejected");
        assert!(!message.contains("0123456789abcdef0123456789abcdef"));
        assert!(!message.contains("secret-endpoint"));
        assert!(!message.chars().any(char::is_control));
    }

    #[test]
    fn info_human_line_sanitizes_capability_controls() {
        let info = SanitizedInfo {
            app_pid: 7,
            created_unix: 1,
            mode: InstanceMode::StandaloneTui,
            protocol_version: PROTOCOL_VERSION,
            capabilities: vec!["events\n\u{1b}[31m-v8".to_string()],
        };
        let line = info.human_line();
        assert!(!line.contains('\n'));
        assert!(!line.contains('\u{1b}'));
        assert!(!line.contains("created_unix"));
        assert!(line.contains("pid 7"));
        assert!(line.contains("mode standalone_tui"));
        assert!(line.contains("protocol 8"));
        assert!(line.contains("events [31m-v8"));
    }
}

async fn read_bounded_line<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> std::io::Result<Option<String>> {
    let mut buf = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    loop {
        let n = reader.read(&mut byte).await?;
        if n == 0 {
            return if buf.is_empty() {
                Ok(None)
            } else {
                Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
            };
        }
        if buf.len() >= MAX_ONESHOT_REPLY_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "remote response too large",
            ));
        }
        buf.push(byte[0]);
        if byte[0] == b'\n' {
            return Ok(Some(String::from_utf8_lossy(&buf).into_owned()));
        }
    }
}
