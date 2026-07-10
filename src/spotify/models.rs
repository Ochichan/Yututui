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
    /// Spotify's offset envelopes include these on every page. Keeping them lets the
    /// client reject a repeated/overlapping continuation before duplicate rows grow the
    /// transfer input indefinitely.
    #[serde(default)]
    pub offset: u32,
    #[serde(default)]
    pub limit: u32,
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
    /// Human-readable owner label retained for existing list output.
    pub owner: String,
    /// Stable owner identifier used for authorization-sensitive operations.
    pub owner_id: Option<String>,
    pub total: u32,
    pub collaborative: bool,
    /// Snapshot returned by Spotify for optimistic concurrency checks.
    pub snapshot_id: Option<String>,
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
    #[serde(default)]
    pub snapshot_id: Option<String>,
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
        let (owner, owner_id) = raw.owner.map_or_else(
            || (String::new(), None),
            |owner| {
                let owner_id = owner.id.filter(|id| !id.is_empty());
                let label = owner
                    .display_name
                    .filter(|name| !name.is_empty())
                    .or_else(|| owner_id.clone())
                    .unwrap_or_default();
                (label, owner_id)
            },
        );
        Self {
            id: raw.id,
            name: raw.name,
            owner,
            owner_id,
            total: raw
                .items
                .or(raw.tracks)
                .map(|t| t.total)
                .unwrap_or_default(),
            collaborative: raw.collaborative,
            snapshot_id: raw.snapshot_id.filter(|id| !id.is_empty()),
        }
    }
}

