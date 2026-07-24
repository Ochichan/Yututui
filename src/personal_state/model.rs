use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt;

use serde::{Deserialize, Serialize};

pub const PERSONAL_STATE_KIND: &str = "yututui_personal_state";
pub const PERSONAL_STATE_SCHEMA_VERSION: u32 = 2;
pub(crate) const MAX_OPERATIONS: usize = 250_000;
pub(crate) const MAX_DEVICES: usize = 256;
pub(crate) const MAX_TEXT_CHARS: usize = 1_024;
pub(crate) const MAX_TRACK_ID_CHARS: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PersonalStateError {
    UnsupportedKind,
    UnsupportedSchema(u32),
    EmptyIdentifier(&'static str),
    IdentifierTooLong(&'static str),
    InvalidVersionVector,
    DuplicateDot,
    ConflictingOperationId,
    TooManyOperations,
    TooManyDevices,
    InvalidOperation(&'static str),
    Serialization(String),
    Io(String),
    ProjectionMismatch,
}

impl fmt::Display for PersonalStateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedKind => write!(f, "not a YuTuTui personal-state file"),
            Self::UnsupportedSchema(schema) => {
                write!(f, "personal-state schema {schema} is not supported")
            }
            Self::EmptyIdentifier(field) => write!(f, "{field} must not be empty"),
            Self::IdentifierTooLong(field) => write!(f, "{field} is too long"),
            Self::InvalidVersionVector => write!(f, "personal-state version vector is invalid"),
            Self::DuplicateDot => write!(f, "different operations reuse the same causal dot"),
            Self::ConflictingOperationId => {
                write!(f, "different operations reuse the same operation id")
            }
            Self::TooManyOperations => write!(f, "personal-state operation limit exceeded"),
            Self::TooManyDevices => write!(f, "personal-state device limit exceeded"),
            Self::InvalidOperation(reason) => {
                write!(f, "invalid personal-state operation: {reason}")
            }
            Self::Serialization(error) => write!(f, "could not decode personal state: {error}"),
            Self::Io(error) => write!(f, "personal-state storage failed: {error}"),
            Self::ProjectionMismatch => {
                write!(
                    f,
                    "personal-state ledger and runtime projections do not match"
                )
            }
        }
    }
}

impl std::error::Error for PersonalStateError {}

impl From<std::io::Error> for PersonalStateError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value.to_string())
    }
}

impl From<serde_json::Error> for PersonalStateError {
    fn from(value: serde_json::Error) -> Self {
        Self::Serialization(value.to_string())
    }
}

macro_rules! string_id {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, PersonalStateError> {
                let value = value.into();
                validate_id(stringify!($name), &value, MAX_TRACK_ID_CHARS)?;
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }
    };
}

