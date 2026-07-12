//! The one-shot command surface (v7-frozen) shared by `ytt -r`, the tray, and sessions.
//!
//! Byte shapes here are frozen: additive variants/fields only, guarded by the golden
//! corpus in [`super::freeze`].

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::search_source::SearchSource;
use crate::streaming::StreamingMode;

use super::ToggleState;

/// Semantic cap on remote search strings. Frame caps bound bytes on the wire; this caps the
/// amount of search/provider work a syntactically valid command can request.
pub const REMOTE_MAX_QUERY_BYTES: usize = crate::util::query::MAX_SEARCH_QUERY_BYTES;
/// Track ids in GUI commands are rows from prior search/library snapshots. Match the queue cap.
pub const REMOTE_MAX_TRACK_IDS: usize = 999;
/// A single row id should be tiny; this covers YouTube IDs and source-prefixed provider IDs.
pub const REMOTE_MAX_TRACK_ID_BYTES: usize = crate::api::GUI_SEARCH_ROW_ID_MAX_BYTES;
/// Gemini keys are normally well below this; reject large pasted blobs at the protocol edge.
pub const REMOTE_MAX_GEMINI_KEY_BYTES: usize = 256;
/// GUI setting group/field identifiers are short ASCII tokens.
pub const REMOTE_MAX_SETTING_NAME_BYTES: usize = 64;
/// Paths and provider app ids may be user-entered, but should never be frame-sized blobs.
pub const REMOTE_MAX_SETTING_STRING_BYTES: usize = 4096;
/// Export destinations travel inside the 4 KiB one-shot request frame. Keep enough headroom
/// for the request envelope, authentication token, and worst-case JSON escaping while supporting
/// long platform paths.
pub const REMOTE_MAX_EXPORT_DIRECTORY_BYTES: usize = 1536;
/// Session subscribe/unsubscribe frames should name each topic at most once.
pub const REMOTE_MAX_TOPICS: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoteCommandValidationError {
    reason: &'static str,
}

/// Whether a repeated stable request identity joins one retained owner outcome or safely starts a
/// fresh read-only/query execution. The exhaustive classifier makes adding a command an explicit
/// retry-semantics decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RequestRetryClass {
    RetainedOutcome,
    ReexecuteReadOnly,
}

impl RemoteCommandValidationError {
    pub fn reason(self) -> &'static str {
        self.reason
    }
}

