//! Radio stream recorder (a Shortwave-style feature).
//!
//! While an internet-radio station plays, mpv is already pulling one continuous ICY
//! stream. We tap **that same connection** with mpv's `stream-record` property (no second
//! network connection, no re-encode, no extra dependency) and rotate the output file on
//! every ICY title change, so a continuous broadcast is split into per-track files.
//!
//! This module holds the **pure** pieces — the config-facing [`RecordingMode`], the
//! in-memory [`RecorderState`] machine data, and filename/codec helpers. The blocking disk
//! work (copy + tag) lives in [`job`]; the state-machine transitions that drive mpv live in
//! `crate::app::recorder_reducer`.

pub mod job;

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::t;

/// What the recorder does with each track it sees while a station plays. Serialized into
/// `config.json` under `recording.mode`. Defaults to [`RecordingMode::Nothing`] — recording
/// is strictly opt-in, nothing is written until the user turns it on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordingMode {
    /// Auto-save every completed track straight to the recordings folder.
    Everything,
    /// Record every track to a temp area; the user saves/discards each from the browser.
    Decide,
    /// Off — nothing is recorded. The default: recording is opt-in, so a fresh install writes
    /// nothing until the user chooses a mode.
    #[default]
    Nothing,
}

impl RecordingMode {
    /// Cycle order for the settings selector (Off → Decide → Save all → Off).
    pub const ALL: [RecordingMode; 3] = [
        RecordingMode::Nothing,
        RecordingMode::Decide,
        RecordingMode::Everything,
    ];

    /// Bilingual label for the settings selector / button summary.
    pub fn label(self) -> String {
        match self {
            RecordingMode::Everything => t!("Save all tracks", "모든 트랙 저장").to_owned(),
            RecordingMode::Decide => t!("Decide per track", "트랙별 선택").to_owned(),
            RecordingMode::Nothing => t!("Off", "끄기").to_owned(),
        }
    }

    /// The next mode in cycle order (`forward` steps toward "Save all", else back).
    pub fn cycled(self, forward: bool) -> RecordingMode {
        let idx = Self::ALL.iter().position(|m| *m == self).unwrap_or(0);
        let n = Self::ALL.len();
        let next = if forward {
            (idx + 1) % n
        } else {
            (idx + n - 1) % n
        };
        Self::ALL[next]
    }

    pub fn is_off(self) -> bool {
        matches!(self, RecordingMode::Nothing)
    }
}

/// The lifecycle state of one track the recorder has seen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordingState {
    /// Currently being written by mpv (the open segment).
    Recording,
    /// Finished at a title boundary, temp file kept, awaiting a save decision (Decide mode).
    Recorded,
    /// Finished because it hit the max-duration cap (kept, temp file valid).
    RecordedReachedMaxDuration,
    /// Copied to the recordings folder.
    Saved,
    /// Dropped because it was shorter than the minimum duration.
    DiscardedBelowMinDuration,
    /// The user discarded it (or it was cut mid-song by stop/leave).
    DiscardedCancelled,
}

impl RecordingState {
    /// Completed with a kept temp file (eligible for saving).
    pub fn is_recorded(self) -> bool {
        matches!(
            self,
            RecordingState::Recorded | RecordingState::RecordedReachedMaxDuration
        )
    }

    /// Bilingual one-line status for the recordings browser.
    pub fn label(self) -> String {
        match self {
            RecordingState::Recording => t!("Recording…", "녹음 중…").to_owned(),
            RecordingState::Recorded => t!("Ready", "준비됨").to_owned(),
            RecordingState::RecordedReachedMaxDuration => {
                t!("Ready (max length)", "준비됨 (최대 길이)").to_owned()
            }
            RecordingState::Saved => t!("Saved", "저장됨").to_owned(),
            RecordingState::DiscardedBelowMinDuration => t!("Too short", "너무 짧음").to_owned(),
            RecordingState::DiscardedCancelled => t!("Discarded", "버림").to_owned(),
        }
    }
}

