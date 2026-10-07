use serde::{Deserialize, Serialize};

use crate::personal_state::{DeviceId, PersonalStateError, PortableTrack, PortableTrackKey};
use crate::streaming::TasteSnapshot;

const ID_CHARS_MAX: usize = 256;
const LABEL_CHARS_MAX: usize = 256;
const NOTE_CHARS_MAX: usize = 1_024;

macro_rules! string_id {
    ($name:ident, $field:literal) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, PersonalStateError> {
                let value = value.into();
                validate_id($field, &value)?;
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

string_id!(BookmarkId, "bookmark id");
string_id!(DjPresetId, "DJ preset id");

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BookmarkRecord {
    pub bookmark_id: BookmarkId,
    pub track: PortableTrack,
    pub position_ms: u64,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResumeProvenance {
    pub playback_session_id: String,
    pub device_id: DeviceId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResumePoint {
    pub track: PortableTrack,
    pub position_ms: u64,
    pub provenance: ResumeProvenance,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResumeClear {
    pub track: PortableTrack,
    pub provenance: ResumeProvenance,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ResumeCandidate {
    Position(ResumePoint),
    Clear(ResumeClear),
}

impl ResumeCandidate {
    pub fn track(&self) -> &PortableTrack {
        match self {
            Self::Position(point) => &point.track,
            Self::Clear(clear) => &clear.track,
        }
    }

    pub fn provenance(&self) -> &ResumeProvenance {
        match self {
            Self::Position(point) => &point.provenance,
            Self::Clear(clear) => &clear.provenance,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PassportVisit {
    pub station_uuid: String,
    pub station_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub country_code: Option<String>,
    pub first_listened_at_unix: i64,
    pub last_listened_at_unix: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PassportNote {
    pub station_uuid: String,
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DjPreset {
    pub preset_id: DjPresetId,
    pub name: String,
    pub snapshot: TasteSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type", deny_unknown_fields)]
pub enum ListeningOperation {
    UpsertBookmark { bookmark: BookmarkRecord },
    DeleteBookmark { bookmark_id: BookmarkId },
    SetResume { point: ResumePoint },
    ClearResume { clear: ResumeClear },
    RecordPassportVisit { visit: PassportVisit },
    DeletePassportVisit { station_uuid: String },
    SetPassportNote { note: PassportNote },
    DeletePassportNote { station_uuid: String },
    ClearPassport,
    UpsertDjPreset { preset: DjPreset },
    DeleteDjPreset { preset_id: DjPresetId },
}

impl ListeningOperation {
    pub fn validate(&self) -> Result<(), PersonalStateError> {
        match self {
            Self::UpsertBookmark { bookmark } => bookmark.validate(),
            Self::DeleteBookmark { bookmark_id } => {
                validate_id("bookmark id", bookmark_id.as_str())
            }
            Self::SetResume { point } => point.validate(),
            Self::ClearResume { clear } => clear.validate(),
            Self::RecordPassportVisit { visit } => visit.validate(),
            Self::DeletePassportVisit { station_uuid }
            | Self::DeletePassportNote { station_uuid } => {
                validate_id("station UUID", station_uuid)
            }
            Self::SetPassportNote { note } => note.validate(),
            Self::ClearPassport => Ok(()),
            Self::UpsertDjPreset { preset } => preset.validate(),
            Self::DeleteDjPreset { preset_id } => validate_id("DJ preset id", preset_id.as_str()),
        }
    }
}

impl BookmarkRecord {
    pub fn validate(&self) -> Result<(), PersonalStateError> {
        validate_id("bookmark id", self.bookmark_id.as_str())?;
        validate_track(&self.track)?;
        validate_position(&self.track, self.position_ms)?;
        validate_text("bookmark label", &self.label, LABEL_CHARS_MAX, false)
    }
}

impl ResumeProvenance {
    pub fn validate(&self) -> Result<(), PersonalStateError> {
        validate_id("playback session id", &self.playback_session_id)?;
        validate_id("resume device id", self.device_id.as_str())
    }
}

impl ResumePoint {
    pub fn validate(&self) -> Result<(), PersonalStateError> {
        validate_track(&self.track)?;
        validate_position(&self.track, self.position_ms)?;
        self.provenance.validate()
    }
}

impl ResumeClear {
    pub fn validate(&self) -> Result<(), PersonalStateError> {
        validate_track(&self.track)?;
        self.provenance.validate()
    }
}

impl PassportVisit {
    pub fn validate(&self) -> Result<(), PersonalStateError> {
        validate_id("station UUID", &self.station_uuid)?;
        validate_text("station name", &self.station_name, LABEL_CHARS_MAX, false)?;
        if let Some(country_code) = &self.country_code
            && (country_code.len() != 2
                || !country_code.bytes().all(|byte| byte.is_ascii_uppercase()))
        {
            return Err(invalid("country code must be two uppercase ASCII letters"));
        }
        if self.first_listened_at_unix > self.last_listened_at_unix {
            return Err(invalid("passport visit timestamps are reversed"));
        }
        Ok(())
    }
}

impl PassportNote {
    pub fn validate(&self) -> Result<(), PersonalStateError> {
        validate_id("station UUID", &self.station_uuid)?;
        validate_text("passport note", &self.note, NOTE_CHARS_MAX, false)
    }
}

impl DjPreset {
    pub fn validate(&self) -> Result<(), PersonalStateError> {
        validate_id("DJ preset id", self.preset_id.as_str())?;
        validate_text("DJ preset name", &self.name, LABEL_CHARS_MAX, false)?;
        self.snapshot
            .validate()
            .map_err(|_| invalid("DJ preset contains an invalid taste snapshot"))
    }
}

fn validate_track(track: &PortableTrack) -> Result<(), PersonalStateError> {
    track.validate()?;
    if [&track.title, &track.artist]
        .into_iter()
        .chain(track.album.iter())
        .chain(track.isrc.iter())
        .any(|value| looks_like_location(value))
    {
        return Err(invalid("portable track metadata contains a path or URL"));
    }
    let values: Vec<&str> = match &track.key {
        PortableTrackKey::Catalog {
            provider,
            exact_catalog_id,
        } => vec![provider, exact_catalog_id],
        PortableTrackKey::OpenSubsonic {
            backend_id,
            account_scope_id,
            item_id,
        } => {
            vec![backend_id, account_scope_id, item_id]
        }
        PortableTrackKey::LocalPlaceholder {
            portable_placeholder_id,
        } => vec![portable_placeholder_id],
    };
    if values.into_iter().any(looks_like_location) {
        return Err(invalid("portable track identity contains a path or URL"));
    }
    Ok(())
}

fn validate_position(track: &PortableTrack, position_ms: u64) -> Result<(), PersonalStateError> {
    if let Some(duration_secs) = track.duration_secs
        && position_ms
            > u64::from(duration_secs)
                .saturating_add(1)
                .saturating_mul(1_000)
    {
        return Err(invalid(
            "listening position exceeds the known track duration",
        ));
    }
    Ok(())
}

fn validate_id(field: &'static str, value: &str) -> Result<(), PersonalStateError> {
    validate_text(field, value, ID_CHARS_MAX, false)
}

fn validate_text(
    field: &'static str,
    value: &str,
    max_chars: usize,
    allow_empty: bool,
) -> Result<(), PersonalStateError> {
    if !allow_empty && value.trim().is_empty() {
        return Err(PersonalStateError::EmptyIdentifier(field));
    }
    if value.chars().count() > max_chars {
        return Err(PersonalStateError::IdentifierTooLong(field));
    }
    if value.trim() != value || value.chars().any(char::is_control) {
        return Err(invalid("listening text contains unsafe characters"));
    }
    if looks_like_location(value) {
        return Err(invalid("listening records must not contain paths or URLs"));
    }
    Ok(())
}

fn looks_like_location(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    lower.contains("http://")
        || lower.contains("https://")
        || lower.contains("file://")
        || value.split_whitespace().any(|word| {
            let normalized = word.to_ascii_lowercase();
            let word = normalized.strip_prefix("local:").unwrap_or(&normalized);
            word.starts_with('/')
                || word.starts_with("~/")
                || word.starts_with("\\\\")
                || (word.len() >= 3
                    && word.as_bytes()[1] == b':'
                    && matches!(word.as_bytes()[2], b'/' | b'\\'))
        })
}

fn invalid(reason: &'static str) -> PersonalStateError {
    PersonalStateError::InvalidOperation(reason)
}