/// A semantic player command. Applied through the same reducer path a keypress uses, so
/// it works regardless of the TUI's current input mode (Search text entry, Settings, …).
///
/// `Eq` is deliberately absent: [`GuiSettingChange`] carries a free-form JSON value
/// (floats included), which only supports `PartialEq`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum RemoteCommand {
    Next,
    Prev,
    TogglePause,
    Play {
        query: String,
    },
    Enqueue {
        query: String,
    },
    VolumeUp,
    VolumeDown,
    /// Set the output volume to an absolute percent (`0..=100`). Additive since v7:
    /// an older server rejects it as `bad_request` instead of misbehaving.
    SetVolume {
        percent: i64,
    },
    SeekBack,
    SeekForward,
    /// Absolute seek within the current track, in milliseconds. Additive since v7.
    SeekTo {
        ms: u64,
    },
    ToggleShuffle,
    CycleRepeat,
    QueuePlay {
        position: usize,
    },
    QueueRemove {
        position: usize,
    },
    /// Play an order position only if it still belongs to the queue snapshot the
    /// caller rendered. Additive in v8; stale snapshots are rejected as `stale_rev`.
    QueuePlayIfRevision {
        position: usize,
        expected_rev: u64,
    },
    /// Remove an order position only if it still belongs to the queue snapshot the
    /// caller rendered. Additive in v8; stale snapshots are rejected as `stale_rev`.
    QueueRemoveIfRevision {
        position: usize,
        expected_rev: u64,
    },
    #[serde(alias = "radio")]
    Streaming {
        state: ToggleState,
    },
    SetSetting {
        change: RemoteSettingChange,
    },
    ResumeSession,
    Status,
    Quit,
    /// GUI search (additive, v8 sessions): run a grouped multi-catalog search and push
    /// the outcome on the `search` topic as
    /// [`PushEvent::SearchCompleted`](super::PushEvent::SearchCompleted), keyed by
    /// `ticket`. Fire-and-forget: the reply only acknowledges the dispatch. That page/session
    /// acknowledgement is not cached; a same-ID retry deliberately runs a fresh provider query.
    RunSearch {
        ticket: u64,
        query: String,
        source: SearchSource,
    },
    /// Play these exact rows now: first replaces the current track, the rest queue up
    /// next. Ids come from rows a prior search/library push handed the client.
    PlayTracks {
        video_ids: Vec<String>,
    },
    /// Append these exact rows to the queue (honoring the enqueue-next setting).
    EnqueueTracks {
        video_ids: Vec<String>,
    },
    /// GUI settings mutation (v8 sessions): one `group.field = value` edit. Every
    /// accepted apply is followed by a `settings_snapshot` push carrying the new state
    /// (the GUI's optimistic pending overlay clears against it).
    Apply {
        change: GuiSettingChange,
    },
    /// Store (or clear, when empty) the Gemini API key. Write-only: snapshots carry
    /// only `has_gemini_key`.
    SetGeminiKey {
        key: String,
    },
    /// Danger zone: reset the whole config to defaults (the GUI double-confirms).
    ResetAllSettings,
    /// Write a portable, credential-free snapshot to this existing absolute directory.
    /// Additive since v8 and capability-gated by `personal-export-v1`.
    ExportPersonalData {
        directory: String,
    },
    // ── Deferred v8 GUI commands (additive; capability-gated by `v8-commands`) ─────────
    //
    // Wire shapes are pinned to what the GUI's stores already send (the demo core in
    // gui/src/lib/dev/democore.ts is the reference implementation; gui/WIRING.md §1.5).
    // Owners implement them stream-by-stream; until an owner dispatches a variant it
    // answers `not_supported` (daemon) / `daemon_required` (TUI App).
    /// Cycle or set the current rating of a track by id (favorite/dislike synthesis).
    Rate {
        video_id: String,
        rating: RateChange,
    },
    /// Move an order position to another (queue drag-reorder). `expected_rev` guards
    /// against a stale queue snapshot like the *_if_revision commands.
    QueueMove {
        from: usize,
        to: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_rev: Option<u64>,
    },
    /// Remove several order positions atomically (multi-select remove).
    QueueRemoveMany {
        positions: Vec<usize>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_rev: Option<u64>,
    },
    /// Drop everything after the current track.
    QueueClearUpcoming {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_rev: Option<u64>,
    },
    /// Spawn the mpv video overlay for a track (core-host side effect).
    PlayVideo {
        video_id: String,
    },
    /// DJ Gem chat: fire-and-forget like `RunSearch`; the transcript rides the `ai`
    /// topic, keyed by `ticket`.
    AskAi {
        ticket: u64,
        prompt: String,
    },
    /// Replace the queue with a library scope's (filtered) tracks and play.
    LibraryPlay {
        scope: String,
        #[serde(default)]
        filter: String,
    },
    /// Append a library scope's (filtered) tracks to the queue.
    LibraryEnqueue {
        scope: String,
        #[serde(default)]
        filter: String,
    },
    /// Remove one track from a library scope (favorites/history/…).
    LibraryRemove {
        scope: String,
        video_id: String,
    },
    /// Page through a library scope; the reply's data lane carries the page model.
    FetchLibraryPage {
        scope: String,
        #[serde(default)]
        filter: String,
        #[serde(default)]
        offset: usize,
        limit: usize,
    },
    /// Start a managed yt-dlp download for a track.
    Download {
        video_id: String,
        #[serde(default)]
        title: String,
    },
    /// Remove a download entry, optionally deleting the on-disk file.
    DeleteDownload {
        video_id: String,
        #[serde(default)]
        delete_file: bool,
    },
    /// Bind a chord; the reply's data lane carries core-side conflict/shadow info.
    KeymapBind {
        context: String,
        action: String,
        chord: String,
    },
    KeymapUnbind {
        context: String,
        action: String,
    },
    KeymapResetAll,
    /// Override one theme role with a hex color.
    ThemeSetOverride {
        role: String,
        hex: String,
    },
    ThemeClearOverride {
        role: String,
    },
    /// Drop every cached romanization; the reply's data lane carries `{ cleared }`.
    ClearRomanizationCache,
    PlaylistCreate {
        name: String,
    },
    PlaylistDelete {
        playlist_id: String,
    },
    PlaylistAddTracks {
        playlist_id: String,
        video_ids: Vec<String>,
    },
    PlaylistRemoveTrack {
        playlist_id: String,
        video_id: String,
    },
    PlaylistPlay {
        playlist_id: String,
    },
    /// Pull one playlist's tracks; the reply's data lane carries the detail model.
    FetchPlaylistDetail {
        playlist_id: String,
    },
    /// Which DJ Gem provenance is known for a track; data lane carries it (or nothing).
    FetchWhyGem {
        video_id: String,
    },
    /// List the connected Spotify account's playlists; results ride the `transfer` topic.
    TransferListSpotify,
    /// Start a Spotify import job. The spec is validated by the transfer engine; results
    /// and progress ride the `transfer` topic. Typed model lands with the B4 stream.
    TransferStart {
        spec: Value,
    },
    TransferCancel,
    /// Begin the Last.fm browser auth flow; the auth URL rides the `accounts` topic.
    LastfmConnect,
    /// Begin the Spotify browser auth flow; the auth URL rides the `accounts` topic.
    SpotifyConnect,
    /// Configure ListenBrainz submission (token is write-only).
    ListenBrainzConfigure {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        submit: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        custom_url: Option<String>,
    },
    /// Uniform account-block field setter (scrobbling toggles, love-sync, …).
    AccountSet {
        service: String,
        field: String,
        value: Value,
    },
}