/// The segment mpv is currently writing. Not part of the history until it is finalized.
pub struct OpenSegment {
    /// Stable id (also used to build the temp filename and to correlate save jobs).
    pub id: u64,
    /// Temp file mpv is writing to.
    pub temp_path: PathBuf,
    /// ICY title fields that were current *during* this segment (used for the filename/tags).
    pub title: Option<String>,
    pub artist: Option<String>,
    pub raw: String,
    /// Station display name at open time (used for the album tag).
    pub station: Option<String>,
    /// Wall clock at open; drives the min/max-duration filters (monotonic, survives stalls).
    pub started_at: Instant,
    /// Joined mid-song (first track after tuning in, or a mid-song reconnect) → always dropped.
    pub incomplete: bool,
    /// Container extension chosen at open time from the stream codec (no leading dot).
    pub ext: &'static str,
}

/// A finalized-and-kept track: a Decide-mode history row, or an Everything-mode auto-save.
#[derive(Debug, Clone)]
pub struct RecordedTrack {
    pub id: u64,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub raw: String,
    /// Station display name (album tag).
    pub station: Option<String>,
    /// Temp file; stays valid until the track is Saved-copied or Discarded.
    pub temp_path: PathBuf,
    pub ext: &'static str,
    pub duration_secs: u32,
    pub state: RecordingState,
    /// Set once the track has been copied into the recordings folder.
    pub final_path: Option<PathBuf>,
}

impl RecordedTrack {
    /// The label shown in the recordings browser ("Artist - Title", else the raw ICY title).
    pub fn display(&self) -> String {
        let base = self.raw.trim();
        if base.is_empty() {
            match (&self.artist, &self.title) {
                (Some(a), Some(t)) => format!("{a} - {t}"),
                (_, Some(t)) => t.clone(),
                _ => "—".to_owned(),
            }
        } else {
            base.to_owned()
        }
    }
}

/// All volatile recorder state. Lives on `App`; nothing here is persisted (only the config
/// is). The temp dir is wiped at startup, so in-progress/undecided recordings never survive a
/// restart — only tracks explicitly copied into the recordings folder persist.
pub struct RecorderState {
    /// Whether this mpv build supports the `stream-record` property (probed at startup).
    pub supported: bool,
    /// The segment mpv is writing right now, if any.
    pub current: Option<OpenSegment>,
    /// False until the first real title of this stream session; the pre-first-title segment
    /// is always incomplete (we joined mid-song) and dropped.
    pub saw_first_title: bool,
    /// Monotonic counter for unique temp filenames + track ids.
    pub temp_seq: u64,
    /// Recent finalized tracks, most-recent at the front, bounded by `past_tracks_count`.
    pub history: VecDeque<RecordedTrack>,
    /// `<cache>/recordings` — ephemeral temp files, wiped at startup.
    pub temp_dir: PathBuf,
}

impl Default for RecorderState {
    fn default() -> Self {
        Self {
            supported: false,
            current: None,
            saw_first_title: false,
            temp_seq: 0,
            history: VecDeque::new(),
            temp_dir: PathBuf::new(),
        }
    }
}

impl RecorderState {
    /// A segment is currently being written to disk.
    pub fn is_recording(&self) -> bool {
        self.current.is_some()
    }

    /// Allocate the next unique id + temp path for an extension (no leading dot).
    pub fn next_temp(&mut self, ext: &'static str) -> (u64, PathBuf) {
        self.temp_seq += 1;
        let id = self.temp_seq;
        let path = self.temp_dir.join(format!("rec-{id}.{ext}"));
        (id, path)
    }
}

/// Pick the passthrough container extension (no leading dot) from mpv's observed codec /
/// container. Common ICY stations are MP3 or AAC. Anything unrecognized (HLS, mp4, …) falls
/// back to Matroska (`mkv`), which stream-copies any codec but cannot be tagged.
pub fn codec_to_ext(audio_codec: Option<&str>, file_format: Option<&str>) -> &'static str {
    let codec = audio_codec.unwrap_or_default().to_ascii_lowercase();
    if codec.contains("mp3") {
        return "mp3";
    }
    if codec.contains("aac") {
        return "aac";
    }
    if codec.contains("vorbis") {
        return "ogg";
    }
    if codec.contains("opus") {
        return "opus";
    }
    if codec.contains("flac") {
        return "flac";
    }
    let fmt = file_format.unwrap_or_default().to_ascii_lowercase();
    if fmt.contains("mp3") {
        return "mp3";
    }
    if fmt.contains("aac") || fmt.contains("adts") {
        return "aac";
    }
    if fmt.contains("ogg") || fmt.contains("vorbis") {
        return "ogg";
    }
    if fmt.contains("opus") {
        return "opus";
    }
    if fmt.contains("flac") {
        return "flac";
    }
    "mkv"
}

