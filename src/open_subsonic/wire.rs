//! Tolerant, allocation-bounded OpenSubsonic JSON wire models.
//!
//! Unknown server fields are intentionally ignored. Every server-controlled sequence is rejected
//! at its first excess item while serde is consuming it, before a large `Vec` can be allocated.

use std::fmt;
use std::marker::PhantomData;

use serde::de::{DeserializeSeed, Error as _, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};

const MAX_WIRE_PAGE_ROWS: usize = 200;
const MAX_WIRE_NESTED_ROWS: usize = 20_000;
const MAX_WIRE_ARTIST_INDEXES: usize = 256;
const MAX_WIRE_CHILD_ARTISTS: usize = 32;
const MAX_WIRE_EXTENSIONS: usize = 256;
const MAX_WIRE_EXTENSION_VERSIONS: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WireError {
    InvalidResponse,
    ApiFailure(Option<i32>),
}

#[derive(Deserialize)]
pub(crate) struct Envelope {
    #[serde(rename = "subsonic-response")]
    pub response: RawResponse,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RawResponse {
    pub status: String,
    pub version: Option<String>,
    #[serde(rename = "type")]
    pub server_type: Option<String>,
    pub server_version: Option<String>,
    pub open_subsonic: Option<bool>,
    pub error: Option<RawApiError>,
    pub token_info: Option<RawTokenInfo>,
    #[serde(default, deserialize_with = "deserialize_extensions")]
    pub open_subsonic_extensions: Vec<RawExtension>,
    pub search_result3: Option<RawSearchResult3>,
    pub album_list2: Option<RawAlbumList>,
    pub artists: Option<RawArtists>,
    pub playlists: Option<RawPlaylists>,
    pub playlist: Option<RawPlaylist>,
    pub album: Option<RawAlbumWithSongs>,
    pub artist: Option<RawArtistWithAlbums>,
    pub song: Option<RawChild>,
}

#[derive(Deserialize)]
pub(crate) struct RawTokenInfo {
    pub username: Option<String>,
}

impl RawResponse {
    pub(crate) fn ensure_ok(&self) -> Result<(), WireError> {
        if self.status == "ok" {
            Ok(())
        } else {
            Err(WireError::ApiFailure(
                self.error.as_ref().and_then(|error| error.code),
            ))
        }
    }
}

#[derive(Deserialize)]
pub(crate) struct RawApiError {
    pub code: Option<i32>,
}

#[derive(Deserialize)]
pub(crate) struct RawExtension {
    pub name: Option<String>,
    #[serde(default, deserialize_with = "deserialize_extension_versions")]
    pub versions: Vec<u32>,
}

#[derive(Default, Deserialize)]
pub(crate) struct RawSearchResult3 {
    #[serde(default, deserialize_with = "deserialize_page_children")]
    pub song: Vec<RawChild>,
}

#[derive(Default, Deserialize)]
pub(crate) struct RawAlbumList {
    #[serde(default, deserialize_with = "deserialize_page_albums")]
    pub album: Vec<RawAlbum>,
}

#[derive(Default, Deserialize)]
pub(crate) struct RawArtists {
    #[serde(default, deserialize_with = "deserialize_artist_indexes")]
    pub index: Vec<RawArtistIndex>,
}

#[derive(Default)]
pub(crate) struct RawArtistIndex {
    pub artist: Vec<RawArtist>,
}

#[derive(Default, Deserialize)]
pub(crate) struct RawPlaylists {
    #[serde(default, deserialize_with = "deserialize_nested_playlists")]
    pub playlist: Vec<RawPlaylistSummary>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RawPlaylist {
    pub id: Option<String>,
    pub name: Option<String>,
    pub owner: Option<String>,
    pub readonly: Option<bool>,
    pub song_count: Option<u64>,
    pub duration: Option<u64>,
    pub public: Option<bool>,
    pub cover_art: Option<String>,
    #[serde(default, deserialize_with = "deserialize_nested_children")]
    pub entry: Vec<RawChild>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RawPlaylistSummary {
    pub id: Option<String>,
    pub name: Option<String>,
    pub owner: Option<String>,
    pub readonly: Option<bool>,
    pub song_count: Option<u64>,
    pub duration: Option<u64>,
    pub public: Option<bool>,
    pub cover_art: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RawAlbumWithSongs {
    pub id: Option<String>,
    pub name: Option<String>,
    pub artist: Option<String>,
    pub artist_id: Option<String>,
    pub song_count: Option<u64>,
    pub duration: Option<u64>,
    pub year: Option<u64>,
    pub cover_art: Option<String>,
    #[serde(default, deserialize_with = "deserialize_nested_children")]
    pub song: Vec<RawChild>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RawArtistWithAlbums {
    pub id: Option<String>,
    pub name: Option<String>,
    pub album_count: Option<u64>,
    pub cover_art: Option<String>,
    #[serde(default, deserialize_with = "deserialize_nested_albums")]
    pub album: Vec<RawAlbum>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RawChild {
    pub id: Option<String>,
    pub is_dir: Option<bool>,
    pub title: Option<String>,
    pub album: Option<String>,
    pub artist: Option<String>,
    pub track: Option<u64>,
    pub year: Option<u64>,
    pub cover_art: Option<String>,
    pub content_type: Option<String>,
    pub suffix: Option<String>,
    pub duration: Option<u64>,
    pub disc_number: Option<u64>,
    pub album_id: Option<String>,
    #[serde(rename = "type")]
    pub media_type: Option<String>,
    pub user_rating: Option<i64>,
    pub starred: Option<String>,
    pub play_count: Option<u64>,
    pub played: Option<String>,
    #[serde(default, deserialize_with = "deserialize_child_artists")]
    pub artists: Vec<RawNamedId>,
}

#[derive(Default, Deserialize)]
pub(crate) struct RawNamedId {
    pub name: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RawAlbum {
    pub id: Option<String>,
    pub name: Option<String>,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub artist_id: Option<String>,
    pub song_count: Option<u64>,
    pub duration: Option<u64>,
    pub year: Option<u64>,
    pub cover_art: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RawArtist {
    pub id: Option<String>,
    pub name: Option<String>,
    pub album_count: Option<u64>,
    pub cover_art: Option<String>,
}

fn deserialize_bounded_vec<'de, D, T, const LIMIT: usize>(
    deserializer: D,
) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct BoundedVecVisitor<T, const LIMIT: usize>(PhantomData<T>);

    impl<'de, T, const LIMIT: usize> Visitor<'de> for BoundedVecVisitor<T, LIMIT>
    where
        T: Deserialize<'de>,
    {
        type Value = Vec<T>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a bounded sequence")
        }

        fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            let mut values = Vec::with_capacity(sequence.size_hint().unwrap_or(0).min(LIMIT));
            while values.len() < LIMIT {
                let Some(value) = sequence.next_element()? else {
                    return Ok(values);
                };
                values.push(value);
            }
            if sequence.next_element::<IgnoredAny>()?.is_some() {
                return Err(A::Error::custom("sequence exceeds its item limit"));
            }
            Ok(values)
        }
    }

    deserializer.deserialize_seq(BoundedVecVisitor::<T, LIMIT>(PhantomData))
}

macro_rules! bounded_sequence {
    ($function:ident, $item:ty, $limit:expr) => {
        fn $function<'de, D>(deserializer: D) -> Result<Vec<$item>, D::Error>
        where
            D: Deserializer<'de>,
        {
            deserialize_bounded_vec::<D, $item, $limit>(deserializer)
        }
    };
}

bounded_sequence!(deserialize_extensions, RawExtension, MAX_WIRE_EXTENSIONS);
bounded_sequence!(
    deserialize_extension_versions,
    u32,
    MAX_WIRE_EXTENSION_VERSIONS
);
bounded_sequence!(deserialize_page_children, RawChild, MAX_WIRE_PAGE_ROWS);
bounded_sequence!(deserialize_page_albums, RawAlbum, MAX_WIRE_PAGE_ROWS);
bounded_sequence!(
    deserialize_nested_playlists,
    RawPlaylistSummary,
    MAX_WIRE_NESTED_ROWS
);
bounded_sequence!(deserialize_nested_children, RawChild, MAX_WIRE_NESTED_ROWS);
bounded_sequence!(deserialize_nested_albums, RawAlbum, MAX_WIRE_NESTED_ROWS);
bounded_sequence!(
    deserialize_child_artists,
    RawNamedId,
    MAX_WIRE_CHILD_ARTISTS
);

fn deserialize_artist_indexes<'de, D>(deserializer: D) -> Result<Vec<RawArtistIndex>, D::Error>
where
    D: Deserializer<'de>,
{
    struct ArtistVecSeed<'a> {
        remaining: &'a mut usize,
    }

    impl<'de> DeserializeSeed<'de> for ArtistVecSeed<'_> {
        type Value = Vec<RawArtist>;

        fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
        where
            D: Deserializer<'de>,
        {
            struct ArtistVecVisitor<'a> {
                remaining: &'a mut usize,
            }

            impl<'de> Visitor<'de> for ArtistVecVisitor<'_> {
                type Value = Vec<RawArtist>;

                fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                    formatter.write_str("an artist sequence within the shared row limit")
                }

                fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
                where
                    A: SeqAccess<'de>,
                {
                    let capacity = sequence.size_hint().unwrap_or(0).min(*self.remaining);
                    let mut artists = Vec::with_capacity(capacity);
                    while *self.remaining > 0 {
                        let Some(artist) = sequence.next_element()? else {
                            return Ok(artists);
                        };
                        artists.push(artist);
                        *self.remaining -= 1;
                    }
                    if sequence.next_element::<IgnoredAny>()?.is_some() {
                        return Err(A::Error::custom(
                            "artist sequences exceed their shared row limit",
                        ));
                    }
                    Ok(artists)
                }
            }