/// One rating step for [`RemoteCommand::Rate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateChange {
    Up,
    Down,
    Clear,
    /// The TUI's 👍/–/👎 cycle; the GUI's rating chip sends this today.
    Cycle,
}

impl RemoteCommand {
    pub(crate) fn expected_queue_rev(&self) -> Option<u64> {
        match self {
            Self::QueuePlayIfRevision { expected_rev, .. }
            | Self::QueueRemoveIfRevision { expected_rev, .. } => Some(*expected_rev),
            // Optional guards: absent means the caller opted out of the stale check
            // (the keyboard path sends no revision; the drag path always does).
            Self::QueueMove { expected_rev, .. }
            | Self::QueueRemoveMany { expected_rev, .. }
            | Self::QueueClearUpcoming { expected_rev } => *expected_rev,
            _ => None,
        }
    }

    pub(crate) fn request_retry_class(&self) -> RequestRetryClass {
        match self {
            // Pure reads (paging, drill-downs, provenance) re-execute freshly on a
            // same-ID retry — replaying a retained page would pin stale data.
            RemoteCommand::Status
            | RemoteCommand::RunSearch { .. }
            | RemoteCommand::FetchLibraryPage { .. }
            | RemoteCommand::FetchPlaylistDetail { .. }
            | RemoteCommand::FetchWhyGem { .. } => RequestRetryClass::ReexecuteReadOnly,
            RemoteCommand::Next
            | RemoteCommand::Prev
            | RemoteCommand::TogglePause
            | RemoteCommand::Play { .. }
            | RemoteCommand::Enqueue { .. }
            | RemoteCommand::VolumeUp
            | RemoteCommand::VolumeDown
            | RemoteCommand::SetVolume { .. }
            | RemoteCommand::SeekBack
            | RemoteCommand::SeekForward
            | RemoteCommand::SeekTo { .. }
            | RemoteCommand::ToggleShuffle
            | RemoteCommand::CycleRepeat
            | RemoteCommand::QueuePlay { .. }
            | RemoteCommand::QueueRemove { .. }
            | RemoteCommand::QueuePlayIfRevision { .. }
            | RemoteCommand::QueueRemoveIfRevision { .. }
            | RemoteCommand::Streaming { .. }
            | RemoteCommand::SetSetting { .. }
            | RemoteCommand::ResumeSession
            | RemoteCommand::Quit
            | RemoteCommand::PlayTracks { .. }
            | RemoteCommand::EnqueueTracks { .. }
            | RemoteCommand::Apply { .. }
            | RemoteCommand::SetGeminiKey { .. }
            | RemoteCommand::ResetAllSettings
            | RemoteCommand::ExportPersonalData { .. }
            | RemoteCommand::Rate { .. }
            | RemoteCommand::QueueMove { .. }
            | RemoteCommand::QueueRemoveMany { .. }
            | RemoteCommand::QueueClearUpcoming { .. }
            | RemoteCommand::PlayVideo { .. }
            | RemoteCommand::AskAi { .. }
            | RemoteCommand::LibraryPlay { .. }
            | RemoteCommand::LibraryEnqueue { .. }
            | RemoteCommand::LibraryRemove { .. }
            | RemoteCommand::Download { .. }
            | RemoteCommand::DeleteDownload { .. }
            | RemoteCommand::KeymapBind { .. }
            | RemoteCommand::KeymapUnbind { .. }
            | RemoteCommand::KeymapResetAll
            | RemoteCommand::ThemeSetOverride { .. }
            | RemoteCommand::ThemeClearOverride { .. }
            | RemoteCommand::ClearRomanizationCache
            | RemoteCommand::PlaylistCreate { .. }
            | RemoteCommand::PlaylistDelete { .. }
            | RemoteCommand::PlaylistAddTracks { .. }
            | RemoteCommand::PlaylistRemoveTrack { .. }
            | RemoteCommand::PlaylistPlay { .. }
            | RemoteCommand::TransferListSpotify
            | RemoteCommand::TransferStart { .. }
            | RemoteCommand::TransferCancel
            | RemoteCommand::LastfmConnect
            | RemoteCommand::SpotifyConnect
            | RemoteCommand::ListenBrainzConfigure { .. }
            | RemoteCommand::AccountSet { .. } => RequestRetryClass::RetainedOutcome,
        }
    }

