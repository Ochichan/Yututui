//! Spotify response models: raw serde shapes plus the simplified app-facing types.
//! Playlist items are decoded tolerantly — entries can be episodes, local files, or
//! `null` (removed tracks); those become `None` and the caller counts them as skipped.
//!
//! March 2026 API migration: playlist contents moved to `/playlists/{id}/items` and the
//! field names changed (`tracks` → `items`, wrapper `track` → `item`). The raw shapes
//! here accept BOTH generations — `/me/tracks` and `/search` still use the old names.

use serde::Deserialize;

/// Spotify's standard offset envelope.
#[derive(Debug, Clone, Deserialize)]
pub struct Paging<T> {
    #[serde(default = "Vec::new")]
    pub items: Vec<T>,
    #[serde(default)]
    pub next: Option<String>,
    #[serde(default)]
    pub total: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SpotifyUser {
    pub id: String,
    #[serde(default)]
    pub display_name: Option<String>,
}

impl SpotifyUser {
    pub fn label(&self) -> &str {
        self.display_name
            .as_deref()
            .filter(|n| !n.is_empty())
            .unwrap_or(&self.id)
    }
}

#[derive(Debug, Clone)]
pub struct SpotifyPlaylist {
    pub id: String,
    pub name: String,
    pub owner: String,
    pub total: u32,
    pub collaborative: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawPlaylist {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub owner: Option<RawOwner>,
    /// Pre-migration name for the contents ref (`{total}`); `null` on new responses.
    #[serde(default)]
    pub tracks: Option<RawPlaylistTracksRef>,
    /// Post-March-2026 name for the same ref.
    #[serde(default)]
    pub items: Option<RawPlaylistTracksRef>,
    #[serde(default)]
    pub collaborative: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawOwner {
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawPlaylistTracksRef {
    #[serde(default)]
    pub total: u32,
}

impl From<RawPlaylist> for SpotifyPlaylist {
    fn from(raw: RawPlaylist) -> Self {
        let owner = raw
            .owner
            .and_then(|o| o.display_name.filter(|n| !n.is_empty()).or(o.id))
            .unwrap_or_default();
        Self {
            id: raw.id,
            name: raw.name,
            owner,
            total: raw
                .items
                .or(raw.tracks)
                .map(|t| t.total)
                .unwrap_or_default(),
            collaborative: raw.collaborative,
        }
    }
}

/// A playable Spotify catalog track, simplified to what matching/export needs.
#[derive(Debug, Clone, PartialEq)]
pub struct SpotifyTrack {
    pub id: Option<String>,
    pub uri: String,
    pub spotify_url: Option<String>,
    pub name: String,
    pub artists: Vec<String>,
    pub album_artists: Vec<String>,
    pub album: String,
    pub album_id: Option<String>,
    pub album_uri: Option<String>,
    pub album_type: Option<String>,
    pub album_total_tracks: Option<u32>,
    pub album_release_date: Option<String>,
    pub album_release_date_precision: Option<String>,
    pub duration_ms: u32,
    pub disc_number: Option<u32>,
    pub track_number: Option<u32>,
    pub isrc: Option<String>,
    pub explicit: bool,
    pub added_at: Option<String>,
    pub is_playable: Option<bool>,
    pub restriction_reason: Option<String>,
}

/// Playlist / liked-songs item wrapper (the payload can be null; episodes have other
/// types). Playlists use `item` since March 2026; `/me/tracks` still says `track`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RawTrackItem {
    #[serde(default)]
    pub track: Option<serde_json::Value>,
    #[serde(default)]
    pub item: Option<serde_json::Value>,
    #[serde(default)]
    pub added_at: Option<String>,
    /// Wrapper-level local-file flag (post-migration playlists carry it here too).
    #[serde(default)]
    pub is_local: Option<bool>,
}

/// Decode one item's track object. `None` = episode / local file / removed → the
/// caller records it as skipped rather than failing the page.
pub fn simplify(item: &RawTrackItem) -> Option<SpotifyTrack> {
    if item.is_local == Some(true) {
        return None;
    }
    let track = item.item.as_ref().or(item.track.as_ref())?;
    if track
        .get("type")
        .and_then(|t| t.as_str())
        .unwrap_or("track")
        != "track"
    {
        return None;
    }
    if track
        .get("is_local")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return None;
    }
    let uri = string_field(track, "uri")?;
    let name = string_field(track, "name")?;
    let artists = named_array(track.get("artists"));
    let album = track.get("album");
    Some(SpotifyTrack {
        id: string_field(track, "id"),
        uri,
        spotify_url: track
            .pointer("/external_urls/spotify")
            .and_then(|s| s.as_str())
            .map(str::to_owned),
        name,
        artists,
        album_artists: named_array(album.and_then(|a| a.get("artists"))),
        album: album
            .and_then(|a| string_field(a, "name"))
            .unwrap_or_default(),
        album_id: album.and_then(|a| string_field(a, "id")),
        album_uri: album.and_then(|a| string_field(a, "uri")),
        album_type: album.and_then(|a| string_field(a, "album_type")),
        album_total_tracks: album
            .and_then(|a| a.get("total_tracks"))
            .and_then(value_u32),
        album_release_date: album.and_then(|a| string_field(a, "release_date")),
        album_release_date_precision: album.and_then(|a| string_field(a, "release_date_precision")),
        duration_ms: track
            .get("duration_ms")
            .and_then(|d| d.as_u64())
            .unwrap_or(0) as u32,
        disc_number: track.get("disc_number").and_then(value_u32),
        track_number: track.get("track_number").and_then(value_u32),
        isrc: track
            .pointer("/external_ids/isrc")
            .and_then(|i| i.as_str())
            .map(str::to_owned),
        explicit: track
            .get("explicit")
            .and_then(|e| e.as_bool())
            .unwrap_or(false),
        added_at: item.added_at.clone(),
        is_playable: track.get("is_playable").and_then(|v| v.as_bool()),
        restriction_reason: track
            .pointer("/restrictions/reason")
            .and_then(|s| s.as_str())
            .map(str::to_owned),
    })
}

fn string_field(value: &serde_json::Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn value_u32(value: &serde_json::Value) -> Option<u32> {
    value.as_u64().and_then(|n| u32::try_from(n).ok())
}

fn named_array(value: Option<&serde_json::Value>) -> Vec<String> {
    value
        .and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|a| a.get("name").and_then(|n| n.as_str()))
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simplify_decodes_tracks_and_skips_the_rest() {
        let item: RawTrackItem = serde_json::from_value(serde_json::json!({
            "added_at": "2026-07-01T12:00:00Z",
            "track": {
                "type": "track", "id": "abc", "uri": "spotify:track:abc", "name": "ETA",
                "external_urls": {"spotify": "https://open.spotify.com/track/abc"},
                "artists": [{"name": "NewJeans", "id": "artist1", "uri": "spotify:artist:artist1"}],
                "album": {
                    "id": "album1", "uri": "spotify:album:album1", "name": "Get Up",
                    "album_type": "ep", "total_tracks": 6,
                    "release_date": "2023-07-21", "release_date_precision": "day",
                    "artists": [{"name": "NewJeans"}],
                    "external_urls": {"spotify": "https://open.spotify.com/album/album1"}
                },
                "duration_ms": 151000, "explicit": false, "disc_number": 1,
                "track_number": 3, "is_playable": true,
                "external_ids": {"isrc": "KRA402300123"}, "is_local": false,
            }
        }))
        .unwrap();
        let t = simplify(&item).unwrap();
        assert_eq!(t.id.as_deref(), Some("abc"));
        assert_eq!(t.name, "ETA");
        assert_eq!(t.artists, vec!["NewJeans"]);
        assert_eq!(t.album_artists, vec!["NewJeans"]);
        assert_eq!(t.album, "Get Up");
        assert_eq!(t.album_id.as_deref(), Some("album1"));
        assert_eq!(t.album_uri.as_deref(), Some("spotify:album:album1"));
        assert_eq!(t.album_type.as_deref(), Some("ep"));
        assert_eq!(t.album_total_tracks, Some(6));
        assert_eq!(t.album_release_date.as_deref(), Some("2023-07-21"));
        assert_eq!(t.album_release_date_precision.as_deref(), Some("day"));
        assert_eq!(t.disc_number, Some(1));
        assert_eq!(t.track_number, Some(3));
        assert_eq!(t.isrc.as_deref(), Some("KRA402300123"));
        assert_eq!(
            t.spotify_url.as_deref(),
            Some("https://open.spotify.com/track/abc")
        );
        assert_eq!(t.added_at.as_deref(), Some("2026-07-01T12:00:00Z"));
        assert_eq!(t.is_playable, Some(true));

        // Post-March-2026 playlist shape: wrapper `item` + wrapper-level `is_local`.
        let migrated: RawTrackItem = serde_json::from_value(serde_json::json!({
            "is_local": false,
            "item": {
                "type": "track", "uri": "spotify:track:new", "name": "Ditto",
                "artists": [{"name": "NewJeans"}], "album": {"name": "Ditto"},
                "duration_ms": 185506,
            }
        }))
        .unwrap();
        assert_eq!(simplify(&migrated).unwrap().name, "Ditto");
        let local_wrapper: RawTrackItem = serde_json::from_value(serde_json::json!({
            "is_local": true,
            "item": {"type": "track", "uri": "spotify:local:x", "name": "rip"}
        }))
        .unwrap();
        assert!(simplify(&local_wrapper).is_none());

        // Episode → skipped.
        let episode: RawTrackItem = serde_json::from_value(serde_json::json!({
            "track": {"type": "episode", "uri": "spotify:episode:x", "name": "podcast"}
        }))
        .unwrap();
        assert!(simplify(&episode).is_none());

        // Local file → skipped.
        let local: RawTrackItem = serde_json::from_value(serde_json::json!({
            "track": {"type": "track", "uri": "spotify:local:x", "name": "rip", "is_local": true}
        }))
        .unwrap();
        assert!(simplify(&local).is_none());

        // Removed/null track → skipped.
        let null: RawTrackItem =
            serde_json::from_value(serde_json::json!({"track": null})).unwrap();
        assert!(simplify(&null).is_none());
    }
}
