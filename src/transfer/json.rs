//! The lossless JSON playlist envelope (`ytm-tui/playlist` v1): full `Song` fidelity,
//! so a backup restores without any re-matching. Versioned so future shape changes stay
//! detectable instead of silently misparsing.

use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::matching::TrackInput;
use crate::api::Song;

pub const FORMAT: &str = "ytm-tui/playlist";
pub const VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaylistFile {
    pub format: String,
    pub version: u32,
    pub name: String,
    /// Where it came from (`ytm:<id>` / `local:<key>` / `likes`), informational.
    #[serde(default)]
    pub source: String,
    pub exported_at_unix: i64,
    pub tracks: Vec<Song>,
}

impl PlaylistFile {
    pub fn new(name: String, source: String, tracks: Vec<Song>) -> Self {
        Self {
            format: FORMAT.to_owned(),
            version: VERSION,
            name,
            source,
            exported_at_unix: crate::signals::unix_now(),
            tracks,
        }
    }

    /// Restore inputs: every track with a YouTube identity takes the fast path.
    pub fn to_track_inputs(&self) -> Vec<TrackInput> {
        self.tracks.iter().map(TrackInput::from_song).collect()
    }
}

/// Plain `std::fs` — exports are user documents, not private state.
pub fn write_playlist(path: &Path, file: &PlaylistFile) -> Result<()> {
    let json = serde_json::to_string_pretty(file)?;
    std::fs::write(path, json).with_context(|| format!("writing {}", path.display()))
}

pub fn read_playlist(path: &Path) -> Result<PlaylistFile> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let file: PlaylistFile = serde_json::from_str(&text)
        .with_context(|| format!("{} is not a ytm-tui playlist file", path.display()))?;
    if file.format != FORMAT {
        bail!(
            "{} has format `{}` (expected `{FORMAT}`)",
            path.display(),
            file.format
        );
    }
    if file.version != VERSION {
        bail!(
            "{} is playlist-file version {} — this build reads version {VERSION}",
            path.display(),
            file.version
        );
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_json(name: &str) -> std::path::PathBuf {
        let mut bytes = [0u8; 6];
        getrandom::fill(&mut bytes).unwrap();
        let suffix = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
        std::env::temp_dir().join(format!(
            "ytm-tui-json-{name}-{}-{suffix}.json",
            std::process::id()
        ))
    }

    #[test]
    fn envelope_round_trips_full_song_fidelity() {
        let path = temp_json("rt");
        let song = Song::from_search(
            "dQw4w9WgXcQ",
            "Title",
            "Artist",
            "3:45",
            Some("Album".to_owned()),
        );
        let file = PlaylistFile::new("Roadtrip".to_owned(), "ytm:PL123".to_owned(), vec![song]);
        write_playlist(&path, &file).unwrap();

        let back = read_playlist(&path).unwrap();
        assert_eq!(back.name, "Roadtrip");
        assert_eq!(back.tracks.len(), 1);
        assert_eq!(back.tracks[0].album.as_deref(), Some("Album"));
        assert_eq!(back.tracks[0].duration_secs, Some(225));
        let inputs = back.to_track_inputs();
        assert_eq!(inputs[0].known_video_id.as_deref(), Some("dQw4w9WgXcQ"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn unknown_versions_are_rejected() {
        let path = temp_json("ver");
        std::fs::write(
            &path,
            format!(r#"{{"format":"{FORMAT}","version":99,"name":"x","exported_at_unix":0,"tracks":[]}}"#),
        )
        .unwrap();
        let err = read_playlist(&path).unwrap_err().to_string();
        assert!(err.contains("version 99"), "{err}");
        let _ = std::fs::remove_file(&path);

        std::fs::write(
            &path,
            r#"{"format":"other","version":1,"name":"x","exported_at_unix":0,"tracks":[]}"#,
        )
        .unwrap();
        assert!(read_playlist(&path).is_err());
        let _ = std::fs::remove_file(path);
    }
}