    /// Whether losing the reply can leave the caller unsure whether observable state changed.
    /// `RunSearch` is included: its acknowledgement confirms dispatch of the later push.
    /// Pure reads are excluded so a lost fetch reply surfaces as `timeout`, never as the
    /// alarming `confirmation_lost`.
    pub(crate) fn requires_confirmation(&self) -> bool {
        !matches!(
            self,
            RemoteCommand::Status
                | RemoteCommand::FetchLibraryPage { .. }
                | RemoteCommand::FetchPlaylistDetail { .. }
                | RemoteCommand::FetchWhyGem { .. }
        )
    }

    pub fn validate(&self) -> Result<(), RemoteCommandValidationError> {
        match self {
            RemoteCommand::SetVolume { percent } if !(0..=100).contains(percent) => {
                Err(validation_error("bad_volume"))
            }
            RemoteCommand::QueuePlay { position }
            | RemoteCommand::QueueRemove { position }
            | RemoteCommand::QueuePlayIfRevision { position, .. }
            | RemoteCommand::QueueRemoveIfRevision { position, .. }
                if *position >= REMOTE_MAX_TRACK_IDS =>
            {
                Err(validation_error("bad_queue_position"))
            }
            RemoteCommand::SetSetting {
                change: RemoteSettingChange::Speed { tenths },
            } if !(5..=20).contains(tenths) => Err(validation_error("bad_speed")),
            RemoteCommand::SetSetting {
                change: RemoteSettingChange::SeekSeconds { seconds },
            } if !(1..=60).contains(seconds) => Err(validation_error("bad_seek_seconds")),
            RemoteCommand::Play { query }
            | RemoteCommand::Enqueue { query }
            | RemoteCommand::RunSearch { query, .. } => validate_query(query),
            RemoteCommand::PlayTracks { video_ids }
            | RemoteCommand::EnqueueTracks { video_ids } => validate_track_ids(video_ids),
            RemoteCommand::Apply { change } => validate_gui_setting_change(change),
            RemoteCommand::SetGeminiKey { key } => validate_gemini_key(key),
            RemoteCommand::ExportPersonalData { directory } => validate_export_directory(directory),
            RemoteCommand::QueueMove { from, to, .. }
                if *from >= REMOTE_MAX_TRACK_IDS || *to >= REMOTE_MAX_TRACK_IDS =>
            {
                Err(validation_error("bad_queue_position"))
            }
            RemoteCommand::QueueRemoveMany { positions, .. } => {
                if positions.is_empty() {
                    return Err(validation_error("empty_selection"));
                }
                if positions.len() > REMOTE_MAX_TRACK_IDS
                    || positions.iter().any(|p| *p >= REMOTE_MAX_TRACK_IDS)
                {
                    return Err(validation_error("bad_queue_position"));
                }
                Ok(())
            }
            RemoteCommand::Rate { video_id, .. }
            | RemoteCommand::PlayVideo { video_id }
            | RemoteCommand::LibraryRemove { video_id, .. }
            | RemoteCommand::Download { video_id, .. }
            | RemoteCommand::DeleteDownload { video_id, .. }
            | RemoteCommand::PlaylistRemoveTrack { video_id, .. }
            | RemoteCommand::FetchWhyGem { video_id } => validate_wire_id(video_id),
            RemoteCommand::AskAi { prompt, .. } => validate_query(prompt),
            RemoteCommand::LibraryPlay { scope, filter }
            | RemoteCommand::LibraryEnqueue { scope, filter } => {
                validate_scope_and_filter(scope, filter)
            }
            RemoteCommand::FetchLibraryPage {
                scope,
                filter,
                limit,
                ..
            } => {
                if !(1..=REMOTE_MAX_PAGE_LIMIT).contains(limit) {
                    return Err(validation_error("bad_page_limit"));
                }
                validate_scope_and_filter(scope, filter)
            }
            RemoteCommand::KeymapBind {
                context,
                action,
                chord,
            } => {
                validate_wire_token(context)?;
                validate_wire_token(action)?;
                validate_wire_string(chord)
            }
            RemoteCommand::KeymapUnbind { context, action } => {
                validate_wire_token(context)?;
                validate_wire_token(action)
            }
            RemoteCommand::ThemeSetOverride { role, hex } => {
                validate_wire_token(role)?;
                validate_wire_string(hex)
            }
            RemoteCommand::ThemeClearOverride { role } => validate_wire_token(role),
            RemoteCommand::PlaylistCreate { name } => validate_wire_string_nonempty(name),
            RemoteCommand::PlaylistDelete { playlist_id }
            | RemoteCommand::PlaylistPlay { playlist_id }
            | RemoteCommand::FetchPlaylistDetail { playlist_id } => validate_wire_id(playlist_id),
            RemoteCommand::PlaylistAddTracks {
                playlist_id,
                video_ids,
            } => {
                validate_wire_id(playlist_id)?;
                validate_track_ids(video_ids)
            }
            RemoteCommand::TransferStart { spec } => {
                if !spec.is_object() {
                    return Err(validation_error("bad_request"));
                }
                Ok(())
            }
            RemoteCommand::ListenBrainzConfigure {
                token, custom_url, ..
            } => {
                if let Some(token) = token {
                    validate_wire_string(token)?;
                }
                if let Some(url) = custom_url {
                    validate_wire_string(url)?;
                }
                Ok(())
            }
            RemoteCommand::AccountSet {
                service,
                field,
                value,
            } => {
                validate_wire_token(service)?;
                validate_wire_token(field)?;
                match value {
                    Value::Bool(_) | Value::Null => Ok(()),
                    Value::Number(n) if n.as_f64().is_some_and(f64::is_finite) => Ok(()),
                    Value::String(s) => validate_wire_string(s),
                    _ => Err(validation_error("bad_setting_value")),
                }
            }
            _ => Ok(()),
        }
    }
}

