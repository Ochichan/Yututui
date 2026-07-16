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
    let instance = read_current_instance()?;
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
    let instance = read_current_instance()?;
    send_to(instance, command).await
}

fn read_current_instance() -> Result<InstanceFile, ClientError> {
    endpoint::read_current_instance().map_err(|error| match error {
        endpoint::CurrentInstanceError::NotFound => ClientError::NoRunningInstance,
        error => ClientError::CurrentDescriptor(error),
    })
}

pub async fn send_to(
    instance: InstanceFile,
    command: RemoteCommand,
) -> Result<RemoteResponse, ClientError> {
    let request_id = super::requests::fresh_request_id();
    send_to_instance_with_request_id(instance, command, &request_id).await
}

#[cfg(all(test, unix))]
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
            host_terminal: _,
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
mod tests;

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
