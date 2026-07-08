use std::collections::{BTreeMap, BTreeSet};

use unicode_normalization::UnicodeNormalization;

use super::model::{
    LocalAlbum, LocalAlbumId, LocalArtist, LocalArtistId, LocalTrack, LocalTrackId, normalize_key,
};

pub fn normalize_query_text(text: &str) -> String {
    text.nfkc().flat_map(char::to_lowercase).collect()
}

pub fn fields_match_query<'a>(fields: impl IntoIterator<Item = &'a str>, query: &str) -> bool {
    let needle = normalize_query_text(query.trim());
    if needle.is_empty() {
        return true;
    }
    fields
        .into_iter()
        .any(|field| normalize_query_text(field).contains(&needle))
}

pub fn track_matches_filter(track: &LocalTrack, query: &str) -> bool {
    let title = track.display_title();
    let artist = track.display_artist();
    let album = track.album.as_deref().unwrap_or_default();
    let album_artist = track.album_artist.as_deref().unwrap_or_default();
    let genres = track.genre.join(" ");
    let import_session_id = track.import_session_id.as_deref().unwrap_or_default();
    let import_source_order = track
        .import_source_order
        .map(|order| order.to_string())
        .unwrap_or_default();
    let filename = track
        .path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let path = track.path.to_string_lossy();
    fields_match_query(
        [
            title.as_str(),
            artist.as_str(),
            album,
            album_artist,
            genres.as_str(),
            import_session_id,
            import_source_order.as_str(),
            filename,
            path.as_ref(),
        ],
        query,
    )
}

pub fn albums_from_tracks(tracks: &[LocalTrack]) -> Vec<LocalAlbum> {
    #[derive(Default)]
    struct AlbumBuilder {
        title: String,
        album_artist: String,
        year: Option<i32>,
        track_ids: Vec<LocalTrackId>,
        duration_ms: Option<u64>,
    }

    let mut groups: BTreeMap<(String, String), AlbumBuilder> = BTreeMap::new();
    for track in tracks {
        let Some(album_title) = track
            .album
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let album_artist = track.grouping_album_artist();
        let key = (normalize_key(album_title), normalize_key(&album_artist));
        let group = groups.entry(key).or_insert_with(|| AlbumBuilder {
            title: album_title.to_owned(),
            album_artist,
            year: track.year,
            track_ids: Vec::new(),
            duration_ms: Some(0),
        });
        if group.year.is_none() {
            group.year = track.year;
        }
        group.track_ids.push(track.id.clone());
        group.duration_ms = match (group.duration_ms, track.duration_ms) {
            (Some(total), Some(duration)) => Some(total + duration),
            _ => None,
        };
    }

    let mut albums: Vec<_> = groups
        .into_values()
        .map(|mut group| {
            group.track_ids.sort();
            LocalAlbum {
                id: LocalAlbumId::from_parts(&group.title, &group.album_artist),
                title: group.title,
                album_artist: group.album_artist,
                year: group.year,
                track_count: group.track_ids.len(),
                track_ids: group.track_ids,
                duration_ms: group.duration_ms,
            }
        })
        .collect();
    albums.sort_by(|a, b| {
        a.album_artist
            .to_lowercase()
            .cmp(&b.album_artist.to_lowercase())
            .then_with(|| a.year.cmp(&b.year))
            .then_with(|| a.title.to_lowercase().cmp(&b.title.to_lowercase()))
    });
    albums
}