/// Page fetches are viewport-driven; anything past this is a bulk export, not a page.
pub const REMOTE_MAX_PAGE_LIMIT: usize = 500;

fn validate_wire_id(id: &str) -> Result<(), RemoteCommandValidationError> {
    let id = id.trim();
    if id.is_empty()
        || id.len() > REMOTE_MAX_TRACK_ID_BYTES
        || id.chars().any(forbidden_command_char)
    {
        return Err(validation_error("bad_track_id"));
    }
    Ok(())
}

fn validate_wire_token(token: &str) -> Result<(), RemoteCommandValidationError> {
    // Contexts/actions/roles/services are identifier-like but mixed-case (e.g. "Player").
    if token.is_empty()
        || token.len() > REMOTE_MAX_SETTING_NAME_BYTES
        || !token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.')
    {
        return Err(validation_error("bad_request"));
    }
    Ok(())
}

fn validate_wire_string(raw: &str) -> Result<(), RemoteCommandValidationError> {
    validate_setting_string(raw).map_err(|_| validation_error("bad_request"))
}

fn validate_wire_string_nonempty(raw: &str) -> Result<(), RemoteCommandValidationError> {
    if raw.trim().is_empty() {
        return Err(validation_error("bad_request"));
    }
    validate_wire_string(raw)
}

fn validate_scope_and_filter(
    scope: &str,
    filter: &str,
) -> Result<(), RemoteCommandValidationError> {
    validate_wire_token(scope)?;
    if filter.len() > REMOTE_MAX_QUERY_BYTES || filter.chars().any(forbidden_command_char) {
        return Err(validation_error("bad_request"));
    }
    Ok(())
}

/// One GUI settings edit: `group` and `field` name a [`super::SettingsModelV8`] slot;
/// `value` is the raw JSON the frontend sent (validated by the owner per field).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GuiSettingChange {
    pub group: String,
    pub field: String,
    pub value: serde_json::Value,
}

fn validation_error(reason: &'static str) -> RemoteCommandValidationError {
    RemoteCommandValidationError { reason }
}

