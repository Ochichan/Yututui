//! CSV playlist codec, Exportify-compatible.
//!
//! Export header: the first six columns carry Exportify's exact names and semantics
//! (artists joined `", "`, duration in ms; Track URI/ISRC empty on YTM-sourced rows),
//! plus a trailing `"YouTube ID"` column of our own — extra columns don't bother
//! Exportify-style consumers, and reading it back gives lossless-enough restores (a
//! present id skips matching entirely). Import locates columns by header name, so plain
//! Exportify files and ours both parse regardless of column order.

use std::path::Path;

use anyhow::{Context, Result, bail};

use super::matching::TrackInput;
use crate::api::Song;

pub const HEADER: [&str; 7] = [
    "Track URI",
    "Track Name",
    "Artist Name(s)",
    "Album Name",
    "Duration (ms)",
    "ISRC",
    "YouTube ID",
];

/// Write YTM songs as CSV. Plain `std::fs` — exports are user documents, not private state.
pub fn write_songs(path: &Path, songs: &[Song]) -> Result<()> {
    let mut writer = csv::Writer::from_path(path)
        .with_context(|| format!("creating {}", path.display()))?;
    writer.write_record(HEADER)?;
    for song in songs {
        let duration_ms = song
            .duration_secs
            .or_else(|| crate::streaming::candidate::parse_duration_secs(&song.duration))
            .map(|s| (u64::from(s) * 1000).to_string())
            .unwrap_or_default();
        writer.write_record([
            "", // Track URI: unknown on the YTM side
            song.title.as_str(),
            song.artist.as_str(),
            song.album.as_deref().unwrap_or(""),
            duration_ms.as_str(),
            "", // ISRC: unknown on the YTM side
            song.youtube_id().unwrap_or(""),
        ])?;
    }
    writer.flush()?;
    Ok(())
}

/// Read a playlist CSV (Exportify or ours) into match inputs.
pub fn read_tracks(path: &Path) -> Result<Vec<TrackInput>> {
    let mut reader = csv::Reader::from_path(path)
        .with_context(|| format!("reading {}", path.display()))?;
    let headers = reader.headers().context("CSV has no header row")?.clone();
    let col = |name: &str| -> Option<usize> {
        headers
            .iter()
            .position(|h| h.trim().eq_ignore_ascii_case(name))
    };
    let (Some(title_col), Some(artist_col)) = (col("Track Name"), col("Artist Name(s)")) else {
        bail!(
            "unrecognized CSV header — expected at least \"Track Name\" and \"Artist Name(s)\" \
             columns (Exportify format, or a ytm-tui export)"
        );
    };
    let album_col = col("Album Name");
    let duration_col = col("Duration (ms)");
    let isrc_col = col("ISRC");
    let uri_col = col("Track URI");
    let ytid_col = col("YouTube ID");

    let mut out = Vec::new();
    for (row_no, record) in reader.records().enumerate() {
        let record = record.with_context(|| format!("CSV row {}", row_no + 2))?;
        let field = |idx: Option<usize>| -> Option<String> {
            idx.and_then(|i| record.get(i))
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_owned)
        };
        let Some(title) = field(Some(title_col)) else {
            continue; // blank row
        };
        let artists: Vec<String> = field(Some(artist_col))
            .map(|a| a.split(", ").map(str::to_owned).collect())
            .unwrap_or_default();
        out.push(TrackInput {
            title,
            artists,
            album: field(album_col),
            duration_secs: field(duration_col)
                .and_then(|ms| ms.parse::<u64>().ok())
                .map(|ms| (ms / 1000) as u32)
                .filter(|s| *s > 0),
            isrc: field(isrc_col),
            source_key: field(uri_col).unwrap_or_else(|| format!("csv-row-{}", row_no + 2)),
            known_video_id: field(ytid_col),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_csv(name: &str) -> std::path::PathBuf {
        let mut bytes = [0u8; 6];
        getrandom::fill(&mut bytes).unwrap();
        let suffix = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
        std::env::temp_dir().join(format!(
            "ytm-tui-csv-{name}-{}-{suffix}.csv",
            std::process::id()
        ))
    }

    #[test]
    fn parses_a_verbatim_exportify_sample() {
        let path = temp_csv("exportify");
        // Exportify's real column set is wider; order-independent lookup must cope.
        std::fs::write(
            &path,
            "\
\"Track URI\",\"Track Name\",\"Artist Name(s)\",\"Album Name\",\"Album Artist Name(s)\",\"Duration (ms)\",\"ISRC\",\"Added At\"
spotify:track:2plbrEY,\"Ditto\",\"NewJeans\",\"Ditto\",\"NewJeans\",185506,KRA402200607,2023-01-01
spotify:track:0Q5VnK2,\"Dynamite, Pt. 2\",\"BTS, Someone\",\"BE\",\"BTS\",199054,QM7282029296,2023-01-02
",
        )
        .unwrap();
        let tracks = read_tracks(&path).unwrap();
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].title, "Ditto");
        assert_eq!(tracks[0].artists, vec!["NewJeans"]);
        assert_eq!(tracks[0].duration_secs, Some(185));
        assert_eq!(tracks[0].isrc.as_deref(), Some("KRA402200607"));
        assert_eq!(tracks[0].source_key, "spotify:track:2plbrEY");
        assert!(tracks[0].known_video_id.is_none());
        // Quoted comma inside a title survives; ", "-joined artists split.
        assert_eq!(tracks[1].title, "Dynamite, Pt. 2");
        assert_eq!(tracks[1].artists, vec!["BTS", "Someone"]);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn our_export_round_trips_with_fast_path_ids() {
        let path = temp_csv("roundtrip");
        let mut song = Song::from_search(
            "dQw4w9WgXcQ",
            "네버 갓 어 유",
            "아티스트, 게스트",
            "3:32",
            Some("앨범".to_owned()),
        );
        song.duration_secs = Some(212);
        write_songs(&path, &[song]).unwrap();

        let tracks = read_tracks(&path).unwrap();
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].title, "네버 갓 어 유");
        assert_eq!(tracks[0].album.as_deref(), Some("앨범"));
        assert_eq!(tracks[0].duration_secs, Some(212));
        assert_eq!(tracks[0].known_video_id.as_deref(), Some("dQw4w9WgXcQ"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn unrecognized_header_is_a_clear_error() {
        let path = temp_csv("bad");
        std::fs::write(&path, "a,b,c\n1,2,3\n").unwrap();
        let err = read_tracks(&path).unwrap_err().to_string();
        assert!(err.contains("Track Name"), "{err}");
        let _ = std::fs::remove_file(path);
    }
}
