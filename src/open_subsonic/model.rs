//! Validated identities and sanitized catalog models for OpenSubsonic.

use std::fmt;

use data_encoding::HEXLOWER;
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest as _, Sha256};

const MAX_SERVER_ID_BYTES: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelError {
    EmptyId,
    IdTooLong,
    ForbiddenIdCharacter,
    RandomFailed,
}

impl fmt::Display for ModelError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::EmptyId => "identifier is empty",
            Self::IdTooLong => "identifier is too long",
            Self::ForbiddenIdCharacter => "identifier contains a forbidden character",
            Self::RandomFailed => "secure random generation failed",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for ModelError {}

macro_rules! validated_id {
    ($name:ident, $max_bytes:expr) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, ModelError> {
                let value = value.into();
                validate_id(&value, $max_bytes)?;
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }

            pub fn into_string(self) -> String {
                self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = ModelError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl TryFrom<&str> for $name {
            type Error = ModelError;

            fn try_from(value: &str) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::new(value).map_err(serde::de::Error::custom)
            }
        }
    };
}

validated_id!(BackendId, MAX_SERVER_ID_BYTES);
validated_id!(AccountScopeId, MAX_SERVER_ID_BYTES);
validated_id!(ItemId, MAX_SERVER_ID_BYTES);
validated_id!(CoverArtId, MAX_SERVER_ID_BYTES);
validated_id!(AlbumId, MAX_SERVER_ID_BYTES);
validated_id!(ArtistId, MAX_SERVER_ID_BYTES);
validated_id!(ServerPlaylistId, MAX_SERVER_ID_BYTES);

impl BackendId {
    pub fn random() -> Result<Self, ModelError> {
        random_stable_id().and_then(Self::new)
    }
}