            deserializer.deserialize_seq(ArtistVecVisitor {
                remaining: self.remaining,
            })
        }
    }

    #[derive(Clone, Copy)]
    enum ArtistIndexField {
        Artist,
        Other,
    }

    impl<'de> Deserialize<'de> for ArtistIndexField {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: Deserializer<'de>,
        {
            struct ArtistIndexFieldVisitor;

            impl<'de> Visitor<'de> for ArtistIndexFieldVisitor {
                type Value = ArtistIndexField;

                fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                    formatter.write_str("an artist index field")
                }

                fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
                where
                    E: serde::de::Error,
                {
                    Ok(if value == "artist" {
                        ArtistIndexField::Artist
                    } else {
                        ArtistIndexField::Other
                    })
                }
            }

            deserializer.deserialize_identifier(ArtistIndexFieldVisitor)
        }
    }

    struct ArtistIndexSeed<'a> {
        remaining: &'a mut usize,
    }

    impl<'de> DeserializeSeed<'de> for ArtistIndexSeed<'_> {
        type Value = RawArtistIndex;

        fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
        where
            D: Deserializer<'de>,
        {
            struct ArtistIndexVisitor<'a> {
                remaining: &'a mut usize,
            }

            impl<'de> Visitor<'de> for ArtistIndexVisitor<'_> {
                type Value = RawArtistIndex;

                fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                    formatter.write_str("an artist index object")
                }

                fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
                where
                    A: MapAccess<'de>,
                {
                    let mut artists = None;
                    while let Some(field) = map.next_key::<ArtistIndexField>()? {
                        match field {
                            ArtistIndexField::Artist => {
                                if artists.is_some() {
                                    return Err(A::Error::duplicate_field("artist"));
                                }
                                artists = Some(map.next_value_seed(ArtistVecSeed {
                                    remaining: self.remaining,
                                })?);
                            }
                            ArtistIndexField::Other => {
                                map.next_value::<IgnoredAny>()?;
                            }
                        }
                    }
                    Ok(RawArtistIndex {
                        artist: artists.unwrap_or_default(),
                    })
                }
            }

            deserializer.deserialize_map(ArtistIndexVisitor {
                remaining: self.remaining,
            })
        }
    }

    struct ArtistIndexesVisitor;

    impl<'de> Visitor<'de> for ArtistIndexesVisitor {
        type Value = Vec<RawArtistIndex>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a bounded artist index sequence")
        }

        fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            let mut indexes = Vec::with_capacity(
                sequence
                    .size_hint()
                    .unwrap_or(0)
                    .min(MAX_WIRE_ARTIST_INDEXES),
            );
            let mut remaining = MAX_WIRE_NESTED_ROWS;
            while indexes.len() < MAX_WIRE_ARTIST_INDEXES {
                let Some(index) = sequence.next_element_seed(ArtistIndexSeed {
                    remaining: &mut remaining,
                })?
                else {
                    return Ok(indexes);
                };
                indexes.push(index);
            }
            if sequence.next_element::<IgnoredAny>()?.is_some() {
                return Err(A::Error::custom("artist index sequence exceeds its limit"));
            }
            Ok(indexes)
        }
    }

    deserializer.deserialize_seq(ArtistIndexesVisitor)
}