/// One Spotify album artwork image.
#[derive(Debug, Clone, PartialEq)]
pub struct SpotifyImage {
    pub url: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

/// A playable Spotify catalog track, simplified to what matching/export needs.
#[derive(Debug, Clone, PartialEq)]
pub struct SpotifyTrack {
    pub id: Option<String>,
    pub uri: String,
    pub spotify_url: Option<String>,
    pub name: String,
    pub artists: Vec<String>,
    pub artist_ids: Vec<String>,
    pub album_artists: Vec<String>,
    pub album_artist_ids: Vec<String>,
    pub album: String,
    pub album_id: Option<String>,
    pub album_uri: Option<String>,
    pub album_url: Option<String>,
    pub album_type: Option<String>,
    pub album_total_tracks: Option<u32>,
    pub album_release_date: Option<String>,
    pub album_release_date_precision: Option<String>,
    pub album_images: Vec<SpotifyImage>,
    pub duration_ms: u32,
    pub disc_number: Option<u32>,
    pub track_number: Option<u32>,
    pub isrc: Option<String>,
    pub explicit: bool,
    pub added_at: Option<String>,
    pub is_playable: Option<bool>,
    pub restriction_reason: Option<String>,
}

impl SpotifyTrack {
    pub fn best_album_image_url(&self) -> Option<String> {
        self.album_images
            .iter()
            .max_by_key(|image| {
                image
                    .width
                    .unwrap_or(0)
                    .saturating_mul(image.height.unwrap_or(0))
            })
            .or_else(|| self.album_images.first())
            .map(|image| image.url.clone())
    }
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

/// One destination playlist row, without discarding unsupported Spotify item types.
///
/// Exact-mirror previews need the row count and order even when the row is an episode,
/// local file, unknown future type, or a removed/null item. `uri` and `kind` therefore
/// remain optional instead of filtering the row out as [`simplify`] does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpotifyPlaylistItemRef {
    pub uri: Option<String>,
    pub kind: Option<String>,
    pub is_local: bool,
}

pub fn playlist_item_ref(item: &RawTrackItem) -> SpotifyPlaylistItemRef {
    let value = item.item.as_ref().or(item.track.as_ref());
    let uri = value.and_then(|entry| string_field(entry, "uri"));
    let kind = value.and_then(|entry| string_field(entry, "type"));
    let is_local = item.is_local == Some(true)
        || value
            .and_then(|entry| entry.get("is_local"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        || uri
            .as_deref()
            .is_some_and(|uri| uri.starts_with("spotify:local:"));
    SpotifyPlaylistItemRef {
        uri,
        kind,
        is_local,
    }
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
    let artist_ids = id_array(track.get("artists"));
    let album = track.get("album");
    let album_artists_value = album.and_then(|a| a.get("artists"));
    Some(SpotifyTrack {
        id: string_field(track, "id"),
        uri,
        spotify_url: track
            .pointer("/external_urls/spotify")
            .and_then(|s| s.as_str())
            .map(str::to_owned),
        name,
        artists,
        artist_ids,
        album_artists: named_array(album_artists_value),
        album_artist_ids: id_array(album_artists_value),
        album: album
            .and_then(|a| string_field(a, "name"))
            .unwrap_or_default(),
        album_id: album.and_then(|a| string_field(a, "id")),
        album_uri: album.and_then(|a| string_field(a, "uri")),
        album_url: album
            .and_then(|a| a.pointer("/external_urls/spotify"))
            .and_then(|s| s.as_str())
            .map(str::to_owned),
        album_type: album.and_then(|a| string_field(a, "album_type")),
        album_total_tracks: album
            .and_then(|a| a.get("total_tracks"))
            .and_then(value_u32),
        album_release_date: album.and_then(|a| string_field(a, "release_date")),
        album_release_date_precision: album.and_then(|a| string_field(a, "release_date_precision")),
        album_images: album
            .and_then(|a| image_array(a.get("images")))
            .unwrap_or_default(),
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

fn id_array(value: Option<&serde_json::Value>) -> Vec<String> {
    value
        .and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|a| a.get("id").and_then(|n| n.as_str()))
                .filter(|id| !id.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn image_array(value: Option<&serde_json::Value>) -> Option<Vec<SpotifyImage>> {
    Some(
        value?
            .as_array()?
            .iter()
            .filter_map(|image| {
                let url = image.get("url")?.as_str()?.to_owned();
                Some(SpotifyImage {
                    url,
                    width: image.get("width").and_then(value_u32),
                    height: image.get("height").and_then(value_u32),
                })
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playlist_keeps_owner_id_label_and_snapshot_separately() {
        let raw: RawPlaylist = serde_json::from_value(serde_json::json!({
            "id": "playlist-id",
            "name": "Roadtrip",
            "owner": {"id": "stable-owner-id", "display_name": "DJ Test"},
            "items": {"total": 23},
            "collaborative": true,
            "snapshot_id": "snapshot-1"
        }))
        .unwrap();

        let playlist = SpotifyPlaylist::from(raw);
        assert_eq!(playlist.owner, "DJ Test");
        assert_eq!(playlist.owner_id.as_deref(), Some("stable-owner-id"));
        assert_eq!(playlist.snapshot_id.as_deref(), Some("snapshot-1"));
        assert_eq!(playlist.total, 23);
        assert!(playlist.collaborative);
    }

    #[test]
    fn playlist_item_refs_preserve_track_episode_local_and_null_rows() {
        let rows: Vec<RawTrackItem> = serde_json::from_value(serde_json::json!([
            {"item": {"type": "track", "uri": "spotify:track:one"}},
            {"item": {"type": "episode", "uri": "spotify:episode:two"}},
            {"is_local": true, "item": {
                "type": "track", "uri": "spotify:local:artist:album:title:123"
            }},
            {"item": null}
        ]))
        .unwrap();

        let refs: Vec<SpotifyPlaylistItemRef> = rows.iter().map(playlist_item_ref).collect();
        assert_eq!(refs.len(), 4);
        assert_eq!(refs[0].uri.as_deref(), Some("spotify:track:one"));
        assert_eq!(refs[0].kind.as_deref(), Some("track"));
        assert!(!refs[0].is_local);
        assert_eq!(refs[1].uri.as_deref(), Some("spotify:episode:two"));
        assert_eq!(refs[1].kind.as_deref(), Some("episode"));
        assert!(!refs[1].is_local);
        assert_eq!(
            refs[2].uri.as_deref(),
            Some("spotify:local:artist:album:title:123")
        );
        assert_eq!(refs[2].kind.as_deref(), Some("track"));
        assert!(refs[2].is_local);
        assert_eq!(refs[3].uri, None);
        assert_eq!(refs[3].kind, None);
        assert!(!refs[3].is_local);
    }

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
                    "images": [
                        {"url": "https://i.scdn.co/image/small", "width": 64, "height": 64},
                        {"url": "https://i.scdn.co/image/large", "width": 640, "height": 640}
                    ],
                    "artists": [{"name": "NewJeans", "id": "artist1"}],
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
        assert_eq!(t.artist_ids, vec!["artist1"]);
        assert_eq!(t.album_artists, vec!["NewJeans"]);
        assert_eq!(t.album_artist_ids, vec!["artist1"]);
        assert_eq!(t.album, "Get Up");
        assert_eq!(t.album_id.as_deref(), Some("album1"));
        assert_eq!(t.album_uri.as_deref(), Some("spotify:album:album1"));
        assert_eq!(
            t.album_url.as_deref(),
            Some("https://open.spotify.com/album/album1")
        );
        assert_eq!(t.album_type.as_deref(), Some("ep"));
        assert_eq!(t.album_total_tracks, Some(6));
        assert_eq!(t.album_release_date.as_deref(), Some("2023-07-21"));
        assert_eq!(t.album_release_date_precision.as_deref(), Some("day"));
        assert_eq!(
            t.best_album_image_url().as_deref(),
            Some("https://i.scdn.co/image/large")
        );
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
