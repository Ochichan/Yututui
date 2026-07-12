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

pub(crate) use crate::util::command_barrier as barrier;
pub mod job;
pub(crate) mod ownership;

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
    /// The reducer emitted a Save effect, but runtime has not yet confirmed the synchronous
    /// journal acceptance boundary. Shutdown must re-issue this state before closing admission.
    SaveRequested,
    /// Save was requested and is crossing or has crossed durable journal acceptance. The final
    /// destination is not yet proven; a deferred worker remains queued for startup recovery.
    SavePending,
    /// Dropped because it was shorter than the minimum duration.
    DiscardedBelowMinDuration,
    /// The user discarded it (or it was cut mid-song by stop/leave).
    DiscardedCancelled,
    /// An Everything-mode Save could not enter the bounded durable spool. The source remains
    /// protected and recording stays paused until this exact Save can be retried.
    AutomaticSaveBlocked,
    /// The exact blocked automatic Save has been re-issued but has not crossed a terminal
    /// Saved/AlreadySettled boundary yet. It remains protected from duplicate/cap eviction.
    AutomaticSaveRetrying,
}

impl RecordingState {
    /// Completed with a kept temp file (eligible for saving).
    pub fn is_recorded(self) -> bool {
        matches!(
            self,
            RecordingState::Recorded
                | RecordingState::RecordedReachedMaxDuration
                | RecordingState::AutomaticSaveBlocked
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
            RecordingState::SaveRequested => {
                t!("Preparing durable save…", "내구성 저장 준비 중…").to_owned()
            }
            RecordingState::SavePending => {
                t!("Saving / queued for recovery…", "저장 중 / 복구 대기 중…").to_owned()
            }
            RecordingState::DiscardedBelowMinDuration => t!("Too short", "너무 짧음").to_owned(),
            RecordingState::DiscardedCancelled => t!("Discarded", "버림").to_owned(),
            RecordingState::AutomaticSaveBlocked => t!(
                "Automatic save blocked — action needed",
                "자동 저장 차단 — 조치 필요"
            )
            .to_owned(),
            RecordingState::AutomaticSaveRetrying => {
                t!("Retrying save…", "저장 재시도 중…").to_owned()
            }
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

#[derive(Clone)]
pub(crate) struct RecorderFinalizePlan {
    pub(crate) reached_max: bool,
    pub(crate) force_incomplete: bool,
    /// Semantic policy captured at the title boundary. Reply latency and a later Settings commit
    /// cannot change duration classification or retroactively promote/demote/destinate a segment.
    pub(crate) duration_secs: u32,
    pub(crate) minimum_duration_secs: u32,
    pub(crate) automatic_final_dir: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub(crate) struct RecorderSaveRequest {
    pub(crate) final_dir: PathBuf,
    pub(crate) automatic: bool,
    pub(crate) bypass_limits: bool,
}

#[derive(Clone)]
pub(crate) struct PlannedOpenSegment {
    pub(crate) id: u64,
    pub(crate) temp_path: PathBuf,
    pub(crate) title: Option<String>,
    pub(crate) artist: Option<String>,
    pub(crate) raw: String,
    pub(crate) station: Option<String>,
    pub(crate) started_at: Instant,
    pub(crate) incomplete: bool,
    pub(crate) ext: &'static str,
}

impl PlannedOpenSegment {
    pub(crate) fn into_open_segment(self) -> OpenSegment {
        OpenSegment {
            id: self.id,
            temp_path: self.temp_path,
            title: self.title,
            artist: self.artist,
            raw: self.raw,
            station: self.station,
            started_at: self.started_at,
            incomplete: self.incomplete,
            ext: self.ext,
        }
    }
}

/// Exact recorder projection held inside [`RecorderState`] until mpv confirms every property
/// phase. Public only because it is carried opaquely through the app's player-commit enum.
#[derive(Clone)]
pub struct RecorderTransitionPlan {
    pub(crate) transition_id: u64,
    pub(crate) expected_current_id: Option<u64>,
    pub(crate) expected_temp_seq: u64,
    pub(crate) expected_saw_first_title: bool,
    pub(crate) finalize: Option<RecorderFinalizePlan>,
    pub(crate) open: Option<PlannedOpenSegment>,
    pub(crate) saw_first_title: bool,
    pub(crate) close_barrier: Option<barrier::CommandBarrier>,
    pub(crate) open_barrier: Option<barrier::CommandBarrier>,
    /// An independent transport failure already retired the actor which owned these commands.
    /// Late negative replies must not trigger a second replacement or install an old-generation
    /// open segment.
    pub(crate) transport_fenced: bool,
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
    /// Original Everything-mode destination captured at the completed title boundary.
    pub(crate) automatic_final_dir: Option<PathBuf>,
    /// An unresolved or failed close proof. Disk work must wait for it and retain `temp_path`
    /// unless exact mpv execution (or a successful replacement open) proved the old writer closed.
    pub(crate) close_barrier: Option<barrier::CommandBarrier>,
    /// Exact policy snapshot for a SaveRequested effect until runtime confirms journal ownership.
    pub(crate) save_request: Option<RecorderSaveRequest>,
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

/// All volatile recorder state. Live segments use a leased per-process namespace beside the
/// legacy startup-wiped temp root, so a second process can reclaim only lock-proven stale owners.
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
    owner: ownership::OwnerNamespace,
    /// True while the bounded durable automatic-save spool has no admission capacity.
    pub capacity_blocked: bool,
    /// Exact automatic source which owns the live capacity latch. `None` represents startup
    /// inventory uncertainty, which cannot be cleared by an unrelated worker event.
    pub(crate) capacity_blocked_id: Option<u64>,
    /// The blocked source itself reached a terminal durable settlement. A later positive spool
    /// scan may then release the latch even if that terminal event reported no capacity yet.
    pub(crate) capacity_owner_settled: bool,
    /// Coalesces fresh spool probes after the owner settled without immediate capacity.
    pub(crate) capacity_probe_pending: bool,
    /// Set after mpv rejects a recorder property command; user/config or transport recovery must
    /// explicitly re-enable recording rather than looping the same failing open every metadata turn.
    pub(crate) execution_blocked: bool,
    /// The one admitted stream-record transition awaiting exact mpv command replies.
    pub(crate) pending_transition: Option<RecorderTransitionPlan>,
    /// A recorder command failure may unblock only after a replacement player is installed.
    pub(crate) restart_unblock_pending: bool,
    /// Fence completed by runtime only after the failed recorder command's mpv process is
    /// synchronously retired. Disk effects may be queued earlier but cannot touch bytes first.
    pub(crate) retirement_barrier: Option<barrier::CommandBarrier>,
    /// True from transport recovery initiation through the replacement readiness callback.
    pub(crate) transport_recovery_active: bool,
    /// Exact blocked automatic Save currently being retried while recording remains paused.
    pub(crate) capacity_retry_id: Option<u64>,
    /// Persistent recorder health/backpressure state. Transient playback actions may mirror this
    /// into the status line but must not erase the underlying recovery warning.
    pub health_warning: Option<String>,
    /// Sticky recovery/durability uncertainty survives live capacity cycles and clears only when
    /// the process restarts and recovery obtains a fresh proof.
    pub(crate) health_sticky: bool,
    /// Orderly teardown gets one bounded attempt to publish the sole unjournaled automatic
    /// source. A synchronous failure must remain visible, never loop another bypass attempt.
    pub(crate) shutdown_bypass_attempted: bool,
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
            owner: ownership::OwnerNamespace::default(),
            capacity_blocked: false,
            capacity_blocked_id: None,
            capacity_owner_settled: false,
            capacity_probe_pending: false,
            execution_blocked: false,
            pending_transition: None,
            restart_unblock_pending: false,
            retirement_barrier: None,
            transport_recovery_active: false,
            capacity_retry_id: None,
            health_warning: None,
            health_sticky: false,
            shutdown_bypass_attempted: false,
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
        let path = self.temp_path(id, ext);
        (id, path)
    }

    pub fn temp_path(&self, id: u64, ext: &str) -> PathBuf {
        self.owner.path(&self.temp_dir, id, ext)
    }

    /// Establish the held process-lifetime lease before a path is handed to mpv.
    pub(crate) fn ensure_owner_active(&self) -> std::io::Result<()> {
        self.owner.ensure_active(&self.temp_dir).map(|_| ())
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
    // Cap by UTF-8 *byte* length, not char count: a 180-char CJK/Hangul title is ~540 bytes,
    // over the 255-byte filename limit on common filesystems, so it would fail to save. 200
    // bytes leaves headroom for the `.<ext>` suffix and any ` (N)` de-duplication suffix.
    const FILENAME_MAX_BYTES: usize = 200;
    let mut capped = String::with_capacity(FILENAME_MAX_BYTES);
    for ch in trimmed.chars() {
        if capped.len() + ch.len_utf8() > FILENAME_MAX_BYTES {
            break;
        }
        capped.push(ch);
    }
    let mut capped = capped
        .trim_matches(|c: char| c == '.' || c == ' ')
        .to_owned();
    if capped.is_empty() {
        "Untitled".to_owned()
    } else {
        if windows_device_name(&capped) {
            while capped.len() > FILENAME_MAX_BYTES - 1 {
                capped.pop();
            }
            capped.insert(0, '_');
        }
        capped
    }
}

fn windows_device_name(name: &str) -> bool {
    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end()
        .to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || stem
            .strip_prefix("COM")
            .or_else(|| stem.strip_prefix("LPT"))
            .is_some_and(|number| {
                matches!(
                    number,
                    "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                )
            })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_track_filename_caps_by_bytes_not_chars() {
        // 300 Hangul syllables = 900 UTF-8 bytes; the char-count cap would have kept ~540.
        let long = "가".repeat(300);
        let out = sanitize_track_filename(&long);
        assert!(out.len() <= 200, "byte length is capped: got {}", out.len());
        assert!(!out.is_empty());
        // The result is still valid UTF-8 (no mid-codepoint truncation) and non-empty.
        assert!(out.chars().all(|c| c == '가'));
        // A normal short ASCII title passes through untouched.
        assert_eq!(sanitize_track_filename("Artist - Title"), "Artist - Title");
    }

    #[test]
    fn sanitize_track_filename_avoids_windows_device_names() {
        assert_eq!(sanitize_track_filename("CON"), "_CON");
        assert_eq!(sanitize_track_filename("con.txt"), "_con.txt");
        assert_eq!(sanitize_track_filename("COM1"), "_COM1");
        assert_eq!(sanitize_track_filename("lpt9.mix"), "_lpt9.mix");
        assert_eq!(sanitize_track_filename("COM¹"), "_COM¹");
        assert_eq!(sanitize_track_filename("lpt².mix"), "_lpt².mix");
        assert_eq!(sanitize_track_filename("CON .txt"), "_CON .txt");
        assert_eq!(sanitize_track_filename("COM10"), "COM10");
    }

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
        let out = track_filename_base(None, &long);
        // Cap is now by UTF-8 byte length (200); ASCII is 1 byte/char, so 200 chars.
        assert!(out.len() <= 200, "byte length capped: {}", out.len());
        assert_eq!(out.chars().count(), 200);
    }

    #[test]
    fn next_temp_is_unique_and_under_stable_owner_root() {
        let mut st = RecorderState {
            temp_dir: PathBuf::from("/tmp/recdir"),
            ..Default::default()
        };
        let (id1, p1) = st.next_temp("mp3");
        let (id2, p2) = st.next_temp("mp3");
        assert_ne!(id1, id2);
        assert_ne!(p1, p2);
        assert!(p1.starts_with(ownership::owners_dir(&st.temp_dir)));
        assert!(!p1.starts_with(&st.temp_dir));
        assert_eq!(p1.extension().unwrap(), "mp3");
    }
}