pub(crate) fn decode(bytes: &[u8]) -> Result<RawResponse, WireError> {
    let envelope: Envelope =
        serde_json::from_slice(bytes).map_err(|_| WireError::InvalidResponse)?;
    envelope.response.ensure_ok()?;
    Ok(envelope.response)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[test]
    fn extension_array_and_unknown_fields_are_accepted() {
        let response = decode(
            br#"{
                "subsonic-response": {
                    "status": "ok",
                    "version": "1.16.1",
                    "futureField": {"anything": true},
                    "openSubsonicExtensions": [
                        {"name":"formPost","versions":[1],"future":true}
                    ]
                }
            }"#,
        )
        .unwrap();
        assert_eq!(response.open_subsonic_extensions.len(), 1);
        assert_eq!(
            response.open_subsonic_extensions[0].name.as_deref(),
            Some("formPost")
        );
    }

    #[test]
    fn token_info_preserves_the_authenticated_username() {
        let response =
            decode(br#"{"subsonic-response":{"status":"ok","tokenInfo":{"username":"alice"}}}"#)
                .unwrap();
        assert_eq!(
            response
                .token_info
                .and_then(|token_info| token_info.username)
                .as_deref(),
            Some("alice")
        );
    }

    #[test]
    fn failed_envelope_is_not_treated_as_http_success() {
        assert!(matches!(
            decode(
                br#"{"subsonic-response":{"status":"failed","error":{"code":40,"message":"bad"}}}"#
            ),
            Err(WireError::ApiFailure(Some(40)))
        ));
    }

    #[test]
    fn playlist_readonly_preserves_true_false_and_absent() {
        for (raw, expected) in [
            (
                br#"{"subsonic-response":{"status":"ok","playlist":{"readonly":true}}}"#.as_slice(),
                Some(true),
            ),
            (
                br#"{"subsonic-response":{"status":"ok","playlist":{"readonly":false}}}"#
                    .as_slice(),
                Some(false),
            ),
            (
                br#"{"subsonic-response":{"status":"ok","playlist":{}}}"#.as_slice(),
                None,
            ),
        ] {
            assert_eq!(decode(raw).unwrap().playlist.unwrap().readonly, expected);
        }
        let response = decode(
            br#"{"subsonic-response":{"status":"ok","playlists":{"playlist":[{"readonly":false}]}}}"#,
        )
        .unwrap();
        assert_eq!(
            response.playlists.unwrap().playlist[0].readonly,
            Some(false)
        );
    }

    #[test]
    fn playlist_readonly_rejects_non_boolean_values() {
        assert!(matches!(
            decode(br#"{"subsonic-response":{"status":"ok","playlist":{"readonly":"false"}}}"#),
            Err(WireError::InvalidResponse)
        ));
    }

    #[test]
    fn server_controlled_sequences_at_their_limits_are_accepted() {
        let songs = std::iter::repeat_n("{}", MAX_WIRE_PAGE_ROWS - 1)
            .collect::<Vec<_>>()
            .join(",");
        let artists = std::iter::repeat_n(r#"{"name":"artist"}"#, MAX_WIRE_CHILD_ARTISTS)
            .collect::<Vec<_>>()
            .join(",");
        let extensions = (0..MAX_WIRE_EXTENSIONS)
            .map(|index| {
                format!(r#"{{"name":"ext{index}","versions":[{}]}}"#, {
                    std::iter::repeat_n("1", MAX_WIRE_EXTENSION_VERSIONS)
                        .collect::<Vec<_>>()
                        .join(",")
                })
            })
            .collect::<Vec<_>>()
            .join(",");
        let payload = format!(
            r#"{{
                "subsonic-response": {{
                    "status": "ok",
                    "openSubsonicExtensions": [{extensions}],
                    "searchResult3": {{
                        "song": [{{"id":"kept","artists":[{artists}]}},{songs}]
                    }}
                }}
            }}"#
        );
        let response = decode(payload.as_bytes()).unwrap();
        assert_eq!(response.open_subsonic_extensions.len(), MAX_WIRE_EXTENSIONS);
        assert!(
            response
                .open_subsonic_extensions
                .iter()
                .all(|extension| extension.versions.len() <= MAX_WIRE_EXTENSION_VERSIONS)
        );
        let songs = response.search_result3.unwrap().song;
        assert_eq!(songs.len(), MAX_WIRE_PAGE_ROWS);
        assert_eq!(songs[0].artists.len(), MAX_WIRE_CHILD_ARTISTS);
    }

    #[test]
    fn bounded_vec_stops_typed_deserialization_at_limit_plus_one() {
        static DESERIALIZED: AtomicUsize = AtomicUsize::new(0);

        struct Counted;

        impl<'de> Deserialize<'de> for Counted {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                DESERIALIZED.fetch_add(1, Ordering::SeqCst);
                IgnoredAny::deserialize(deserializer)?;
                Ok(Self)
            }
        }

        DESERIALIZED.store(0, Ordering::SeqCst);
        let mut decoder = serde_json::Deserializer::from_str("[{},{},{},{},{}]");
        assert!(deserialize_bounded_vec::<_, Counted, 2>(&mut decoder).is_err());
        assert_eq!(DESERIALIZED.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn oversized_sequences_within_response_cap_fail_closed() {
        let payloads = [
            format!(
                r#"{{"subsonic-response":{{"status":"ok","searchResult3":{{"song":[{}]}}}}}}"#,
                std::iter::repeat_n("{}", MAX_WIRE_PAGE_ROWS + 1)
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            format!(
                r#"{{"subsonic-response":{{"status":"ok","openSubsonicExtensions":[{}]}}}}"#,
                std::iter::repeat_n("{}", MAX_WIRE_EXTENSIONS + 1)
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            format!(
                r#"{{"subsonic-response":{{"status":"ok","searchResult3":{{"song":[{{"artists":[{}]}}]}}}}}}"#,
                std::iter::repeat_n("{}", MAX_WIRE_CHILD_ARTISTS + 1)
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            format!(
                r#"{{"subsonic-response":{{"status":"ok","playlists":{{"playlist":[{}]}}}}}}"#,
                std::iter::repeat_n("{}", MAX_WIRE_NESTED_ROWS + 1)
                    .collect::<Vec<_>>()
                    .join(",")
            ),
        ];

        for payload in payloads {
            assert!(payload.len() < 8 * 1024 * 1024);
            assert!(matches!(
                decode(payload.as_bytes()),
                Err(WireError::InvalidResponse)
            ));
        }
    }

    #[test]
    fn nested_artist_indexes_accept_exact_shared_total_budget() {
        let first = std::iter::repeat_n(r#"{"id":"a"}"#, 12_000)
            .collect::<Vec<_>>()
            .join(",");
        let second = std::iter::repeat_n(r#"{"id":"b"}"#, 8_000)
            .collect::<Vec<_>>()
            .join(",");
        let payload = format!(
            r#"{{
                "subsonic-response": {{
                    "status": "ok",
                    "artists": {{
                        "index": [
                            {{"artist":[{first}]}},
                            {{"artist":[{second}]}}
                        ]
                    }}
                }}
            }}"#
        );
        let indexes = decode(payload.as_bytes()).unwrap().artists.unwrap().index;
        assert_eq!(
            indexes
                .iter()
                .map(|index| index.artist.len())
                .sum::<usize>(),
            MAX_WIRE_NESTED_ROWS
        );
    }

    #[test]
    fn nested_artist_indexes_reject_shared_total_limit_plus_one() {
        let first = std::iter::repeat_n(r#"{"id":"a"}"#, 12_000)
            .collect::<Vec<_>>()
            .join(",");
        let second = std::iter::repeat_n(r#"{"id":"b"}"#, 8_001)
            .collect::<Vec<_>>()
            .join(",");
        let payload = format!(
            r#"{{
                "subsonic-response": {{
                    "status": "ok",
                    "artists": {{
                        "index": [
                            {{"artist":[{first}]}},
                            {{"artist":[{second}]}}
                        ]
                    }}
                }}
            }}"#
        );
        assert!(payload.len() < 8 * 1024 * 1024);
        assert!(matches!(
            decode(payload.as_bytes()),
            Err(WireError::InvalidResponse)
        ));
    }

    #[test]
    fn artist_index_count_rejects_limit_plus_one_without_typed_excess() {
        let indexes = std::iter::repeat_n(r#"{"artist":[]}"#, MAX_WIRE_ARTIST_INDEXES + 1)
            .collect::<Vec<_>>()
            .join(",");
        let payload = format!(
            r#"{{"subsonic-response":{{"status":"ok","artists":{{"index":[{indexes}]}}}}}}"#
        );
        assert!(payload.len() < 8 * 1024 * 1024);
        assert!(matches!(
            decode(payload.as_bytes()),
            Err(WireError::InvalidResponse)
        ));
    }
}
