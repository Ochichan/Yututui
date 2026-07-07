//! Blocking disk work for the recorder, run off the main loop via `spawn_blocking`
//! (mirrors `Cmd::ScanDownloads`). The reducer decides *what* to do (drop/keep/save); this
//! module only moves bytes: size-stabilize the mpv temp file, copy it into the recordings
//! folder, and best-effort tag it. Failures come back as [`RecorderEvent::SaveFailed`] and
//! never panic the app.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::util::sanitize::sanitize_error_text;

use super::ext_is_taggable;

/// A unit of disk work handed to `spawn_blocking`.
pub enum RecorderJob {
    /// Copy a kept temp recording into the recordings folder and tag it.
    Save {
        /// Correlates the result back to the [`super::RecordedTrack`].
        id: u64,
        temp: PathBuf,
        final_dir: PathBuf,
        /// Sanitized base name (no extension).
        filename: String,
        ext: &'static str,
        title: Option<String>,
        artist: Option<String>,
        station: Option<String>,
    },
    /// Delete a temp recording (too short, discarded, or evicted from history).
    Discard { temp: PathBuf },
    /// Wipe + recreate the temp dir at startup (nothing undecided survives a restart).
    WipeTemp { dir: PathBuf },
}

/// Result of a [`RecorderJob::Save`]. `Discard`/`WipeTemp` report nothing back.
pub enum RecorderEvent {
    Saved { id: u64, final_path: PathBuf },
    SaveFailed { id: u64, error: String },
}