pub fn artists_from_tracks(tracks: &[LocalTrack], albums: &[LocalAlbum]) -> Vec<LocalArtist> {
    #[derive(Default)]
    struct ArtistBuilder {
        name: String,
        album_ids: BTreeSet<LocalAlbumId>,
        track_ids: BTreeSet<LocalTrackId>,
    }

    let album_by_key: BTreeMap<(String, String), LocalAlbumId> = albums
        .iter()
        .map(|album| {
            (
                (
                    normalize_key(&album.title),
                    normalize_key(&album.album_artist),
                ),
                album.id.clone(),
            )
        })
        .collect();

    let mut groups: BTreeMap<String, ArtistBuilder> = BTreeMap::new();
    for track in tracks {
        let artist_name = track.grouping_album_artist();
        let key = normalize_key(&artist_name);
        let group = groups.entry(key).or_insert_with(|| ArtistBuilder {
            name: artist_name.clone(),
            album_ids: BTreeSet::new(),
            track_ids: BTreeSet::new(),
        });
        group.track_ids.insert(track.id.clone());
        if let Some(album_title) = track
            .album
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let album_key = (normalize_key(album_title), normalize_key(&artist_name));
            if let Some(album_id) = album_by_key.get(&album_key) {
                group.album_ids.insert(album_id.clone());
            }
        }
    }

    let mut artists: Vec<_> = groups
        .into_values()
        .map(|group| LocalArtist {
            id: LocalArtistId::from_name(&group.name),
            name: group.name,
            album_ids: group.album_ids.into_iter().collect(),
            track_ids: group.track_ids.into_iter().collect(),
        })
        .collect();
    artists.sort_by_key(|artist| artist.name.to_lowercase());
    artists
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::local::model::LocalTrack;

    fn track(path: &str, title: &str, artist: &[&str]) -> LocalTrack {
        let mut track = LocalTrack::untagged(PathBuf::from(path), 100, 10);
        track.title = title.to_owned();
        track.artist = artist.iter().map(|s| (*s).to_owned()).collect();
        track.duration_ms = Some(60_000);
        track
    }

    #[test]
    fn albums_group_by_album_title_and_album_artist() {
        let mut a = track("/m/a.flac", "A", &["Track Artist A"]);
        a.album = Some("Discovery".to_owned());
        a.album_artist = Some("Daft Punk".to_owned());
        a.year = Some(2001);
        let mut b = track("/m/b.flac", "B", &["Track Artist B"]);
        b.album = Some("Discovery".to_owned());
        b.album_artist = Some("Daft Punk".to_owned());
        b.year = Some(2001);
        let mut c = track("/m/c.flac", "C", &["Other"]);
        c.album = Some("Discovery".to_owned());
        c.album_artist = Some("Other".to_owned());

        let albums = albums_from_tracks(&[a, b, c]);

        assert_eq!(albums.len(), 2);
        assert_eq!(albums[0].album_artist, "Daft Punk");
        assert_eq!(albums[0].track_count, 2);
        assert_eq!(albums[0].duration_ms, Some(120_000));
        assert_eq!(albums[1].album_artist, "Other");
    }

    #[test]
    fn albums_use_track_artist_when_album_artist_is_missing() {
        let mut a = track("/m/a.flac", "A", &["IU"]);
        a.album = Some("Palette".to_owned());
        let albums = albums_from_tracks(&[a]);

        assert_eq!(albums[0].album_artist, "IU");
    }

    #[test]
    fn artists_prefer_album_artist_but_keep_track_ids() {
        let mut a = track("/m/a.flac", "A", &["Guest A"]);
        a.album = Some("Compilation".to_owned());
        a.album_artist = Some("Various Artists".to_owned());
        let mut b = track("/m/b.flac", "B", &["Guest B"]);
        b.album = Some("Compilation".to_owned());
        b.album_artist = Some("Various Artists".to_owned());
        let albums = albums_from_tracks(&[a.clone(), b.clone()]);

        let artists = artists_from_tracks(&[a, b], &albums);

        assert_eq!(artists.len(), 1);
        assert_eq!(artists[0].name, "Various Artists");
        assert_eq!(artists[0].track_ids.len(), 2);
        assert_eq!(artists[0].album_ids.len(), 1);
    }

    #[test]
    fn filter_matches_case_insensitively_across_fields() {
        let mut track = track("/music/IU/Palette.flac", "Palette", &["IU"]);
        track.album = Some("Palette".to_owned());
        track.genre = vec!["K-Pop".to_owned()];

        assert!(track_matches_filter(&track, "palette"));
        assert!(track_matches_filter(&track, "iu"));
        assert!(track_matches_filter(&track, "k-pop"));
        assert!(track_matches_filter(&track, "music/iu"));
        assert!(!track_matches_filter(&track, "missing"));
    }

    #[test]
    fn filter_matches_import_session_metadata() {
        let mut track = track("/music/imported.m4a", "Imported", &["Artist"]);
        track.import_session_id = Some("sp2yt-20260708-abcd".to_owned());
        track.import_source_order = Some(17);

        assert!(track_matches_filter(&track, "20260708"));
        assert!(track_matches_filter(&track, "17"));
    }

    #[test]
    fn filter_uses_nfkc_width_normalization() {
        let track = track("/music/fullwidth.mp3", "ＡＢＣ ｶﾌｪ", &["Artist"]);

        assert!(track_matches_filter(&track, "abc カフェ"));
    }

    #[test]
    fn filter_matches_cjk_metadata() {
        let track = track("/music/iu.mp3", "좋은 날", &["아이유"]);

        assert!(track_matches_filter(&track, "아이유"));
        assert!(track_matches_filter(&track, "좋은"));
    }
}