string_id!(DeviceId);
string_id!(PlaylistId);
string_id!(PlaylistEntryId);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Dot {
    pub device_id: DeviceId,
    pub sequence: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct VersionVector(pub BTreeMap<DeviceId, u64>);

impl VersionVector {
    pub fn observed(&self, device: &DeviceId) -> u64 {
        self.0.get(device).copied().unwrap_or(0)
    }

    pub fn observe(&mut self, dot: &Dot) {
        self.0
            .entry(dot.device_id.clone())
            .and_modify(|sequence| *sequence = (*sequence).max(dot.sequence))
            .or_insert(dot.sequence);
    }

    pub fn covers(&self, dot: &Dot) -> bool {
        self.observed(&dot.device_id) >= dot.sequence
    }

    pub fn merge(&mut self, other: &Self) {
        for (device, sequence) in &other.0 {
            self.0
                .entry(device.clone())
                .and_modify(|current| *current = (*current).max(*sequence))
                .or_insert(*sequence);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CausalStamp {
    pub dot: Dot,
    #[serde(default)]
    pub observed: VersionVector,
    #[serde(default)]
    pub recorded_at_unix: i64,
}

impl CausalStamp {
    pub fn happens_after(&self, other: &Self) -> bool {
        self.observed.covers(&other.dot)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Rating {
    #[default]
    Neutral,
    Liked,
    Disliked,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum PortableTrackKey {
    Catalog {
        provider: String,
        exact_catalog_id: String,
    },
    OpenSubsonic {
        backend_id: String,
        account_scope_id: String,
        item_id: String,
    },
    LocalPlaceholder {
        portable_placeholder_id: String,
    },
}

impl PortableTrackKey {
    pub fn validate(&self) -> Result<(), PersonalStateError> {
        match self {
            Self::Catalog {
                provider,
                exact_catalog_id,
            } => {
                validate_id("track provider", provider, 64)?;
                validate_id("catalog track id", exact_catalog_id, MAX_TRACK_ID_CHARS)
            }
            Self::OpenSubsonic {
                backend_id,
                account_scope_id,
                item_id,
            } => {
                validate_id("backend id", backend_id, MAX_TRACK_ID_CHARS)?;
                validate_id("account scope id", account_scope_id, MAX_TRACK_ID_CHARS)?;
                validate_id("server item id", item_id, MAX_TRACK_ID_CHARS)
            }
            Self::LocalPlaceholder {
                portable_placeholder_id,
            } => validate_id(
                "portable placeholder id",
                portable_placeholder_id,
                MAX_TRACK_ID_CHARS,
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortableTrack {
    pub key: PortableTrackKey,
    pub title: String,
    pub artist: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isrc: Option<String>,
}

impl PortableTrack {
    pub fn validate(&self) -> Result<(), PersonalStateError> {
        self.key.validate()?;
        validate_text("track title", &self.title)?;
        validate_text("track artist", &self.artist)?;
        if let Some(album) = &self.album {
            validate_text("track album", album)?;
        }
        if let Some(isrc) = &self.isrc {
            validate_id("ISRC", isrc, 64)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EngagementKind {
    Play,
    QuickSkip,
    Completion,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum OperationOrigin {
    Local,
    WebDav,
    OpenSubsonic { backend_id: String },
    Imported,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum Operation {
    SetRating {
        track: PortableTrack,
        rating: Rating,
    },
    RecordEngagement {
        event_id: String,
        track: PortableTrack,
        engagement: EngagementKind,
        played_duration_ms: Option<u64>,
        total_duration_ms: Option<u64>,
        artist_key: String,
    },
    SetRadioFavorite {
        station: PortableTrack,
        favorite: bool,
    },
    UpsertPlaylist {
        playlist_id: PlaylistId,
        name: String,
    },
    DeletePlaylist {
        playlist_id: PlaylistId,
        deleted: bool,
    },
    UpsertPlaylistEntry {
        playlist_id: PlaylistId,
        entry_id: PlaylistEntryId,
        track: PortableTrack,
        after_entry_id: Option<PlaylistEntryId>,
    },
    MovePlaylistEntry {
        playlist_id: PlaylistId,
        entry_id: PlaylistEntryId,
        after_entry_id: Option<PlaylistEntryId>,
    },
    RemovePlaylistEntry {
        playlist_id: PlaylistId,
        entry_id: PlaylistEntryId,
        removed: bool,
    },
    SetStationProfile {
        query: Option<String>,
        explore: crate::station::Explore,
    },
    SetAvoidArtist {
        artist_key: String,
        avoid: bool,
    },
    BindTrack {
        placeholder: PortableTrackKey,
        target: PortableTrackKey,
    },
    AddDevice {
        device: DeviceRecord,
    },
    RevokeDevice {
        device_id: DeviceId,
    },
    LegacyBaseline {
        baseline: Box<crate::personal_state::LegacyProjection>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OperationEnvelope {
    pub operation_id: String,
    pub stamp: CausalStamp,
    pub origin: OperationOrigin,
    pub operation: Operation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceRecord {
    pub device_id: DeviceId,
    pub name: String,
    #[serde(default)]
    pub revoked: bool,
}

pub type DeviceRegistry = BTreeMap<DeviceId, DeviceRecord>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionCheckpoint {
    pub checkpoint_id: String,
    pub coverage: VersionVector,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_checkpoint_hash: Option<String>,
    #[serde(default)]
    pub acknowledged_by: BTreeSet<DeviceId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersonalStateMetadata {
    pub source_app_version: String,
    #[serde(default)]
    pub contains_listening_history: bool,
    #[serde(default)]
    pub credentials_included: bool,
    #[serde(default)]
    pub filesystem_paths_included: bool,
    #[serde(default)]
    pub playable_urls_included: bool,
}

impl Default for PersonalStateMetadata {
    fn default() -> Self {
        Self {
            source_app_version: env!("CARGO_PKG_VERSION").to_owned(),
            contains_listening_history: true,
            credentials_included: false,
            filesystem_paths_included: false,
            playable_urls_included: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersonalStateV2 {
    pub kind: String,
    pub schema_version: u32,
    pub dataset_id: String,
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    pub device_registry: DeviceRegistry,
    #[serde(default)]
    pub version_vector: VersionVector,
    #[serde(default)]
    pub operations: Vec<OperationEnvelope>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction_checkpoint: Option<CompactionCheckpoint>,
    #[serde(default)]
    pub metadata: PersonalStateMetadata,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub projection_fingerprint: Option<String>,
}

impl Default for PersonalStateV2 {
    fn default() -> Self {
        Self {
            kind: PERSONAL_STATE_KIND.to_owned(),
            schema_version: PERSONAL_STATE_SCHEMA_VERSION,
            dataset_id: "uninitialized".to_owned(),
            revision: 0,
            device_registry: DeviceRegistry::new(),
            version_vector: VersionVector::default(),
            operations: Vec::new(),
            compaction_checkpoint: None,
            metadata: PersonalStateMetadata::default(),
            projection_fingerprint: None,
        }
    }
}

impl PersonalStateV2 {
    pub fn empty(dataset_id: String) -> Result<Self, PersonalStateError> {
        validate_id("dataset id", &dataset_id, 128)?;
        Ok(Self {
            kind: PERSONAL_STATE_KIND.to_owned(),
            schema_version: PERSONAL_STATE_SCHEMA_VERSION,
            dataset_id,
            revision: 0,
            device_registry: DeviceRegistry::new(),
            version_vector: VersionVector::default(),
            operations: Vec::new(),
            compaction_checkpoint: None,
            metadata: PersonalStateMetadata::default(),
            projection_fingerprint: None,
        })
    }

    pub fn validate(&self) -> Result<(), PersonalStateError> {
        if self.kind != PERSONAL_STATE_KIND {
            return Err(PersonalStateError::UnsupportedKind);
        }
        if self.schema_version != PERSONAL_STATE_SCHEMA_VERSION {
            return Err(PersonalStateError::UnsupportedSchema(self.schema_version));
        }
        validate_id("dataset id", &self.dataset_id, 128)?;
        validate_id("source app version", &self.metadata.source_app_version, 128)?;
        if self.metadata.credentials_included
            || self.metadata.filesystem_paths_included
            || self.metadata.playable_urls_included
        {
            return Err(PersonalStateError::InvalidOperation(
                "portable metadata includes private machine data",
            ));
        }
        if self.operations.len() > MAX_OPERATIONS {
            return Err(PersonalStateError::TooManyOperations);
        }
        if self.device_registry.len() > MAX_DEVICES || self.version_vector.0.len() > MAX_DEVICES {
            return Err(PersonalStateError::TooManyDevices);
        }
        validate_version_vector(&self.version_vector)?;
        for (device_id, device) in &self.device_registry {
            validate_id("device id", device_id.as_str(), MAX_TRACK_ID_CHARS)?;
            if device_id != &device.device_id {
                return Err(PersonalStateError::InvalidOperation(
                    "device registry key does not match its record",
                ));
            }
            validate_text("device name", &device.name)?;
        }
        if let Some(checkpoint) = &self.compaction_checkpoint {
            validate_id("checkpoint id", &checkpoint.checkpoint_id, 256)?;
            if checkpoint.coverage.0.len() > MAX_DEVICES
                || checkpoint.acknowledged_by.len() > MAX_DEVICES
            {
                return Err(PersonalStateError::TooManyDevices);
            }
            validate_version_vector(&checkpoint.coverage)?;
            for device_id in &checkpoint.acknowledged_by {
                validate_id(
                    "checkpoint device id",
                    device_id.as_str(),
                    MAX_TRACK_ID_CHARS,
                )?;
            }
            if let Some(previous_hash) = &checkpoint.previous_checkpoint_hash {
                validate_id("previous checkpoint hash", previous_hash, 128)?;
            }
        }

        let mut dots = BTreeMap::<&Dot, &str>::new();
        let mut ids = BTreeMap::<&str, &OperationEnvelope>::new();
        let mut observed = VersionVector::default();
        for envelope in &self.operations {
            validate_id("operation id", &envelope.operation_id, 256)?;
            validate_id(
                "operation device id",
                envelope.stamp.dot.device_id.as_str(),
                MAX_TRACK_ID_CHARS,
            )?;
            if envelope.stamp.dot.sequence == 0 {
                return Err(PersonalStateError::InvalidOperation(
                    "dot sequence must be positive",
                ));
            }
            if envelope.stamp.observed.0.len() > MAX_DEVICES {
                return Err(PersonalStateError::TooManyDevices);
            }
            validate_version_vector(&envelope.stamp.observed)?;
            if envelope
                .stamp
                .observed
                .observed(&envelope.stamp.dot.device_id)
                >= envelope.stamp.dot.sequence
            {
                return Err(PersonalStateError::InvalidVersionVector);
            }
            if envelope
                .stamp
                .observed
                .0
                .iter()
                .any(|(device, sequence)| self.version_vector.observed(device) < *sequence)
            {
                return Err(PersonalStateError::InvalidVersionVector);
            }
            if let Some(existing) = dots.insert(&envelope.stamp.dot, &envelope.operation_id)
                && existing != envelope.operation_id
            {
                return Err(PersonalStateError::DuplicateDot);
            }
            if let Some(existing) = ids.insert(&envelope.operation_id, envelope)
                && existing != envelope
            {
                return Err(PersonalStateError::ConflictingOperationId);
            }
            validate_origin(&envelope.origin)?;
            validate_operation(&envelope.operation)?;
            observed.observe(&envelope.stamp.dot);
        }
        if observed
            .0
            .iter()
            .any(|(device, sequence)| self.version_vector.observed(device) < *sequence)
        {
            return Err(PersonalStateError::InvalidVersionVector);
        }
        Ok(())
    }

    pub fn normalize(&mut self) -> Result<(), PersonalStateError> {
        self.validate()?;
        self.operations.sort_by(|left, right| {
            left.stamp
                .dot
                .cmp(&right.stamp.dot)
                .then(left.operation_id.cmp(&right.operation_id))
        });
        self.operations
            .dedup_by(|left, right| left.operation_id == right.operation_id);
        // Keep covered history that is no longer present as a raw operation (for example after
        // compaction), while ensuring every retained operation and causal dependency is covered.
        for operation in &self.operations {
            self.version_vector.merge(&operation.stamp.observed);
            self.version_vector.observe(&operation.stamp.dot);
        }
        Ok(())
    }
}

fn validate_operation(operation: &Operation) -> Result<(), PersonalStateError> {
    match operation {
        Operation::SetRating { track, .. } => track.validate(),
        Operation::RecordEngagement {
            event_id,
            track,
            played_duration_ms,
            total_duration_ms,
            artist_key,
            ..
        } => {
            validate_id("event id", event_id, 256)?;
            track.validate()?;
            validate_optional_text("artist key", artist_key)?;
            if let (Some(played), Some(total)) = (played_duration_ms, total_duration_ms)
                && total > &0
                && played > &total.saturating_mul(4)
            {
                return Err(PersonalStateError::InvalidOperation(
                    "played duration is outside the accepted bound",
                ));
            }
            Ok(())
        }
        Operation::SetRadioFavorite { station, .. } => station.validate(),
        Operation::UpsertPlaylist { playlist_id, name } => {
            validate_id("playlist id", playlist_id.as_str(), MAX_TRACK_ID_CHARS)?;
            validate_text("playlist name", name)
        }
        Operation::DeletePlaylist { playlist_id, .. } => {
            validate_id("playlist id", playlist_id.as_str(), MAX_TRACK_ID_CHARS)
        }
        Operation::UpsertPlaylistEntry {
            playlist_id,
            entry_id,
            track,
            after_entry_id,
        } => {
            validate_id("playlist id", playlist_id.as_str(), MAX_TRACK_ID_CHARS)?;
            validate_id("playlist entry id", entry_id.as_str(), MAX_TRACK_ID_CHARS)?;
            if after_entry_id.as_ref() == Some(entry_id) {
                return Err(PersonalStateError::InvalidOperation(
                    "playlist entry cannot follow itself",
                ));
            }
            if let Some(after_entry_id) = after_entry_id {
                validate_id(
                    "previous playlist entry id",
                    after_entry_id.as_str(),
                    MAX_TRACK_ID_CHARS,
                )?;
            }
            track.validate()
        }
        Operation::MovePlaylistEntry {
            playlist_id,
            entry_id,
            after_entry_id,
        } => {
            validate_id("playlist id", playlist_id.as_str(), MAX_TRACK_ID_CHARS)?;
            validate_id("playlist entry id", entry_id.as_str(), MAX_TRACK_ID_CHARS)?;
            if after_entry_id.as_ref() == Some(entry_id) {
                return Err(PersonalStateError::InvalidOperation(
                    "playlist entry cannot follow itself",
                ));
            }
            if let Some(after_entry_id) = after_entry_id {
                validate_id(
                    "previous playlist entry id",
                    after_entry_id.as_str(),
                    MAX_TRACK_ID_CHARS,
                )?;
            }
            Ok(())
        }
        Operation::RemovePlaylistEntry {
            playlist_id,
            entry_id,
            ..
        } => {
            validate_id("playlist id", playlist_id.as_str(), MAX_TRACK_ID_CHARS)?;
            validate_id("playlist entry id", entry_id.as_str(), MAX_TRACK_ID_CHARS)
        }
        Operation::SetStationProfile { query, .. } => {
            if let Some(query) = query {
                validate_text("station query", query)?;
            }
            Ok(())
        }
        Operation::SetAvoidArtist { artist_key, .. } => {
            validate_text("station artist key", artist_key)
        }
        Operation::BindTrack {
            placeholder,
            target,
        } => {
            placeholder.validate()?;
            target.validate()
        }
        Operation::AddDevice { device } => {
            validate_id("device id", device.device_id.as_str(), MAX_TRACK_ID_CHARS)?;
            validate_text("device name", &device.name)
        }
        Operation::RevokeDevice { device_id } => {
            validate_id("device id", device_id.as_str(), MAX_TRACK_ID_CHARS)
        }
        Operation::LegacyBaseline { baseline } => baseline.validate(),
    }
}

fn validate_origin(origin: &OperationOrigin) -> Result<(), PersonalStateError> {
    if let OperationOrigin::OpenSubsonic { backend_id } = origin {
        validate_id("origin backend id", backend_id, MAX_TRACK_ID_CHARS)?;
    }
    Ok(())
}

fn validate_version_vector(vector: &VersionVector) -> Result<(), PersonalStateError> {
    for (device_id, sequence) in &vector.0 {
        validate_id(
            "version-vector device id",
            device_id.as_str(),
            MAX_TRACK_ID_CHARS,
        )?;
        if *sequence == 0 {
            return Err(PersonalStateError::InvalidVersionVector);
        }
    }
    Ok(())
}

pub(crate) fn operation_set(state: &PersonalStateV2) -> HashSet<&str> {
    state
        .operations
        .iter()
        .map(|operation| operation.operation_id.as_str())
        .collect()
}

pub(crate) fn validate_id(
    field: &'static str,
    value: &str,
    max_chars: usize,
) -> Result<(), PersonalStateError> {
    if value.trim().is_empty() {
        return Err(PersonalStateError::EmptyIdentifier(field));
    }
    if value.chars().count() > max_chars || value.chars().any(forbidden_char) {
        return Err(PersonalStateError::IdentifierTooLong(field));
    }
    Ok(())
}

pub(crate) fn validate_text(field: &'static str, value: &str) -> Result<(), PersonalStateError> {
    if value.chars().count() > MAX_TEXT_CHARS || value.chars().any(forbidden_char) {
        return Err(PersonalStateError::InvalidOperation(field));
    }
    Ok(())
}

fn validate_optional_text(field: &'static str, value: &str) -> Result<(), PersonalStateError> {
    if value.is_empty() {
        Ok(())
    } else {
        validate_text(field, value)
    }
}

fn forbidden_char(character: char) -> bool {
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
}