/// Whether `lofty` can write tags for this container (everything except the mkv fallback).
pub fn ext_is_taggable(ext: &str) -> bool {
    matches!(ext, "mp3" | "aac" | "ogg" | "opus" | "flac")
}

/// The saved-file base name (no extension) from the ICY fields. Uses the verbatim ICY title
/// (`raw`, typically "Artist - Title") like Shortwave, sanitized for the filesystem.
pub fn track_filename_base(title: Option<&str>, raw: &str) -> String {
    let base = if raw.trim().is_empty() {
        title.unwrap_or("")
    } else {
        raw
    };
    sanitize_track_filename(base)
}

/// Make an arbitrary ICY title safe as a single path component: strip control chars and
/// path separators / Windows-reserved chars, collapse whitespace, trim leading/trailing dots
/// and spaces, and cap the length. Prevents path traversal from an untrusted stream title.
pub fn sanitize_track_filename(base: &str) -> String {
    let mut cleaned = String::with_capacity(base.len());
    for ch in base.chars() {
        let bad = ch.is_control()
            || matches!(
                ch,
                '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0'
            );
        cleaned.push(if bad { ' ' } else { ch });
    }
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = collapsed.trim_matches(|c: char| c == '.' || c == ' ');
    let capped: String = trimmed.chars().take(180).collect();
    let capped = capped
        .trim_matches(|c: char| c == '.' || c == ' ')
        .to_owned();
    if capped.is_empty() {
        "Untitled".to_owned()
    } else {
        capped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_cycles_both_ways_and_wraps() {
        assert_eq!(RecordingMode::Nothing.cycled(true), RecordingMode::Decide);
        assert_eq!(
            RecordingMode::Decide.cycled(true),
            RecordingMode::Everything
        );
        assert_eq!(
            RecordingMode::Everything.cycled(true),
            RecordingMode::Nothing
        );
        assert_eq!(
            RecordingMode::Nothing.cycled(false),
            RecordingMode::Everything
        );
        assert_eq!(RecordingMode::default(), RecordingMode::Nothing);
    }

    #[test]
    fn codec_maps_common_streams() {
        assert_eq!(codec_to_ext(Some("mp3"), None), "mp3");
        assert_eq!(codec_to_ext(Some("aac"), None), "aac");
        assert_eq!(codec_to_ext(Some("aac_latm"), None), "aac");
        assert_eq!(codec_to_ext(Some("vorbis"), None), "ogg");
        assert_eq!(codec_to_ext(Some("opus"), None), "opus");
        assert_eq!(codec_to_ext(Some("flac"), None), "flac");
        // Fall back on file-format when the codec is unknown.
        assert_eq!(codec_to_ext(None, Some("mp3")), "mp3");
        assert_eq!(codec_to_ext(None, Some("hls")), "mkv");
        assert_eq!(codec_to_ext(None, None), "mkv");
        assert!(ext_is_taggable("mp3"));
        assert!(!ext_is_taggable("mkv"));
    }

    #[test]
    fn filename_uses_raw_icy_title_and_is_path_safe() {
        assert_eq!(
            track_filename_base(Some("One More Time"), "Daft Punk - One More Time"),
            "Daft Punk - One More Time"
        );
        // Path separators / reserved chars stripped — no traversal, single component.
        let evil = track_filename_base(None, "../../etc/passwd");
        assert!(!evil.contains('/'));
        assert!(!evil.contains(".."));
        assert_eq!(
            track_filename_base(None, r#"a:b*c?"d<e>f|g"#),
            "a b c d e f g"
        );
        // Empty / dot-only collapses to a placeholder, never "" or ".".
        assert_eq!(track_filename_base(None, "   ...  "), "Untitled");
    }

    #[test]
    fn filename_caps_length() {
        let long = "x".repeat(500);
        assert_eq!(track_filename_base(None, &long).chars().count(), 180);
    }

    #[test]
    fn next_temp_is_unique_and_under_temp_dir() {
        let mut st = RecorderState {
            temp_dir: PathBuf::from("/tmp/recdir"),
            ..Default::default()
        };
        let (id1, p1) = st.next_temp("mp3");
        let (id2, p2) = st.next_temp("mp3");
        assert_ne!(id1, id2);
        assert_ne!(p1, p2);
        assert!(p1.starts_with("/tmp/recdir"));
        assert_eq!(p1.extension().unwrap(), "mp3");
    }
}