/// Execute a job on the current (blocking) thread. Returns an event only for `Save`.
pub fn run(job: RecorderJob) -> Option<RecorderEvent> {
    match job {
        RecorderJob::Save {
            id,
            temp,
            final_dir,
            filename,
            ext,
            title,
            artist,
            station,
        } => Some(save(
            id, &temp, &final_dir, &filename, ext, title, artist, station,
        )),
        RecorderJob::Discard { temp } => {
            let _ = std::fs::remove_file(temp);
            None
        }
        RecorderJob::WipeTemp { dir } => {
            let _ = std::fs::remove_dir_all(&dir);
            let _ = std::fs::create_dir_all(&dir);
            None
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn save(
    id: u64,
    temp: &Path,
    final_dir: &Path,
    filename: &str,
    ext: &'static str,
    title: Option<String>,
    artist: Option<String>,
    station: Option<String>,
) -> RecorderEvent {
    if let Err(e) = std::fs::create_dir_all(final_dir) {
        return RecorderEvent::SaveFailed {
            id,
            error: sanitize_error_text(e.to_string()),
        };
    }
    // mpv finalizes the container asynchronously after `stream-record` is cleared; wait for
    // the file size to settle so we never copy a half-flushed file.
    wait_for_stable(temp);

    // Never silently overwrite an existing recording (common in Everything mode, or with
    // duplicate/"Untitled" titles): probe for a free `<name> (N).<ext>`.
    let final_path = unique_recording_path(final_dir, filename, ext);
    if let Err(e) = std::fs::copy(temp, &final_path) {
        return RecorderEvent::SaveFailed {
            id,
            error: sanitize_error_text(e.to_string()),
        };
    }
    // Best-effort: a tag-write failure must not fail the save (the audio is already on disk).
    if ext_is_taggable(ext) {
        let _ = tag_file(
            &final_path,
            title.as_deref(),
            artist.as_deref(),
            station.as_deref(),
        );
    }
    RecorderEvent::Saved { id, final_path }
}

/// Pick a destination that won't overwrite an existing recording: `<name>.<ext>`, then
/// `<name> (2).<ext>`, `<name> (3).<ext>`, … Bounded so a pathological directory can't loop
/// forever — in that extreme it falls back to the base name (accepting one overwrite).
fn unique_recording_path(dir: &Path, filename: &str, ext: &str) -> std::path::PathBuf {
    let base = dir.join(format!("{filename}.{ext}"));
    if !base.exists() {
        return base;
    }
    for n in 2..=999 {
        let candidate = dir.join(format!("{filename} ({n}).{ext}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    base
}

/// Poll the temp file's length until it stops growing (stable ~200ms), capped at ~2s.
fn wait_for_stable(path: &Path) {
    let start = Instant::now();
    let mut last = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let mut stable_since = Instant::now();
    loop {
        std::thread::sleep(Duration::from_millis(50));
        let len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(last);
        if len != last {
            last = len;
            stable_since = Instant::now();
        } else if stable_since.elapsed() >= Duration::from_millis(200) {
            return;
        }
        if start.elapsed() >= Duration::from_secs(2) {
            return;
        }
    }
}

/// Stamp title/artist/album onto the copied file. Uses whatever primary tag the container
/// supports (ID3v2 for mp3/aac, VorbisComments for ogg/opus/flac).
fn tag_file(
    path: &Path,
    title: Option<&str>,
    artist: Option<&str>,
    album: Option<&str>,
) -> Result<(), String> {
    use lofty::config::WriteOptions;
    use lofty::file::TaggedFileExt;
    use lofty::probe::Probe;
    use lofty::tag::{Accessor, Tag, TagExt};

    let mut tagged = Probe::open(path)
        .map_err(|e| e.to_string())?
        .read()
        .map_err(|e| e.to_string())?;

    let tag_type = tagged.primary_tag_type();
    if tagged.primary_tag().is_none() {
        tagged.insert_tag(Tag::new(tag_type));
    }
    let tag = tagged
        .primary_tag_mut()
        .ok_or_else(|| "no writable tag".to_owned())?;

    if let Some(t) = title {
        tag.set_title(t.to_owned());
    }
    if let Some(a) = artist {
        tag.set_artist(a.to_owned());
    }
    if let Some(al) = album {
        tag.set_album(al.to_owned());
    }
    tag.save_to_path(path, WriteOptions::default())
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let mut bytes = [0u8; 8];
        getrandom::fill(&mut bytes).unwrap();
        let suffix = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
        std::env::temp_dir().join(format!(
            "yututui-recorder-job-{name}-{}-{suffix}",
            std::process::id()
        ))
    }

    #[test]
    fn wipe_and_discard_jobs_are_best_effort_without_events() {
        let dir = temp_dir("wipe-discard");
        let temp = dir.join("segment.tmp");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&temp, b"partial").unwrap();

        assert!(run(RecorderJob::WipeTemp { dir: dir.clone() }).is_none());
        assert!(dir.exists());
        assert!(!temp.exists());

        std::fs::write(&temp, b"partial").unwrap();
        assert!(run(RecorderJob::Discard { temp: temp.clone() }).is_none());
        assert!(!temp.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn save_job_copies_to_first_free_recording_path() {
        let dir = temp_dir("save");
        let temp = dir.join("tmp").join("rec-1.mkv");
        let final_dir = dir.join("final");
        std::fs::create_dir_all(temp.parent().unwrap()).unwrap();
        std::fs::create_dir_all(&final_dir).unwrap();
        std::fs::write(&temp, b"recording bytes").unwrap();
        std::fs::write(final_dir.join("Song.mkv"), b"existing").unwrap();

        let event = run(RecorderJob::Save {
            id: 7,
            temp: temp.clone(),
            final_dir: final_dir.clone(),
            filename: "Song".to_owned(),
            ext: "mkv",
            title: Some("Title".to_owned()),
            artist: Some("Artist".to_owned()),
            station: Some("Station".to_owned()),
        })
        .expect("save event");

        match event {
            RecorderEvent::Saved { id, final_path } => {
                assert_eq!(id, 7);
                assert_eq!(final_path, final_dir.join("Song (2).mkv"));
                assert_eq!(std::fs::read(final_path).unwrap(), b"recording bytes");
            }
            RecorderEvent::SaveFailed { error, .. } => panic!("unexpected failure: {error}"),
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn save_job_reports_copy_failure_with_sanitized_error() {
        let dir = temp_dir("missing-source");
        let final_dir = dir.join("final");
        let missing = dir.join("tmp").join("missing.mkv");

        let event = run(RecorderJob::Save {
            id: 8,
            temp: missing,
            final_dir,
            filename: "Missing".to_owned(),
            ext: "mkv",
            title: None,
            artist: None,
            station: None,
        })
        .expect("save failure event");

        match event {
            RecorderEvent::SaveFailed { id, error } => {
                assert_eq!(id, 8);
                assert!(!error.is_empty());
                assert!(
                    !error.contains('\n'),
                    "error text should be single-line sanitized"
                );
            }
            RecorderEvent::Saved { final_path, .. } => {
                panic!(
                    "missing source unexpectedly saved to {}",
                    final_path.display()
                )
            }
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn tag_write_failure_does_not_fail_the_audio_save() {
        let dir = temp_dir("tag-failure");
        let temp = dir.join("tmp").join("rec-1.mp3");
        let final_dir = dir.join("final");
        std::fs::create_dir_all(temp.parent().unwrap()).unwrap();
        // Not a valid mp3; `tag_file` should fail internally, but the copied bytes are kept.
        std::fs::write(&temp, b"not actually audio").unwrap();

        let event = run(RecorderJob::Save {
            id: 9,
            temp,
            final_dir: final_dir.clone(),
            filename: "Tagged".to_owned(),
            ext: "mp3",
            title: Some("Title".to_owned()),
            artist: Some("Artist".to_owned()),
            station: Some("Station".to_owned()),
        })
        .expect("save event");

        match event {
            RecorderEvent::Saved { id, final_path } => {
                assert_eq!(id, 9);
                assert_eq!(final_path, final_dir.join("Tagged.mp3"));
                assert_eq!(std::fs::read(final_path).unwrap(), b"not actually audio");
            }
            RecorderEvent::SaveFailed { error, .. } => panic!("tag failure leaked: {error}"),
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn unique_recording_path_advances_past_existing_names() {
        let dir = temp_dir("unique");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Track.flac"), b"1").unwrap();
        std::fs::write(dir.join("Track (2).flac"), b"2").unwrap();

        assert_eq!(
            unique_recording_path(&dir, "Track", "flac"),
            dir.join("Track (3).flac")
        );
        assert_eq!(
            unique_recording_path(&dir, "Other", "flac"),
            dir.join("Other.flac")
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
