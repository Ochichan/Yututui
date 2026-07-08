//! Metadata extraction for Local Deck tracks.

use std::path::PathBuf;

use lofty::file::{AudioFile, TaggedFileExt};
use lofty::tag::{Accessor, ItemKey};

use super::model::LocalTrack;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataRead {
    pub track: LocalTrack,
    pub warning: Option<String>,
}

pub fn read_track(path: PathBuf, file_size: u64, modified_at: i64) -> MetadataRead {
    let mut track = LocalTrack::untagged(path.clone(), file_size, modified_at);
    let tagged = match lofty::read_from_path(&path) {
        Ok(tagged) => tagged,
        Err(error) => {
            return MetadataRead {
                track,
                warning: Some(error.to_string()),
            };
        }
    };

    let props = tagged.properties();
    let duration = props.duration();
    if !duration.is_zero() {
        track.duration_ms = Some(duration.as_millis().min(u128::from(u64::MAX)) as u64);
    }
    track.bitrate = props.audio_bitrate().or_else(|| props.overall_bitrate());
    track.sample_rate = props.sample_rate();

    if let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) {
        if let Some(title) = clean_text(tag.title()) {
            track.title = title;
        }
        if let Some(artist) = clean_text(tag.artist()) {
            track.artist = split_people(&artist);
        }
        track.album = clean_text(tag.album());
        track.album_artist = tag
            .get_string(ItemKey::AlbumArtist)
            .or_else(|| tag.get_string(ItemKey::AlbumArtists))
            .and_then(clean_str);
        if let Some(genre) = clean_text(tag.genre()) {
            track.genre = split_people(&genre);
        }
        track.year = tag.date().map(|date| i32::from(date.year));
        track.track_no = tag.track();
        track.disc_no = tag.disk();
        if !tag.pictures().is_empty() {
            track.embedded_art_key = Some(format!("embedded:{}", track.id.as_str()));
        }
    }

    MetadataRead {
        track,
        warning: None,
    }
}

fn clean_text(text: Option<std::borrow::Cow<'_, str>>) -> Option<String> {
    text.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty())
}

fn clean_str(text: &str) -> Option<String> {
    let text = text.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_owned())
    }
}

fn split_people(text: &str) -> Vec<String> {
    text.split(&[';', '/', ','][..])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_audio_still_returns_untagged_track_with_warning() {
        let path = PathBuf::from("/tmp/No Tags.mp3");
        let read = read_track(path.clone(), 99, 123);

        assert_eq!(read.track.path, path);
        assert_eq!(read.track.title, "No Tags");
        assert_eq!(read.track.file_size, 99);
        assert!(read.warning.is_some());
    }

    #[test]
    fn split_people_handles_common_separators() {
        assert_eq!(
            split_people("A; B / C, D"),
            vec![
                "A".to_owned(),
                "B".to_owned(),
                "C".to_owned(),
                "D".to_owned()
            ]
        );
    }
}