fn validate_query(query: &str) -> Result<(), RemoteCommandValidationError> {
    let query = query.trim();
    if query.is_empty() {
        return Err(validation_error("empty_query"));
    }
    if query.len() > REMOTE_MAX_QUERY_BYTES {
        return Err(validation_error("query_too_long"));
    }
    if query.chars().any(crate::util::query::forbidden_query_char) {
        return Err(validation_error("bad_request"));
    }
    Ok(())
}

fn validate_track_ids(video_ids: &[String]) -> Result<(), RemoteCommandValidationError> {
    if video_ids.is_empty() {
        return Err(validation_error("empty_selection"));
    }
    if video_ids.len() > REMOTE_MAX_TRACK_IDS {
        return Err(validation_error("too_many_tracks"));
    }
    for id in video_ids {
        let id = id.trim();
        if id.is_empty()
            || id.len() > REMOTE_MAX_TRACK_ID_BYTES
            || id.chars().any(forbidden_command_char)
        {
            return Err(validation_error("bad_track_id"));
        }
    }
    Ok(())
}

fn validate_gemini_key(key: &str) -> Result<(), RemoteCommandValidationError> {
    let key = key.trim();
    if key.len() > REMOTE_MAX_GEMINI_KEY_BYTES {
        return Err(validation_error("key_too_long"));
    }
    if key.chars().any(forbidden_command_char) {
        return Err(validation_error("bad_request"));
    }
    Ok(())
}

fn validate_export_directory(directory: &str) -> Result<(), RemoteCommandValidationError> {
    if directory.is_empty() {
        return Err(validation_error("empty_export_directory"));
    }
    if directory.len() > REMOTE_MAX_EXPORT_DIRECTORY_BYTES {
        return Err(validation_error("export_directory_too_long"));
    }
    if directory.chars().any(forbidden_command_char) {
        return Err(validation_error("bad_export_directory"));
    }
    if !std::path::Path::new(directory).is_absolute() {
        return Err(validation_error("export_directory_not_absolute"));
    }
    Ok(())
}

fn validate_gui_setting_change(
    change: &GuiSettingChange,
) -> Result<(), RemoteCommandValidationError> {
    if !valid_setting_token(&change.group) || !valid_setting_token(&change.field) {
        return Err(validation_error("bad_setting"));
    }
    if !known_setting_group(&change.group) {
        return Err(validation_error("unknown_setting"));
    }
    validate_setting_value(&change.group, &change.field, &change.value)
}

fn known_setting_group(group: &str) -> bool {
    matches!(
        group,
        "playback"
            | "eq"
            | "streaming"
            | "search"
            | "ui"
            | "storage"
            | "audio"
            | "animations"
            | "theme"
    )
}