impl AccountScopeId {
    pub fn random() -> Result<Self, ModelError> {
        random_stable_id().and_then(Self::new)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenSubsonicItemRef {
    backend_id: BackendId,
    account_scope_id: AccountScopeId,
    item_id: ItemId,
}

impl OpenSubsonicItemRef {
    pub fn new(backend_id: BackendId, account_scope_id: AccountScopeId, item_id: ItemId) -> Self {
        Self {
            backend_id,
            account_scope_id,
            item_id,
        }
    }

    pub fn backend_id(&self) -> &BackendId {
        &self.backend_id
    }

    pub fn account_scope_id(&self) -> &AccountScopeId {
        &self.account_scope_id
    }

    pub fn item_id(&self) -> &ItemId {
        &self.item_id
    }

    /// Opaque stable identity for UI/cache rows. The exact tuple remains the semantic identity.
    pub fn stable_track_id(&self) -> String {
        let mut digest = Sha256::new();
        digest.update(b"yututui-open-subsonic-track-v1\0");
        update_length_prefixed(&mut digest, self.backend_id.as_str().as_bytes());
        update_length_prefixed(&mut digest, self.account_scope_id.as_str().as_bytes());
        update_length_prefixed(&mut digest, self.item_id.as_str().as_bytes());
        format!("sub:{}", HEXLOWER.encode(&digest.finalize()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerSong {
    pub item: OpenSubsonicItemRef,
    pub title: String,
    pub artist: String,
    pub artists: Vec<String>,
    pub album: Option<String>,
    pub album_id: Option<AlbumId>,
    pub album_artist: Option<String>,
    pub duration_secs: Option<u32>,
    pub track_number: Option<u32>,
    pub disc_number: Option<u32>,
    pub year: Option<u32>,
    pub cover_art_id: Option<CoverArtId>,
    pub content_type: Option<String>,
    pub suffix: Option<String>,
    pub starred: bool,
    /// Exact server value. Values outside `0..=5` remain available to the bridge shadow so a
    /// malformed/mobile-written value is never silently normalized.
    pub user_rating: Option<i64>,
    /// Aggregate fallback evidence from the standard Child response.
    pub play_count: Option<u64>,
    /// Sanitized RFC3339-ish server value. Parsing is deferred to the history bridge; malformed
    /// values cannot make an otherwise playable catalog row disappear.
    pub played_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerAlbum {
    pub id: AlbumId,
    pub name: String,
    pub artist: String,
    pub artist_id: Option<ArtistId>,
    pub song_count: Option<u32>,
    pub duration_secs: Option<u32>,
    pub year: Option<u32>,
    pub cover_art_id: Option<CoverArtId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerArtist {
    pub id: ArtistId,
    pub name: String,
    pub album_count: Option<u32>,
    pub cover_art_id: Option<CoverArtId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerPlaylistSummary {
    pub id: ServerPlaylistId,
    pub name: String,
    pub owner: Option<String>,
    pub song_count: Option<u32>,
    pub duration_secs: Option<u32>,
    pub public: Option<bool>,
    pub cover_art_id: Option<CoverArtId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerPlaylist {
    pub summary: ServerPlaylistSummary,
    pub entries: Vec<ServerSong>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next_offset: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ServerLibrarySection {
    RecentlyPlayed,
    Albums,
    Artists,
    Songs,
    Playlists,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(
    clippy::large_enum_variant,
    reason = "library rows stay inline because pages are bounded and consumers borrow every variant"
)]
pub enum ServerLibraryRow {
    Song(ServerSong),
    Album(ServerAlbum),
    Artist(ServerArtist),
    Playlist(ServerPlaylistSummary),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LibraryWarning {
    FeatureUnsupported,
    PartialResults,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerLibraryPage {
    pub section: ServerLibrarySection,
    pub rows: Vec<ServerLibraryRow>,
    pub next_offset: Option<u32>,
    pub warning: Option<LibraryWarning>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerLibraryDetail {
    AlbumSongs {
        album: ServerAlbum,
        songs: Vec<ServerSong>,
    },
    ArtistAlbums {
        artist: ServerArtist,
        albums: Vec<ServerAlbum>,
    },
    PlaylistEntries(ServerPlaylist),
}

fn validate_id(value: &str, max_bytes: usize) -> Result<(), ModelError> {
    if value.is_empty() {
        return Err(ModelError::EmptyId);
    }
    if value.len() > max_bytes {
        return Err(ModelError::IdTooLong);
    }
    if value.chars().any(is_forbidden_id_character) {
        return Err(ModelError::ForbiddenIdCharacter);
    }
    Ok(())
}

fn is_forbidden_id_character(character: char) -> bool {
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

fn random_stable_id() -> Result<String, ModelError> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| ModelError::RandomFailed)?;
    Ok(HEXLOWER.encode(&bytes))
}

fn update_length_prefixed(digest: &mut Sha256, bytes: &[u8]) {
    digest.update((bytes.len() as u64).to_be_bytes());
    digest.update(bytes);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(backend: &str, account: &str, item: &str) -> OpenSubsonicItemRef {
        OpenSubsonicItemRef::new(
            BackendId::new(backend).unwrap(),
            AccountScopeId::new(account).unwrap(),
            ItemId::new(item).unwrap(),
        )
    }

    #[test]
    fn item_reference_round_trips_through_serde() {
        let original = item("backend-a", "account-a", "song-1");
        let encoded = serde_json::to_vec(&original).unwrap();
        let decoded: OpenSubsonicItemRef = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded, original);
        assert_eq!(decoded.backend_id().as_str(), "backend-a");
        assert_eq!(decoded.account_scope_id().as_str(), "account-a");
        assert_eq!(decoded.item_id().as_str(), "song-1");
    }

    #[test]
    fn stable_track_id_includes_backend_and_account_scope() {
        let base = item("backend-a", "account-a", "song-1");
        assert_ne!(
            base.stable_track_id(),
            item("backend-b", "account-a", "song-1").stable_track_id()
        );
        assert_ne!(
            base.stable_track_id(),
            item("backend-a", "account-b", "song-1").stable_track_id()
        );
        assert_eq!(
            base.stable_track_id(),
            item("backend-a", "account-a", "song-1").stable_track_id()
        );
    }

    #[test]
    fn deserialization_rejects_unbounded_or_unsafe_ids() {
        assert!(ItemId::new("").is_err());
        assert!(ItemId::new("x".repeat(MAX_SERVER_ID_BYTES + 1)).is_err());
        assert!(ItemId::new("unsafe\u{202e}id").is_err());
        assert!(serde_json::from_str::<ItemId>("\"unsafe\\nvalue\"").is_err());
    }

    #[test]
    fn length_prefix_prevents_tuple_boundary_aliases() {
        assert_ne!(
            item("ab", "c", "d").stable_track_id(),
            item("a", "bc", "d").stable_track_id()
        );
    }
}