fn valid_setting_token(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= REMOTE_MAX_SETTING_NAME_BYTES
        && token
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

fn validate_setting_value(
    group: &str,
    field: &str,
    value: &Value,
) -> Result<(), RemoteCommandValidationError> {
    match value {
        Value::Null if nullable_setting(group, field) => Ok(()),
        Value::Null => Err(validation_error("bad_setting_value")),
        Value::Bool(_) => Ok(()),
        Value::Number(n) if n.as_f64().is_some_and(f64::is_finite) => Ok(()),
        Value::Number(_) => Err(validation_error("bad_setting_value")),
        Value::String(s) => validate_setting_string(s),
        Value::Array(values) if group == "eq" && field == "bands" => {
            if values.len() != 10 {
                return Err(validation_error("bad_setting_value"));
            }
            for value in values {
                let Value::Number(n) = value else {
                    return Err(validation_error("bad_setting_value"));
                };
                if !n.as_f64().is_some_and(f64::is_finite) {
                    return Err(validation_error("bad_setting_value"));
                }
            }
            Ok(())
        }
        Value::Array(_) | Value::Object(_) => Err(validation_error("bad_setting_value")),
    }
}

fn nullable_setting(group: &str, field: &str) -> bool {
    matches!(
        (group, field),
        ("search", "audius_app_name")
            | ("search", "jamendo_client_id")
            | ("storage", "download_dir")
            | ("storage", "cookies_file")
            | ("audio", "mpv_output")
            | ("audio", "mpv_device")
    )
}

fn validate_setting_string(raw: &str) -> Result<(), RemoteCommandValidationError> {
    if raw.len() > REMOTE_MAX_SETTING_STRING_BYTES {
        return Err(validation_error("bad_setting_value"));
    }
    if raw.chars().any(forbidden_command_char) {
        return Err(validation_error("bad_setting_value"));
    }
    Ok(())
}

fn forbidden_command_char(ch: char) -> bool {
    ch == '\0' || ch.is_control()
}

/// A single persisted/live setting mutation from companion surfaces such as the tray panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "setting", rename_all = "snake_case")]
pub enum RemoteSettingChange {
    AutoplayStreaming {
        value: bool,
    },
    StreamingMode {
        value: StreamingMode,
    },
    StreamingSource {
        value: SearchSource,
    },
    /// Playback speed in tenths: `10` means `1.0x`, `15` means `1.5x`.
    Speed {
        tenths: u16,
    },
    SeekSeconds {
        seconds: u16,
    },
    Normalize {
        value: bool,
    },
    Gapless {
        value: bool,
    },
    AiEnabled {
        value: bool,
    },
    RadioMode {
        state: ToggleState,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_class_separates_reexecuted_queries_from_retained_mutations() {
        assert_eq!(
            RemoteCommand::Status.request_retry_class(),
            RequestRetryClass::ReexecuteReadOnly
        );
        assert!(!RemoteCommand::Status.requires_confirmation());
        let search = RemoteCommand::RunSearch {
            ticket: 1,
            query: "query".to_string(),
            source: SearchSource::Youtube,
        };
        assert_eq!(
            search.request_retry_class(),
            RequestRetryClass::ReexecuteReadOnly
        );
        assert!(
            search.requires_confirmation(),
            "reexecution policy does not make a lost dispatch acknowledgement definitive"
        );
        assert_eq!(
            RemoteCommand::TogglePause.request_retry_class(),
            RequestRetryClass::RetainedOutcome
        );
        assert!(RemoteCommand::TogglePause.requires_confirmation());
        let export = RemoteCommand::ExportPersonalData {
            directory: std::env::temp_dir().to_string_lossy().into_owned(),
        };
        assert_eq!(
            export.request_retry_class(),
            RequestRetryClass::RetainedOutcome
        );
        assert!(export.requires_confirmation());
    }

    #[test]
    fn command_validation_caps_queries_keys_and_track_lists() {
        assert_eq!(
            RemoteCommand::Play {
                query: String::new()
            }
            .validate()
            .unwrap_err()
            .reason(),
            "empty_query"
        );
        assert_eq!(
            RemoteCommand::RunSearch {
                ticket: 1,
                query: "q".repeat(REMOTE_MAX_QUERY_BYTES + 1),
                source: SearchSource::Youtube,
            }
            .validate()
            .unwrap_err()
            .reason(),
            "query_too_long"
        );
        assert_eq!(
            RemoteCommand::SetGeminiKey {
                key: "k".repeat(REMOTE_MAX_GEMINI_KEY_BYTES + 1)
            }
            .validate()
            .unwrap_err()
            .reason(),
            "key_too_long"
        );
        assert_eq!(
            RemoteCommand::PlayTracks {
                video_ids: vec!["id".to_string(); REMOTE_MAX_TRACK_IDS + 1]
            }
            .validate()
            .unwrap_err()
            .reason(),
            "too_many_tracks"
        );
    }

    #[test]
    fn export_command_round_trips_and_requires_a_bounded_absolute_directory() {
        let directory = std::env::temp_dir().to_string_lossy().into_owned();
        let command = RemoteCommand::ExportPersonalData {
            directory: directory.clone(),
        };
        assert!(command.validate().is_ok());

        let line = serde_json::to_string(&command).unwrap();
        let back: RemoteCommand = serde_json::from_str(&line).unwrap();
        assert_eq!(back, command);
        assert!(line.contains(r#""cmd":"export_personal_data""#));

        for (directory, reason) in [
            (String::new(), "empty_export_directory"),
            ("relative/path".to_string(), "export_directory_not_absolute"),
            ("bad\npath".to_string(), "bad_export_directory"),
            (
                format!("/{}", "x".repeat(REMOTE_MAX_EXPORT_DIRECTORY_BYTES)),
                "export_directory_too_long",
            ),
        ] {
            let error = RemoteCommand::ExportPersonalData { directory }
                .validate()
                .unwrap_err();
            assert_eq!(error.reason(), reason);
        }
    }

    #[test]
    fn desktop_control_ranges_are_rejected_at_the_protocol_edge() {
        for percent in [-1, 101] {
            assert_eq!(
                RemoteCommand::SetVolume { percent }
                    .validate()
                    .unwrap_err()
                    .reason(),
                "bad_volume"
            );
        }
        assert_eq!(
            RemoteCommand::QueueRemove {
                position: REMOTE_MAX_TRACK_IDS,
            }
            .validate()
            .unwrap_err()
            .reason(),
            "bad_queue_position"
        );
        assert_eq!(
            RemoteCommand::SetSetting {
                change: RemoteSettingChange::Speed { tenths: 21 },
            }
            .validate()
            .unwrap_err()
            .reason(),
            "bad_speed"
        );
        assert_eq!(
            RemoteCommand::SetSetting {
                change: RemoteSettingChange::SeekSeconds { seconds: 0 },
            }
            .validate()
            .unwrap_err()
            .reason(),
            "bad_seek_seconds"
        );
        for command in [
            RemoteCommand::SetVolume { percent: 100 },
            RemoteCommand::QueuePlay {
                position: REMOTE_MAX_TRACK_IDS - 1,
            },
            RemoteCommand::SetSetting {
                change: RemoteSettingChange::Speed { tenths: 5 },
            },
            RemoteCommand::SetSetting {
                change: RemoteSettingChange::SeekSeconds { seconds: 60 },
            },
        ] {
            assert!(command.validate().is_ok());
        }
    }

    #[test]
    fn command_validation_rejects_structured_setting_values() {
        let bad = RemoteCommand::Apply {
            change: GuiSettingChange {
                group: "search".to_string(),
                field: "default_source".to_string(),
                value: serde_json::json!({"nested": "object"}),
            },
        };
        assert_eq!(bad.validate().unwrap_err().reason(), "bad_setting_value");

        let good_null = RemoteCommand::Apply {
            change: GuiSettingChange {
                group: "storage".to_string(),
                field: "download_dir".to_string(),
                value: Value::Null,
            },
        };
        assert!(good_null.validate().is_ok());

        let bad_array = RemoteCommand::Apply {
            change: GuiSettingChange {
                group: "eq".to_string(),
                field: "bands".to_string(),
                value: serde_json::json!([0, 0, 0]),
            },
        };
        assert_eq!(
            bad_array.validate().unwrap_err().reason(),
            "bad_setting_value"
        );
    }

    #[test]
    fn command_validation_handles_deterministic_fuzz_corpus() {
        let mut state = 0x243f_6a88_85a3_08d3u64;
        let fields = [
            ("player", "volume"),
            ("streaming", "enabled"),
            ("storage", "download_dir"),
            ("eq", "bands"),
            ("unknown", "field"),
        ];

        for _ in 0..512 {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let query_len = (state as usize) % (REMOTE_MAX_QUERY_BYTES + 64);
            let query = "q".repeat(query_len);
            let cmd = RemoteCommand::RunSearch {
                ticket: state,
                query,
                source: SearchSource::Youtube,
            };
            let result = cmd.validate();
            if query_len == 0 {
                assert_eq!(result.unwrap_err().reason(), "empty_query");
            } else if query_len > REMOTE_MAX_QUERY_BYTES {
                assert_eq!(result.unwrap_err().reason(), "query_too_long");
            } else {
                assert!(result.is_ok());
            }

            let count = (state.rotate_left(7) as usize) % (REMOTE_MAX_TRACK_IDS + 8);
            let ids: Vec<String> = (0..count)
                .map(|idx| {
                    if idx % 13 == 0 {
                        "x".repeat(REMOTE_MAX_TRACK_ID_BYTES + 1)
                    } else {
                        format!("id{idx}")
                    }
                })
                .collect();
            let result = RemoteCommand::PlayTracks { video_ids: ids }.validate();
            if count > REMOTE_MAX_TRACK_IDS {
                assert_eq!(result.unwrap_err().reason(), "too_many_tracks");
            }

            let (group, field) = fields[(state as usize) % fields.len()];
            let value = match state % 6 {
                0 => Value::Bool(state & 1 == 0),
                1 => Value::from((state % 100) as i64),
                2 => Value::from(
                    "x".repeat((state as usize) % (REMOTE_MAX_SETTING_STRING_BYTES + 32)),
                ),
                3 => Value::Null,
                4 => serde_json::json!({"nested": state}),
                _ => serde_json::json!([state, state + 1]),
            };
            let _ = RemoteCommand::Apply {
                change: GuiSettingChange {
                    group: group.to_string(),
                    field: field.to_string(),
                    value,
                },
            }
            .validate();
        }
    }
}
